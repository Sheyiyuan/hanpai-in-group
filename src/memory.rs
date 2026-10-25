//! 长期记忆：抽取和检索。
//!
//! 抽取是批量的、异步的：水位（watermark）记录已处理到哪条消息，攒够一批或到点就抽。
//! 检索是三路融合：向量相似度 + 关键词命中 + 重要度/时间衰减。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::{debug, info, warn};

use crate::db;
use crate::embed::{blob_to_vec, cosine, vec_to_blob};
use crate::llm::{Msg, Role};
use crate::state::{now_ts, AppState};

/// 一批最多抽多少条事实，防止模型跑飞
const MAX_FACTS_PER_BATCH: usize = 20;
/// 送进抽取提示词的最大字符数
const MAX_TRANSCRIPT_CHARS: usize = 6000;

#[derive(Debug, Deserialize)]
struct FactsPayload {
    #[serde(default)]
    facts: Vec<RawFact>,
}

#[derive(Debug, Deserialize)]
struct RawFact {
    content: String,
    #[serde(default)]
    subject_qq: i64,
    #[serde(default = "default_importance")]
    importance: f32,
    #[serde(default = "default_confidence")]
    confidence: f32,
}

fn default_importance() -> f32 {
    3.0
}
fn default_confidence() -> f32 {
    0.7
}

#[derive(Debug, Clone, Serialize)]
pub struct RecalledFact {
    pub id: i64,
    pub content: String,
    pub score: f32,
}

/// 抽取一个群的记忆。返回新插入的事实条数。
pub async fn extract_group(state: &Arc<AppState>, group_id: i64) -> Result<usize> {
    let Some(llm) = state.llm() else {
        return Ok(0);
    };
    let cfg = state.cfg();
    let _guard = state.extract_lock.lock().await;

    let wm_key = format!("extract_wm:{group_id}");
    let watermark = db::meta_get(&state.db, &wm_key)
        .await?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    let msgs = db::messages_after(
        &state.db,
        group_id,
        watermark,
        cfg.memory.extract_batch_size as i64,
    )
    .await?;
    if msgs.is_empty() {
        return Ok(0);
    }

    // 机器人自己的发言也落库（供上下文/查重/引用归因），但不该被当成“群友说过的事实”抽取
    let self_id = state.self_qq(&cfg);
    let last_id = msgs.last().map(|m| m.id).unwrap_or(watermark);

    let mut transcript = String::new();
    let mut known_users: HashSet<i64> = HashSet::new();
    for m in msgs.iter().filter(|m| self_id <= 0 || m.user_id != self_id) {
        known_users.insert(m.user_id);
        transcript.push_str(&format!(
            "[{}] {}({}): {}\n",
            m.id,
            m.display(),
            m.user_id,
            m.text
        ));
    }
    if transcript.trim().is_empty() {
        // 整批都是自己说的：只推进水位，不浪费一次模型调用
        db::meta_set(&state.db, &wm_key, &last_id.to_string()).await?;
        return Ok(0);
    }
    let transcript = truncate_tail(&transcript, MAX_TRANSCRIPT_CHARS);

    let prompts = state.prompts();
    let messages = vec![
        Msg::system(prompts.extract.clone()),
        Msg::user(format!("群聊记录：\n{transcript}")),
    ];
    let value = llm
        .chat_json(Role::Summary, &messages)
        .await
        .context("记忆抽取模型调用失败")?;

    let payload: FactsPayload = serde_json::from_value(value)
        .context("记忆抽取返回的 JSON 结构不对（期望 {\"facts\":[...]}）")?;

    // 清洗 + 上限
    let mut candidates: Vec<(String, i64, f32, f32)> = Vec::new();
    for f in payload.facts.into_iter().take(MAX_FACTS_PER_BATCH) {
        let content = f.content.trim().to_string();
        if content.chars().count() < 4 {
            continue;
        }
        let subject = if known_users.contains(&f.subject_qq) {
            f.subject_qq
        } else {
            0
        };
        candidates.push((
            content,
            subject,
            f.importance.clamp(0.0, 5.0),
            f.confidence.clamp(0.0, 1.0),
        ));
    }

    // 向量：一次请求拿全，避免 N 次往返
    let embedder = state.embedder();
    let embeddings: Vec<Option<Vec<f32>>> = if embedder.available() && !candidates.is_empty() {
        let texts: Vec<String> = candidates.iter().map(|c| c.0.clone()).collect();
        match embedder.embed(&texts).await {
            Ok(vecs) => vecs.into_iter().map(Some).collect(),
            Err(e) => {
                warn!("批量取向量失败，本批只存文本: {e:#}");
                vec![None; candidates.len()]
            }
        }
    } else {
        vec![None; candidates.len()]
    };

    // 已有的向量，用于去重
    let mut existing: Vec<Vec<f32>> = if embedder.available() {
        db::fact_vecs_for_group(&state.db, group_id)
            .await?
            .into_iter()
            .map(|(_, blob)| blob_to_vec(&blob))
            .collect()
    } else {
        Vec::new()
    };
    let threshold = cfg.memory.vector_threshold as f32;
    let now = now_ts();

    let mut inserted = 0usize;
    for (i, (content, subject, importance, confidence)) in candidates.into_iter().enumerate() {
        let vec = embeddings.get(i).cloned().flatten();

        if let Some(v) = &vec {
            if existing.iter().any(|e| cosine(v, e) >= threshold) {
                debug!("事实与已有记忆过于相似，跳过: {content}");
                continue;
            }
        }

        let base_score = importance.round() as i64;
        let fact_id = db::insert_fact(
            &state.db,
            &db::NewFact {
                group_id,
                subject_qq: subject,
                scope: if subject == 0 { "group" } else { "user" },
                content: &content,
                confidence,
                importance,
                base_score,
                source_msg_ids: &json!([last_id]).to_string(),
                created_at: now,
            },
        )
        .await?;

        if let Some(v) = vec {
            db::set_fact_vec(&state.db, fact_id, v.len() as i64, &vec_to_blob(&v)).await?;
            existing.push(v);
        }
        inserted += 1;
    }

    db::meta_set(&state.db, &wm_key, &last_id.to_string()).await?;
    if inserted > 0 {
        info!("群 {group_id} 记忆抽取完成：新增 {inserted} 条（处理到消息 {last_id}）");
    } else {
        debug!("群 {group_id} 无新记忆（处理到消息 {last_id}）");
    }
    Ok(inserted)
}

