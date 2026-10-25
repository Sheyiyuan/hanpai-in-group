//! 生成回复：人物设定 + 近期群聊 + 相关记忆 → 一句话。

use std::sync::Arc;

use anyhow::Result;
use serde_json::json;
use tracing::{debug, info, warn};

use crate::config::GroupPolicy;
use crate::db::{self, MsgRow};
use crate::llm::{Msg, Role};
use crate::memory::{self, RecalledFact};
use crate::state::AppState;
use crate::state::OutboxItem;

#[derive(Debug, Clone)]
pub struct Reply {
    pub text: String,
    pub facts: Vec<RecalledFact>,
    /// 挑中的表情包（可为空）
    pub sticker: Option<db::StickerRow>,
}

pub fn persona_name(cfg: &crate::config::Config) -> String {
    if cfg.persona.name.trim().is_empty() {
        cfg.bot.nickname.trim().to_string()
    } else {
        cfg.persona.name.trim().to_string()
    }
}

/// 被叫到时生成一条回复。返回 None 表示生成失败或内容不可用（不该发）。
pub async fn respond(
    state: &Arc<AppState>,
    group_id: i64,
    sender: &str,
    trigger: &str,
) -> Result<Option<Reply>> {
    let Some(llm) = state.llm() else {
        return Ok(None);
    };
    let cfg = state.cfg();
    let prompts = state.prompts();
    let name = persona_name(&cfg);

    let facts = memory::recall(state, group_id, trigger, cfg.memory.recall_top_k as usize).await?;
    let recent = db::recent_messages(&state.db, group_id, cfg.memory.recent_context as i64).await?;

    let mut transcript = String::new();
    for m in &recent {
        transcript.push_str(&format!("{}: {}\n", m.display(), m.text));
    }
    if transcript.is_empty() {
        transcript.push_str("（暂无）\n");
    }

    let mem_block = if facts.is_empty() {
        "（没有找到相关记忆）".to_string()
    } else {
        facts
            .iter()
            .map(|f| format!("- {}", f.content))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let system = format!(
        "{}\n\n{}",
        crate::persona::system_block(&cfg.persona, &cfg.bot.nickname),
        prompts.reply_for(&name)
    );
    let user = format!(
        "最近的群聊：\n{transcript}\n可能相关的记忆：\n{mem_block}\n\n\
         现在 {sender} 说：{trigger}\n\n直接说出你要回的那句话。"
    );

    let raw = llm
        .chat(Role::Chat, &[Msg::system(system), Msg::user(user)], 0.8)
        .await?;

    let max_chars = cfg.groups.policy(group_id).max_reply_chars;
    match clean(&raw, &name, max_chars, &recent) {
        Some(text) => {
            let sticker = crate::sticker::pick(state, group_id).await;
            Ok(Some(Reply {
                text,
                facts,
                sticker,
            }))
        }
        None => Ok(None),
    }
}

/// 回复前的随机延迟：模拟"看到消息 → 打字 → 发送"。
pub fn pick_delay(min_ms: u64, max_ms: u64, roll: f64) -> u64 {
    if max_ms <= min_ms {
        return min_ms;
    }
    let span = (max_ms - min_ms) as f64;
    min_ms + (span * roll.clamp(0.0, 1.0)) as u64
}

/// 清洗模型输出：去前缀、去引号、压成一行、查复读、截断。
pub fn clean(raw: &str, name: &str, max_chars: usize, recent: &[MsgRow]) -> Option<String> {
    let mut text = raw.trim().to_string();

    for prefix in [
        format!("{name}："),
        format!("{name}:"),
        format!("@{name}"),
        format!("「{name}」"),
    ] {
        if let Some(rest) = text.strip_prefix(&prefix) {
            text = rest.trim().to_string();
        }
    }

    for (left, right) in [("「", "」"), ("“", "”"), ("\"", "\""), ("'", "'")] {
        if text.starts_with(left) && text.ends_with(right) && text.chars().count() > 2 {
            let start = left.len();
            let end = text.len() - right.len();
            if start < end {
                text = text[start..end].trim().to_string();
            }
        }
    }

    let joined = text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    text = joined;

    if text.is_empty() {
        return None;
    }

    if recent
        .iter()
        .any(|m| m.text.trim() == text && !m.text.trim().is_empty())
    {
        debug!("生成内容与最近群消息重复，丢弃: {text}");
        return None;
    }

    if text.chars().count() > max_chars {
        text = text.chars().take(max_chars).collect();
    }
    Some(text)
}

/// 统一投递：dry_run 只记不发，正常则发送、记冷却、写决策。
pub async fn deliver(
    state: &Arc<AppState>,
    policy: &GroupPolicy,
    group_id: i64,
    trigger: &str,
    reason: &str,
    confidence: f32,
    r: Reply,
) -> Option<i64> {
    let facts: Vec<String> = r.facts.iter().map(|f| f.content.clone()).collect();
    let facts_json = serde_json::to_string(&facts).unwrap_or_else(|_| "[]".into());

    if policy.dry_run {
        info!("[dry_run] 群 {group_id} 本想回复：{}", r.text);
        log_decision(
            state,
            group_id,
            trigger,
            "dry_run",
            reason,
            confidence,
            &facts_json,
            None,
        )
        .await;
        return None;
    }

    let item = OutboxItem {
        group_id,
        text: r.text,
        sticker: r.sticker,
        trigger: trigger.to_string(),
        reason: reason.to_string(),
        confidence,
        facts_json,
        queued_at: crate::state::now_ts(),
    };

    // 冷却中不是丢弃，是排队：等冷却结束按序发出
    if state.is_cooling(group_id, policy.cooldown_secs).await {
        let len = enqueue(state, item, policy.outbox_max).await;
        info!(group_id, queue_len = len, "冷却中，回复进入队列");
        return None;
    }

    send_item(state, policy, item).await
}

/// 入队。队列满了丢最早的一条（聊天里最新的话通常更值得回）。
pub async fn enqueue(state: &Arc<AppState>, item: OutboxItem, max: usize) -> usize {
    let max = max.max(1);
    let mut queues = state.outbox.write().await;
    let queue = queues.entry(item.group_id).or_default();
    if queue.len() >= max {
        if let Some(dropped) = queue.pop_front() {
            warn!(
                group_id = item.group_id,
                "发件队列已满，丢弃较早的一条：{}", dropped.text
            );
        }
    }
    queue.push_back(item);
    queue.len()
}

async fn send_item(state: &Arc<AppState>, policy: &GroupPolicy, item: OutboxItem) -> Option<i64> {
    let cfg = state.cfg();

    let mut segments = vec![json!({ "type": "text", "data": { "text": item.text } })];
    if let Some(st) = &item.sticker {
        if let Some(data) = crate::sticker::base64_data(&st.file) {
            segments.push(json!({ "type": "image", "data": { "file": data } }));
        }
    }

    let delay = pick_delay(
        policy.reply_delay_min_ms,
        policy.reply_delay_max_ms,
        crate::sticker::rand01(),
    );
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }

    match state.api.send_group_segments(item.group_id, segments).await {
        Ok(v) => {
            let sent_id = v["data"]["message_id"].as_i64();
            debug!(
                group_id = item.group_id,
                status = v["status"].as_str().unwrap_or(""),
                retcode = v["retcode"].as_i64().unwrap_or(0),
                message_id = sent_id,
                "send_group_msg 回包"
            );
            if sent_id.is_none() {
                warn!(
                    group_id = item.group_id,
                    resp = %v,
                    "发送返回成功但没有 message_id，消息可能没有真正送达"
                );
            }
            state.mark_spoke(item.group_id).await;

            // 自己的发言也写回 messages：后续上下文能看见自己说过什么，
            // 8-gram 查重也能挡住「复读自己上一句」，同时让引用回复可归因。
            if let Some(id) = sent_id {
                remember_sent(state, &cfg, item.group_id, id, &item.text).await;
            }

            let waited = crate::state::now_ts() - item.queued_at;
            let reason = if waited > 0 {
                format!("{}（排队 {waited}s）", item.reason)
            } else {
                item.reason.clone()
            };
            log_decision(
                state,
                item.group_id,
                &item.trigger,
                "reply",
                &reason,
                item.confidence,
                &item.facts_json,
                sent_id,
            )
            .await;
            info!(
                group_id = item.group_id,
                waited_secs = waited,
                "已回复：{}{}",
                item.text,
                if item.sticker.is_some() {
                    " [+表情]"
                } else {
                    ""
                }
            );
            sent_id
        }
        Err(e) => {
            warn!("发送回复失败: {e:#}");
            None
        }
    }
}

