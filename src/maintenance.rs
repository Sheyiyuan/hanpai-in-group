//! 后台维护：token 统计落库 + 过期消息清理。
//!
//! - 每 60 秒把 `LlmClient` 里累计的 token 刷进 `usage_log`
//! - 每 6 小时（以及启动时）删掉超过 `memory.retain_days` 的原始消息

use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use crate::db;
use crate::state::{now_ts, AppState};

pub async fn ticker(state: Arc<AppState>) {
    let mut flush = tokio::time::interval(Duration::from_secs(60));
    let mut cleanup = tokio::time::interval(Duration::from_secs(6 * 3600));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    cleanup.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // 第一次 tick 立即返回，顺便在启动时清一次旧消息
    flush.tick().await;

    loop {
        tokio::select! {
            _ = flush.tick() => flush_usage(&state).await,
            _ = cleanup.tick() => cleanup_messages(&state).await,
        }
    }
}

async fn flush_usage(state: &Arc<AppState>) {
    let Some(llm) = state.llm() else {
        return;
    };
    for (role, usage) in llm.drain_usage() {
        if let Err(e) = db::insert_usage(
            &state.db,
            now_ts(),
            role.as_str(),
            usage.prompt,
            usage.completion,
        )
        .await
        {
            warn!("写 token 统计失败: {e:#}");
        }
    }
}

async fn cleanup_messages(state: &Arc<AppState>) {
    let days = state.cfg().memory.retain_days;
    let cutoff = now_ts() - days as i64 * 86_400;

    match db::delete_messages_before(&state.db, cutoff).await {
        Ok(n) if n > 0 => info!("清理了 {n} 条超过 {days} 天的消息"),
        Ok(_) => {}
        Err(e) => warn!("清理旧消息失败: {e:#}"),
    }
}