/// 后台定时器：每分钟检查一次，攒够一批或到时间就抽。
pub async fn ticker(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tick.tick().await;
        let cfg = state.cfg();
        for group_id in cfg.groups.whitelist.clone() {
            if let Err(e) = maybe_extract(&state, group_id).await {
                warn!("群 {group_id} 记忆抽取失败: {e:#}");
            }
        }
    }
}

async fn maybe_extract(state: &Arc<AppState>, group_id: i64) -> Result<()> {
    if state.llm().is_none() {
        return Ok(());
    }
    let cfg = state.cfg();
    let wm_key = format!("extract_wm:{group_id}");
    let watermark = db::meta_get(&state.db, &wm_key)
        .await?
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(0);

    let count = db::count_messages_after(&state.db, group_id, watermark).await?;
    if count == 0 {
        return Ok(());
    }

    let oldest = db::oldest_ts_after(&state.db, group_id, watermark).await?;
    let aged = oldest
        .map(|ts| now_ts() - ts >= cfg.memory.extract_interval_mins as i64 * 60)
        .unwrap_or(false);
    let batched = count >= cfg.memory.extract_batch_size as i64;

    if aged || batched {
        extract_group(state, group_id).await?;
    }
    Ok(())
}

/// 三路融合检索。
pub async fn recall(
    state: &Arc<AppState>,
    group_id: i64,
    query: &str,
    limit: usize,
) -> Result<Vec<RecalledFact>> {
    let facts = db::facts_for_group(&state.db, group_id, 2000).await?;
    if facts.is_empty() {
        return Ok(Vec::new());
    }

    let embedder = state.embedder();
    let query_vec = if embedder.available() {
        match embedder.embed(&[query.to_string()]).await {
            Ok(mut v) => v.pop(),
            Err(e) => {
                warn!("查询向量化失败，本次仅用关键词: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let mut vecs: HashMap<i64, Vec<f32>> = HashMap::new();
    if query_vec.is_some() {
        for (id, blob) in db::fact_vecs_for_group(&state.db, group_id).await? {
            vecs.insert(id, blob_to_vec(&blob));
        }
    }

    let tokens = tokenize(query);
    let now = now_ts();

    let mut scored: Vec<RecalledFact> = facts
        .into_iter()
        .map(|f| {
            let cos = query_vec
                .as_ref()
                .and_then(|q| vecs.get(&f.id).map(|v| cosine(q, v)))
                .unwrap_or(0.0);
            let kw = keyword_score(&f.content, query, &tokens);
            let age_days = ((now - f.created_at).max(0) as f32) / 86_400.0;
            let decay = 0.5f32.powf(age_days / 30.0);
            let imp = (f.importance / 5.0).clamp(0.0, 1.0) * decay;

            let score = if query_vec.is_some() {
                0.55 * cos + 0.25 * kw + 0.20 * imp
            } else {
                0.60 * kw + 0.40 * imp
            };

            RecalledFact {
                id: f.id,
                content: f.content,
                score,
            }
        })
        .filter(|f| f.score > 0.05)
        .collect();

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(limit);
    Ok(scored)
}

fn keyword_score(content: &str, query: &str, tokens: &[String]) -> f32 {
    let q = query.trim();
    // 整句原样出现，最强信号
    if q.chars().count() >= 2 && content.contains(q) {
        return 1.0;
    }
    if tokens.is_empty() {
        return 0.0;
    }
    let hits = tokens
        .iter()
        .filter(|t| content.contains(t.as_str()))
        .count();
    let raw = hits as f32 / tokens.len() as f32;
    // 开方抬一下部分命中的分，否则长句永远匹配不到
    raw.sqrt().min(1.0)
}

/// 中文没有词边界，用「整段 + 二元组」近似分词。
/// 「你还记得老王说的成都吗」会拆出 老王 / 成都 这样的有效信号。
fn tokenize(query: &str) -> Vec<String> {
    let mut out = Vec::new();
    for chunk in query.split(|c: char| !c.is_alphanumeric()) {
        let chars: Vec<char> = chunk.chars().collect();
        if chars.len() < 2 {
            continue;
        }
        out.push(chunk.to_string());
        if chars.len() > 4 {
            for w in chars.windows(2) {
                out.push(w.iter().collect());
            }
        }
    }
    out.sort();
    out.dedup();
    out.truncate(40);
    out
}

fn truncate_tail(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    let skip = count - max_chars;
    s.chars().skip(skip).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenize_splits_cjk_bigrams() {
        let t = tokenize("半拍，你还记得老王说的成都吗？");
        assert!(t.contains(&"半拍".to_string()));
        assert!(t.contains(&"老王".to_string()));
        assert!(t.contains(&"成都".to_string()));
        assert!(t.iter().all(|s| s.chars().count() >= 2));
    }

    #[test]
    fn keyword_scoring() {
        let tokens = vec!["成都".to_string(), "出差".to_string()];
        assert_eq!(
            keyword_score("老王下周去成都出差", "随便问问", &tokens),
            1.0
        );
        assert!((keyword_score("老王下周去成都", "随便问问", &tokens) - 0.707).abs() < 0.01);
        assert_eq!(keyword_score("今天天气不错", "随便问问", &tokens), 0.0);
    }

    #[test]
    fn keyword_exact_query_wins() {
        assert_eq!(
            keyword_score("老王去成都出差", "老王去成都出差", &["成都".to_string()]),
            1.0
        );
    }

    #[test]
    fn tail_truncation_keeps_latest() {
        let s = "1234567890";
        assert_eq!(truncate_tail(s, 4), "7890");
        assert_eq!(truncate_tail("abc", 10), "abc");
    }

    #[test]
    fn parse_payload_tolerates_missing_fields() {
        let v: FactsPayload =
            serde_json::from_str(r#"{"facts":[{"content":"老王养了猫"}]}"#).unwrap();
        assert_eq!(v.facts[0].importance, 3.0);
        assert_eq!(v.facts[0].confidence, 0.7);
        assert_eq!(v.facts[0].subject_qq, 0);
    }
}
