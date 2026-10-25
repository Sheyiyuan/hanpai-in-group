//! 群内管理命令。只有 admin.qq 里的人能触发。

use std::sync::Arc;
use std::time::Instant;

use crate::config::Config;
use crate::state::{now_ts, AppState};

pub async fn handle(
    state: &Arc<AppState>,
    group_id: i64,
    user_id: i64,
    text: &str,
) -> Option<String> {
    let cfg = state.cfg();
    if !cfg.admin.qq.contains(&user_id) {
        return None;
    }

    let mut it = text.split_whitespace();
    let cmd = it.next().unwrap_or("");
    let args: Vec<&str> = it.collect();

    let reply = match cmd {
        "/ping" => {
            let t = Instant::now();
            let ok = state
                .api
                .call("get_login_info", serde_json::json!({}))
                .await
                .is_ok();
            format!(
                "pong {}ms{}",
                t.elapsed().as_millis(),
                if ok { "" } else { "（OneBot API 不通）" }
            )
        }
        "/status" => status(state, &cfg, group_id).await,
        "/mute" => {
            let mins = args
                .first()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(30)
                .clamp(1, 1440);
            let until = now_ts() + mins * 60;
            state.mute_until(group_id, until).await;
            format!("已静音 {mins} 分钟（仍会继续记录消息）")
        }
        "/unmute" => {
            state.unmute(group_id).await;
            "已解除静音".to_string()
        }
        "/persona" => crate::persona::system_block(&cfg.persona, &cfg.bot.nickname),
        "/why" => why(state, group_id).await,
        "/forget" => forget(state, group_id, &args).await,
        "/probe" => probe(state, group_id).await,
        "/reload" => reload(state).await,
        "/help" => "可用命令：/ping /status /persona /mute [分钟] /unmute /why /reload".to_string(),
        _ => return None,
    };
    Some(reply)
}

async fn status(state: &Arc<AppState>, cfg: &Config, group_id: i64) -> String {
    let since = now_ts() - 86_400;
    let self_id = state.self_qq(cfg);
    let recent = crate::db::count_incoming_since(&state.db, group_id, since, self_id)
        .await
        .unwrap_or(-1);
    let uptime = state.started_at.elapsed().as_secs();
    let muted = match state.muted_until(group_id).await {
        Some(until) if until > now_ts() => format!("静音中（剩 {} 分钟）", (until - now_ts()) / 60),
        _ => "未静音".to_string(),
    };
    let policy = cfg.groups.policy(group_id);
    let facts = crate::db::count_facts(&state.db, group_id)
        .await
        .unwrap_or(-1);
    let (prompt_tokens, completion_tokens) = crate::db::usage_since(&state.db, since)
        .await
        .unwrap_or((0, 0));
    let queued = state
        .outbox
        .read()
        .await
        .get(&group_id)
        .map(|q| q.len())
        .unwrap_or(0);
    let models = state
        .llm()
        .map(|l| l.describe())
        .unwrap_or_else(|| "未配置（没有可用路由）".to_string());

    format!(
        "在线 {}h{}m｜近 24h 消息 {recent} 条｜记忆 {facts} 条｜{muted}\n\
         白名单 {} 群｜talk_value {:.2}｜{}｜dry_run {}｜排队 {queued}｜OneBot {}\n\
         模型：{models}\n24h tokens：{prompt_tokens} 输入 / {completion_tokens} 输出",
        uptime / 3600,
        (uptime % 3600) / 60,
        cfg.groups.whitelist.len(),
        policy.talk_value,
        limits_text(&policy),
        if policy.dry_run { "开" } else { "关" },
        if state.api.is_connected() {
            "已连接"
        } else {
            "未连接"
        }
    )
}

