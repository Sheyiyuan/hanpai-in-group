//! 发言决策：批量聚合 → Planner 判断 → 触发函数采样 → 生成/放弃。
//!
//! 分工：
//! - Planner（小模型）只回答「有没有必要接这个话茬」，给出 `interest`
//! - 触发函数把 `interest` 和「憋了多久」合成概率，再掷一次骰子
//!
//! 这样「想说」和「真说」是两件事：前者由模型判断，后者由可调的物理参数控制。

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::config::{Config, GroupPolicy};
use crate::db;
use crate::llm::{truncate, Msg, Role};
use crate::persona;
use crate::reply;
use crate::state::{now_ms as epoch_ms, now_ts, AppState};

/// 一条消息进来后，攒多久再判断：群里安静就快判，正热闹就多等一会儿。
pub fn debounce_window(has_pending: bool, gap_ms: u64, quiet_ms: u64, active_ms: u64) -> u64 {
    if !has_pending || gap_ms > 30_000 {
        quiet_ms
    } else {
        active_ms
    }
}

#[derive(Debug, Clone)]
pub struct BatchItem {
    pub msg_id: i64,
    pub user_id: i64,
    pub name: String,
    pub text: String,
    pub ts: i64,
}

#[derive(Debug, Default, Clone)]
pub struct Batch {
    pub messages: Vec<BatchItem>,
    pub first_ms: u64,
    pub last_ms: u64,
    pub due_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Plan {
    #[serde(default = "default_action")]
    pub action: String,
    #[serde(default = "default_half")]
    pub interest: f64,
    #[serde(default = "default_half")]
    pub confidence: f64,
    #[serde(default)]
    pub reason: String,
}

fn default_action() -> String {
    "wait".to_string()
}
fn default_half() -> f64 {
    0.5
}

impl Plan {
    pub fn wants_reply(&self) -> bool {
        self.action.eq_ignore_ascii_case("reply")
    }
}

pub fn parse_plan(value: &serde_json::Value) -> Plan {
    serde_json::from_value(value.clone()).unwrap_or_else(|_| Plan {
        action: "wait".into(),
        interest: 0.0,
        confidence: 0.0,
        reason: "解析失败".into(),
    })
}

// ---------------------------------------------------------------------------
// 触发函数
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub struct TriggerParams {
    pub talk_value: f64,
    pub idle_half_life_secs: f64,
    pub idle_weight: f64,
    pub interest_gamma: f64,
    pub min_probability: f64,
}

impl TriggerParams {
    pub fn from_config(cfg: &Config, group_id: i64) -> Self {
        let policy = cfg.groups.policy(group_id);
        Self {
            talk_value: policy.talk_value.clamp(0.0, 1.0),
            idle_half_life_secs: cfg.trigger.idle_half_life_secs.max(1.0),
            idle_weight: cfg.trigger.idle_weight.clamp(0.0, 1.0),
            interest_gamma: cfg.trigger.interest_gamma.clamp(0.01, 5.0),
            min_probability: cfg.trigger.min_probability.clamp(0.0, 1.0),
        }
    }
}

/// p = talk_value × (兴趣^gamma + (1-兴趣^gamma) × 沉默加权 × idle_weight)
pub fn trigger_probability(p: &TriggerParams, interest: f64, idle_secs: f64) -> f64 {
    let interest = interest.clamp(0.0, 1.0);
    let idle = idle_secs.max(0.0);

    let base = interest.powf(p.interest_gamma);
    let idle_boost = 1.0 - 0.5_f64.powf(idle / p.idle_half_life_secs.max(1.0));

    let raw = p.talk_value * (base + (1.0 - base) * idle_boost * p.idle_weight);
    raw.clamp(p.min_probability.min(1.0), 1.0)
}

pub fn should_trigger(p: &TriggerParams, interest: f64, idle_secs: f64, roll: f64) -> bool {
    roll < trigger_probability(p, interest, idle_secs)
}

/// 硬上限检查。`max_per_*` 为负表示不限。
pub fn within_limits(policy: &GroupPolicy, sent_last_hour: i64, sent_today: i64) -> bool {
    let by_hour = policy.max_per_hour < 0 || sent_last_hour < policy.max_per_hour;
    let by_day = policy.max_per_day < 0 || sent_today < policy.max_per_day;
    by_hour && by_day
}

/// 没有 Planner 时的兜底兴趣估计（当前只在测试/降级路径用）。
pub fn heuristic_interest(
    addressed: bool,
    is_question: bool,
    keyword_hits: u32,
    relevance: f64,
) -> f64 {
    let mut s = 0.10;
    if addressed {
        s += 0.75;
    }
    if is_question {
        s += 0.10;
    }
    s += keyword_hits.min(3) as f64 * 0.05;
    s += relevance.clamp(0.0, 1.0) * 0.25;
    s.clamp(0.0, 1.0)
}

// ---------------------------------------------------------------------------
// 批量聚合
// ---------------------------------------------------------------------------

pub async fn on_message(
    state: &Arc<AppState>,
    group_id: i64,
    item: BatchItem,
    quiet_ms: u64,
    active_ms: u64,
) {
    let now = now_ms();
    let mut batches = state.batches.write().await;
    let batch = batches.entry(group_id).or_default();

    let gap = if batch.messages.is_empty() {
        0
    } else {
        now.saturating_sub(batch.last_ms)
    };
    let window = debounce_window(!batch.messages.is_empty(), gap, quiet_ms, active_ms);

    if batch.messages.is_empty() {
        batch.first_ms = now;
    }
    batch.messages.push(item);
    batch.last_ms = now;
    batch.due_ms = now + window;

    // 只留最近的 20 条，防止长时间沉默后攒出巨长上下文
    if batch.messages.len() > 20 {
        let excess = batch.messages.len() - 20;
        batch.messages.drain(0..excess);
    }
}

pub async fn ticker(state: Arc<AppState>) {
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;

    loop {
        tick.tick().await;
        let now = now_ms();
        let due: Vec<i64> = {
            let batches = state.batches.read().await;
            batches
                .iter()
                .filter(|(_, b)| b.due_ms <= now && !b.messages.is_empty())
                .map(|(gid, _)| *gid)
                .collect()
        };

        for group_id in due {
            {
                let mut evaluating = state.evaluating.lock().await;
                if !evaluating.insert(group_id) {
                    continue;
                }
            }

            let batch = state.batches.write().await.remove(&group_id);
            let Some(batch) = batch else {
                state.evaluating.lock().await.remove(&group_id);
                continue;
            };

            let st = state.clone();
            tokio::spawn(async move {
                if let Err(e) = evaluate(&st, group_id, batch).await {
                    warn!("群 {group_id} 决策失败: {e:#}");
                }
                st.evaluating.lock().await.remove(&group_id);
            });
        }
    }
}

// ---------------------------------------------------------------------------
// 单次决策
// ---------------------------------------------------------------------------

async fn evaluate(state: &Arc<AppState>, group_id: i64, batch: Batch) -> Result<()> {
    let Some(_llm) = state.llm() else {
        return Ok(());
    };
    let cfg = state.cfg();
    if !cfg.groups.is_allowed(group_id) || state.is_muted(group_id).await {
        return Ok(());
    }

    let policy = cfg.groups.policy(group_id);
    if policy.talk_value <= 0.0 {
        debug!("群 {group_id} talk_value=0，不参与发言");
        return Ok(());
    }

    let hour_ago = now_ts() - 3600;
    let day_ago = now_ts() - 86_400;
    let sent_hour = db::sent_count_since(&state.db, group_id, hour_ago).await?;
    let sent_day = db::sent_count_since(&state.db, group_id, day_ago).await?;
    if !within_limits(&policy, sent_hour, sent_day) {
        log_decision(state, group_id, "limit", "达到发言上限", 0.0, "[]", None).await;
        return Ok(());
    }

    let facts = crate::memory::recall(
        state,
        group_id,
        &batch
            .messages
            .last()
            .map(|m| m.text.clone())
            .unwrap_or_default(),
        cfg.planner.recall_top_k as usize,
    )
    .await?;

    let plan = match ask_planner(state, &cfg, group_id, &batch, &facts).await {
        Ok(p) => p,
        Err(e) => {
            warn!("Planner 调用失败: {e:#}");
            log_decision(state, group_id, "error", &format!("{e:#}"), 0.0, "[]", None).await;
            return Ok(());
        }
    };

    debug!(
        group_id,
        interest = plan.interest,
        confidence = plan.confidence,
        reason = %plan.reason,
        "Planner 判断"
    );

    if !plan.wants_reply() || plan.confidence < cfg.planner.min_confidence {
        log_decision(
            state,
            group_id,
            "wait",
            &format!(
                "interest={:.2} conf={:.2} {}",
                plan.interest, plan.confidence, plan.reason
            ),
            plan.confidence as f32,
            "[]",
            None,
        )
        .await;
        return Ok(());
    }

    let idle = match state.last_spoke.read().await.get(&group_id).copied() {
        Some(ts) => ((epoch_ms() - ts).max(0) / 1000) as f64,
        // 从没说过话，按憋了很久算
        None => 3600.0,
    };
    let params = TriggerParams::from_config(&cfg, group_id);
    let probability = trigger_probability(&params, plan.interest, idle);
    let roll = pseudo_roll();

    debug!(
        group_id,
        interest = plan.interest,
        probability,
        roll,
        idle_secs = idle,
        sent_last_hour = sent_hour,
        sent_today = sent_day,
        passed = roll < probability,
        "发言采样"
    );

    if roll >= probability {
        log_decision(
            state,
            group_id,
            "skip",
            &format!(
                "概率未过 p={probability:.2} roll={roll:.2} interest={:.2} idle={idle:.0}s",
                plan.interest
            ),
            plan.confidence as f32,
            "[]",
            None,
        )
        .await;
        return Ok(());
    }

    info!(
        group_id,
        interest = plan.interest,
        probability,
        roll,
        idle_secs = idle,
        "决定开口"
    );

    match reply::respond(state, group_id, "群友", &batch_summary(&batch)).await {
        Ok(Some(r)) => {
            reply::deliver(
                state,
                &policy,
                group_id,
                "batch",
                &format!(
                    "Planner 想接（p={probability:.2} interest={:.2}）",
                    plan.interest
                ),
                plan.confidence as f32,
                r,
            )
            .await;
        }
        Ok(None) => {
            log_decision(
                state,
                group_id,
                "skip",
                "生成内容不可用",
                plan.confidence as f32,
                "[]",
                None,
            )
            .await;
        }
        Err(e) => {
            warn!("生成回复失败: {e:#}");
            log_decision(state, group_id, "error", &format!("{e:#}"), 0.0, "[]", None).await;
        }
    }

    Ok(())
}

async fn ask_planner(
    state: &Arc<AppState>,
    cfg: &Config,
    group_id: i64,
    batch: &Batch,
    facts: &[crate::memory::RecalledFact],
) -> Result<Plan> {
    let Some(llm) = state.llm() else {
        return Ok(Plan {
            action: "wait".into(),
            interest: 0.0,
            confidence: 0.0,
            reason: "没有模型".into(),
        });
    };
    let prompts = state.prompts();
    let name = reply::persona_name(cfg);

    let recent = db::recent_messages(&state.db, group_id, 20).await?;
    let mut transcript = String::new();
    for m in &recent {
        transcript.push_str(&format!("{}: {}\n", m.display(), m.text));
    }

    let mut batch_text = String::new();
    for m in batch.messages.iter().take(cfg.planner.max_batch as usize) {
        batch_text.push_str(&format!("{}: {}\n", m.name, m.text));
    }

    let mem_block = if facts.is_empty() {
        "（没有相关记忆）".to_string()
    } else {
        facts
            .iter()
            .map(|f| format!("- {}", f.content))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let last_spoke = state.last_spoke.read().await.get(&group_id).copied();
    let idle_text = match last_spoke {
        Some(ts) => format!("{} 分钟前", (epoch_ms() - ts).max(0) / 60_000),
        None => "还没在群里说过话".to_string(),
    };

    let system = format!(
        "{}\n\n{}",
        persona::system_block(&cfg.persona, &cfg.bot.nickname),
        prompts.planner_for(&name)
    );
    let user = format!(
        "最近的群聊：\n{transcript}\n现在这批新消息：\n{batch_text}\n\
         你可能记得的事：\n{mem_block}\n\n你上次发言是：{idle_text}。"
    );

    let value = llm
        .chat_json(Role::Planner, &[Msg::system(system), Msg::user(user)])
        .await
        .context("Planner 请求失败")?;
    Ok(parse_plan(&value))
}

fn batch_summary(batch: &Batch) -> String {
    batch
        .messages
        .iter()
        .map(|m| format!("{}: {}", m.name, m.text))
        .collect::<Vec<_>>()
        .join("\n")
}

async fn log_decision(
    state: &Arc<AppState>,
    group_id: i64,
    action: &str,
    reason: &str,
    confidence: f32,
    facts_used: &str,
    sent_msg_id: Option<i64>,
) {
    let row = db::NewDecision {
        group_id,
        ts: now_ts(),
        trigger: "batch",
        action,
        reason: &truncate(reason, 200),
        confidence,
        facts_used,
        sent_msg_id,
    };
    if let Err(e) = db::insert_decision(&state.db, &row).await {
        warn!("写决策日志失败: {e:#}");
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn pseudo_roll() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut x = nanos ^ 0x9E37_79B9_7F4A_7C15;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    (x % 10_000) as f64 / 10_000.0
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn params(talk: f64) -> TriggerParams {
        TriggerParams {
            talk_value: talk,
            idle_half_life_secs: 600.0,
            idle_weight: 0.35,
            interest_gamma: 0.8,
            min_probability: 0.0,
        }
    }

    #[test]
    fn bounded() {
        let p = params(1.0);
        for i in [0.0, 0.01, 0.5, 0.99, 1.0] {
            for idle in [0.0, 1.0, 600.0, 1e9] {
                let v = trigger_probability(&p, i, idle);
                assert!((0.0..=1.0).contains(&v), "i={i} idle={idle} v={v}");
            }
        }
    }

    #[test]
    fn monotonic_in_interest() {
        let p = params(0.8);
        let mut last = -1.0;
        for i in 0..=20 {
            let interest = i as f64 / 20.0;
            let v = trigger_probability(&p, interest, 120.0);
            assert!(
                v >= last,
                "兴趣上升概率不应下降: {interest} => {v} < {last}"
            );
            last = v;
        }
    }

    #[test]
    fn monotonic_in_silence() {
        let p = params(0.8);
        let mut last = -1.0;
        for idle in [0.0, 5.0, 60.0, 300.0, 600.0, 1800.0, 3600.0, 86400.0] {
            let v = trigger_probability(&p, 0.2, idle);
            assert!(v >= last, "沉默越久概率不应下降: {idle}s => {v} < {last}");
            last = v;
        }
    }

    #[test]
    fn interest_ceiling_equals_talk_value() {
        let p = params(0.7);
        let v = trigger_probability(&p, 1.0, 0.0);
        assert!((v - 0.7).abs() < 1e-9, "{v}");
    }

    #[test]
    fn silence_floor_is_weighted() {
        let p = params(1.0);
        let v = trigger_probability(&p, 0.0, 1e9);
        assert!((v - 0.35).abs() < 1e-6, "{v}");
    }

    #[test]
    fn zero_talk_value_is_silent() {
        let p = params(0.0);
        assert_eq!(trigger_probability(&p, 1.0, 1e9), 0.0);
    }

    #[test]
    fn min_probability_floor() {
        let mut p = params(1.0);
        p.min_probability = 0.02;
        assert!((trigger_probability(&p, 0.0, 0.0) - 0.02).abs() < 1e-9);
    }

    #[test]
    fn unlimited_limits() {
        let mut policy = crate::config::Config::default().groups.policy(1);
        policy.max_per_hour = -1;
        policy.max_per_day = -1;
        assert!(within_limits(&policy, 10_000, 1_000_000));
    }

    #[test]
    fn hard_limits_still_work() {
        let mut policy = crate::config::Config::default().groups.policy(1);
        policy.max_per_hour = 3;
        assert!(within_limits(&policy, 2, 0));
        assert!(!within_limits(&policy, 3, 0));
    }

    #[test]
    fn addressed_raises_interest() {
        let quiet = heuristic_interest(false, false, 0, 0.0);
        let called = heuristic_interest(true, false, 0, 0.0);
        assert!(called > quiet);
    }

    #[test]
    fn debounce_prefers_quiet_when_idle() {
        assert_eq!(debounce_window(false, 0, 2000, 5000), 2000);
        assert_eq!(debounce_window(true, 40_000, 2000, 5000), 2000);
        assert_eq!(debounce_window(true, 500, 2000, 5000), 5000);
    }

    #[test]
    fn plan_parsing() {
        let v = serde_json::json!({"action":"reply","interest":0.8,"confidence":0.7,"reason":"聊到猫了"});
        let p = parse_plan(&v);
        assert!(p.wants_reply());
        assert!((p.interest - 0.8).abs() < 1e-9);

        let bad = parse_plan(&serde_json::json!({"action": 42}));
        assert!(!bad.wants_reply());
        assert_eq!(bad.confidence, 0.0);
        assert_eq!(bad.reason, "解析失败");

        let empty = parse_plan(&serde_json::json!({}));
        assert!(!empty.wants_reply());
    }

    #[test]
    fn roll_is_in_range() {
        for _ in 0..100 {
            let r = pseudo_roll();
            assert!((0.0..1.0).contains(&r));
        }
    }
}