/// 每 250ms 扫一次队列：不在冷却、且有积压，就发一条。
pub async fn outbox_ticker(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tick.tick().await;
        let groups: Vec<i64> = {
            let queues = state.outbox.read().await;
            queues
                .iter()
                .filter(|(_, q)| !q.is_empty())
                .map(|(gid, _)| *gid)
                .collect()
        };

        for group_id in groups {
            let cfg = state.cfg();
            let policy = cfg.groups.policy(group_id);
            if state.is_cooling(group_id, policy.cooldown_secs).await {
                continue;
            }

            let item = {
                let mut queues = state.outbox.write().await;
                queues.get_mut(&group_id).and_then(|q| q.pop_front())
            };
            if let Some(item) = item {
                send_item(&state, &policy, item).await;
                let mut queues = state.outbox.write().await;
                if queues.get(&group_id).is_some_and(|q| q.is_empty()) {
                    queues.remove(&group_id);
                }
            }
        }
    }
}

/// 把机器人自己发出的一条消息写回 messages（user_id = 自己）。
async fn remember_sent(
    state: &Arc<AppState>,
    cfg: &crate::config::Config,
    group_id: i64,
    msg_id: i64,
    text: &str,
) {
    let self_id = state.self_qq(cfg);
    if self_id <= 0 {
        return;
    }

    let name = persona_name(cfg);
    let segments = json!([{ "type": "text", "data": { "text": text } }]).to_string();
    let msg = db::NewMessage {
        msg_id,
        group_id,
        user_id: self_id,
        nickname: &name,
        card: "",
        ts: crate::state::now_ts(),
        text,
        segments: &segments,
        reply_to: None,
        addressed: false,
    };
    if let Err(e) = db::insert_message(&state.db, &msg, cfg.onebot.dedup_window_secs).await {
        warn!("记录自身发言失败: {e:#}");
    }
}