async fn reload(state: &Arc<AppState>) -> String {
    match Config::load(&state.config_path) {
        Ok(new_cfg) => {
            state.reload_onebot(&new_cfg);

            match crate::llm::LlmClient::new(&new_cfg) {
                Ok(v) => state.llm.store(v.map(Arc::new)),
                Err(e) => return format!("配置已载入但模型客户端创建失败：{e:#}"),
            }
            match crate::embed::Embedder::from_config(&new_cfg) {
                Ok(v) => state.embedder.store(Arc::new(v)),
                Err(e) => {
                    tracing::warn!("重载后向量客户端不可用: {e:#}，降级为关键词检索");
                    state
                        .embedder
                        .store(Arc::new(crate::embed::Embedder::disabled()));
                }
            }
            state
                .prompts
                .store(Arc::new(crate::prompts::PromptSet::load(
                    &new_cfg.prompts.dir,
                )));

            state.cfg.store(Arc::new(new_cfg));
            "配置与提示词已重载（监听地址/路径变更需重启）".to_string()
        }
        Err(e) => format!("重载失败，保持旧配置：{e:#}"),
    }
}

async fn why(state: &Arc<AppState>, group_id: i64) -> String {
    match crate::db::last_decision(&state.db, group_id).await {
        Ok(Some(d)) => {
            let facts: Vec<String> = serde_json::from_str(&d.facts_used).unwrap_or_default();
            let facts_text = if facts.is_empty() {
                "（没有用到记忆）".to_string()
            } else {
                facts.join("；")
            };
            format!(
                "最近一次判断（{}）：{} / {}\n理由：{}\n用到的记忆：{}",
                ago(d.ts),
                d.trigger,
                d.action,
                d.reason,
                facts_text
            )
        }
        Ok(None) => "还没有决策记录。".to_string(),
        Err(e) => format!("查询失败：{e:#}"),
    }
}

async fn probe(state: &Arc<AppState>, group_id: i64) -> String {
    let params = serde_json::json!({
        "group_id": group_id,
        "message": [{ "type": "text", "data": { "text": "[probe] 连通性测试" } }]
    });
    match state.api.call("send_group_msg", params).await {
        Ok(v) => format!(
            "发送返回：status={} retcode={} message_id={}",
            v["status"].as_str().unwrap_or("-"),
            v["retcode"].as_i64().unwrap_or(-1),
            v["data"]["message_id"]
                .as_i64()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "无".to_string())
        ),
        Err(e) => format!("发送失败：{e:#}"),
    }
}

async fn forget(state: &Arc<AppState>, group_id: i64, args: &[&str]) -> String {
    let Some(arg) = args.first() else {
        return "用法：/forget all（整群） 或 /forget 123456（按 QQ）".to_string();
    };

    let result = if arg.eq_ignore_ascii_case("all") {
        crate::db::delete_facts(&state.db, group_id, None).await
    } else {
        let qq = arg.trim_start_matches('@').parse::<i64>().unwrap_or(0);
        if qq <= 0 {
            return "没看懂这个 QQ 号，用法：/forget all 或 /forget 123456".to_string();
        }
        crate::db::delete_facts(&state.db, group_id, Some(qq)).await
    };

    match result {
        Ok(n) => format!("已删除 {n} 条记忆（原始消息不受影响）"),
        Err(e) => format!("删除失败：{e:#}"),
    }
}

fn ago(ts: i64) -> String {
    let diff = (crate::state::now_ts() - ts).max(0);
    if diff < 60 {
        format!("{diff} 秒前")
    } else if diff < 3600 {
        format!("{} 分钟前", diff / 60)
    } else if diff < 86400 {
        format!("{} 小时前", diff / 3600)
    } else {
        format!("{} 天前", diff / 86400)
    }
}

fn limits_text(policy: &crate::config::GroupPolicy) -> String {
    let hour = if policy.max_per_hour < 0 {
        "不限".to_string()
    } else {
        format!("{} 次", policy.max_per_hour)
    };
    let day = if policy.max_per_day < 0 {
        "不限".to_string()
    } else {
        format!("{} 次", policy.max_per_day)
    };
    format!("每小时上限 {hour} / 每天 {day}")
}