#[allow(clippy::too_many_arguments)]
async fn log_decision(
    state: &Arc<AppState>,
    group_id: i64,
    trigger: &str,
    action: &str,
    reason: &str,
    confidence: f32,
    facts_used: &str,
    sent_msg_id: Option<i64>,
) {
    let row = db::NewDecision {
        group_id,
        ts: crate::state::now_ts(),
        trigger,
        action,
        reason,
        confidence,
        facts_used,
        sent_msg_id,
    };
    if let Err(e) = db::insert_decision(&state.db, &row).await {
        warn!("写决策日志失败: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(text: &str) -> MsgRow {
        MsgRow {
            id: 1,
            user_id: 1,
            nickname: "a".into(),
            card: "".into(),
            text: text.into(),
            ts: 0,
        }
    }

    #[test]
    fn strips_prefix_and_quotes() {
        let out = clean("半拍：「成都挺好玩的」", "半拍", 60, &[]).unwrap();
        assert_eq!(out, "成都挺好玩的");
    }

    #[test]
    fn collapses_multiline() {
        let out = clean("第一行\n\n第二行", "半拍", 60, &[]).unwrap();
        assert_eq!(out, "第一行 第二行");
    }

    #[test]
    fn rejects_empty() {
        assert!(clean("   \n ", "半拍", 60, &[]).is_none());
    }

    #[test]
    fn rejects_repeat_of_recent() {
        let recent = vec![msg("确实")];
        assert!(clean("确实", "半拍", 60, &recent).is_none());
    }

    #[test]
    fn delay_within_range() {
        assert_eq!(pick_delay(500, 1500, 0.0), 500);
        assert_eq!(pick_delay(500, 1500, 1.0), 1500);
        assert_eq!(pick_delay(500, 1500, 0.5), 1000);
        assert_eq!(pick_delay(0, 0, 0.9), 0);
        assert_eq!(pick_delay(800, 800, 0.3), 800);
    }

    #[test]
    fn truncates_by_chars() {
        let out = clean(&"啊".repeat(100), "半拍", 10, &[]).unwrap();
        assert_eq!(out.chars().count(), 10);
    }
}
