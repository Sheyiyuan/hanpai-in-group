//! 应用侧适配层：把 [`crate::onebot`] 协议模块接到本项目的业务逻辑。
//!
//! 协议细节（鉴权、角色、心跳、队列、echo 回包）都在 `onebot::server` 里，
//! 这里只负责：解析 token、记录 self_id、把事件分发给白名单/记忆/决策链。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::extract::ws::{WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tracing::{debug, error, info, trace, warn};

use crate::admin;
use crate::config::Config;
use crate::db;
use crate::decision;
use crate::onebot::event::{self, Incoming};
use crate::onebot::{authorize, ConnMeta};
use crate::state::{now_ts, AppState};
use crate::vision;

pub async fn ws_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let cfg = state.cfg();
    let token = match resolve_access_token(&cfg) {
        Ok(t) => t,
        Err(e) => {
            error!("{e}");
            return StatusCode::UNAUTHORIZED.into_response();
        }
    };
    if !authorize(
        token.as_deref(),
        header_str(&headers, AUTHORIZATION.as_str()),
    ) {
        error!("OneBot 鉴权失败，拒绝连接");
        return StatusCode::UNAUTHORIZED.into_response();
    }

    let meta = ConnMeta::from_headers(
        header_str(&headers, "x-self-id"),
        header_str(&headers, "x-client-role"),
    );
    if let Some(id) = meta.self_id {
        state.self_id.store(id, Ordering::Relaxed);
    }
    info!(
        "OneBot 连接：role={:?} self_id={:?}",
        meta.role, meta.self_id
    );

    ws.on_upgrade(move |socket| handle_socket(state, socket, meta))
        .into_response()
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// 从配置指定的环境变量取 token。
///
/// 配了变量名但环境变量缺失/为空 → Err（fail-closed）。
/// `access_token_env` 为 `None` 或空字符串 → 本机调试，不鉴权。
fn resolve_access_token(cfg: &Config) -> Result<Option<String>, String> {
    let Some(var) = cfg.onebot.access_token_env.as_deref() else {
        return Ok(None);
    };
    let var = var.trim();
    if var.is_empty() {
        return Ok(None);
    }
    match std::env::var(var) {
        Ok(t) if !t.is_empty() => Ok(Some(t)),
        _ => Err(format!(
            "onebot.access_token_env = {var}，但环境变量缺失或为空；拒绝连接。\
             本机调试不鉴权请把 access_token_env 设为空字符串"
        )),
    }
}

async fn handle_socket(state: Arc<AppState>, socket: WebSocket, meta: ConnMeta) {
    let mut conn = state.onebot.accept(socket, meta);
    while let Some(ev) = conn.recv().await {
        dispatch(&state, ev).await;
    }
    conn.close().await;
}

async fn dispatch(state: &Arc<AppState>, ev: Incoming) {
    // 每条事件都带 self_id，别只依赖 meta_event
    if let Some(id) = ev.self_id.filter(|id| *id > 0) {
        state.self_id.store(id, Ordering::Relaxed);
    }

    match ev.post_type.as_deref() {
        Some("meta_event") => match ev.meta_event_type.as_deref() {
            Some("lifecycle") => info!("OneBot 生命周期：sub_type={:?}", ev.sub_type),
            Some("heartbeat") => debug!(
                "OneBot 心跳：interval={:?} status={:?}",
                ev.interval, ev.status
            ),
            other => debug!("忽略元事件 meta_event_type={other:?}"),
        },
        Some("message") => match ev.message_type.as_deref() {
            Some("group") => {
                if let Err(e) = handle_group_message(state, ev).await {
                    warn!("处理群消息失败: {e:#}");
                }
            }
            other => trace!("忽略非群消息 message_type={other:?}"),
        },
        Some("message_sent") => trace!("忽略机器人自身发送回执"),
        Some("notice") => log_notice(state, &ev).await,
        Some("request") => info!(
            "收到请求事件 request_type={:?} group={:?} user={:?}（第一版不处理）",
            ev.request_type, ev.group_id, ev.user_id
        ),
        other => trace!("忽略事件类型 {other:?}"),
    }
}

async fn log_notice(state: &Arc<AppState>, ev: &Incoming) {
    if ev.notice_type.as_deref() == Some("group_recall") {
        let self_id = state.self_qq(&state.cfg());
        if let Some(target) = ev.target_id {
            let author = db::message_sender(&state.db, target).await.ok().flatten();
            if self_id > 0 && author == Some(self_id) {
                warn!(
                    group_id = ev.group_id,
                    msg_id = target,
                    "自己发的消息被撤回了（大概率触发风控，注意降频）"
                );
            }
        }
    }

    if let Some(group_id) = ev.group_id {
        info!(
            "群通知 notice_type={:?} sub_type={:?} group={group_id} user={:?} operator={:?} target={:?}",
            ev.notice_type, ev.sub_type, ev.user_id, ev.operator_id, ev.target_id
        );
    } else {
        debug!(
            "通知 notice_type={:?} sub_type={:?} user={:?}",
            ev.notice_type, ev.sub_type, ev.user_id
        );
    }
}

async fn handle_group_message(state: &Arc<AppState>, ev: Incoming) -> anyhow::Result<()> {
    let cfg = state.cfg();
    let self_id = state.self_qq(&cfg);

    let Some(group_id) = ev.group_id.filter(|id| *id > 0) else {
        return Ok(());
    };
    // 白名单闸门：非白名单群零写入
    if !cfg.groups.is_allowed(group_id) {
        trace!("忽略非白名单群 {group_id}");
        return Ok(());
    }
    // sub_type=notice 是“管理员已禁止匿名聊天”这类系统提示，不是人话
    if ev.sub_type.as_deref() == Some("notice") {
        trace!("忽略群系统提示消息");
        return Ok(());
    }

    let Some(user_id) = ev.user_id.filter(|id| *id > 0) else {
        warn!("群 {group_id} 的消息缺 user_id 或为 0，忽略");
        return Ok(());
    };

    if cfg.groups.blacklist_users.contains(&user_id) {
        trace!("忽略黑名单用户 {user_id}");
        return Ok(());
    }
    // 协议端把机器人自己的消息也当 message 上报时，别自问自答
    if self_id > 0 && user_id == self_id {
        trace!("忽略机器人自己的消息 {msg_id:?}", msg_id = ev.message_id);
        return Ok(());
    }

    let Some(msg_id) = ev.message_id else {
        warn!("群 {group_id} 的消息缺 message_id，丢弃（可能是协议端异常）");
        return Ok(());
    };

    let raw = ev.raw_message.clone().unwrap_or_default();
    let segments = ev.segments();
    let (text, sanitized) = enrich(state, &cfg, group_id, &segments, &raw).await;
    let segments_json = serde_json::to_string(&sanitized).unwrap_or_else(|_| "[]".into());
    let reply_to = event::reply_target(Some(&segments));
    let sender = ev.sender.clone().unwrap_or_default();
    let nickname = sender.nickname.clone().unwrap_or_default();
    let card = sender.card.clone().unwrap_or_default();
    let ts = ev.time.filter(|t| *t > 0).unwrap_or_else(now_ts);

    let mut trigger = event::detect_trigger(&segments, &text, self_id, &cfg.bot.aliases);
    if trigger.is_none() {
        if let Some(quoted) = reply_to {
            match db::message_sender(&state.db, quoted).await {
                Ok(Some(author)) if self_id > 0 && author == self_id => {
                    trigger = Some("reply");
                }
                Ok(_) => {}
                Err(e) => debug!("查询被引用消息失败: {e:#}"),
            }
        }
    }
    let addressed = trigger.is_some();

    let is_new = db::insert_message(
        &state.db,
        &db::NewMessage {
            msg_id,
            group_id,
            user_id,
            nickname: &nickname,
            card: &card,
            ts,
            text: &text,
            segments: &segments_json,
            reply_to,
            addressed,
        },
        cfg.onebot.dedup_window_secs,
    )
    .await?;

    if !is_new {
        debug!("重复消息 {msg_id}，跳过");
        return Ok(());
    }

    let display = if card.trim().is_empty() {
        nickname.as_str()
    } else {
        card.as_str()
    };
    if let Err(e) = db::upsert_user(&state.db, user_id, display, ts).await {
        warn!("更新用户失败: {e:#}");
    }

    // 管理命令优先，静音期间也能用（否则没法 /unmute）
    if text.starts_with('/') && cfg.admin.qq.contains(&user_id) {
        if let Some(reply) = admin::handle(state, group_id, user_id, &text).await {
            if let Err(e) = state.api.send_group_msg(group_id, &reply).await {
                warn!("发送管理回复失败: {e:#}");
            }
        }
        return Ok(());
    }

    if state.is_muted(group_id).await {
        trace!("群 {group_id} 静音中，只记录不发言");
        return Ok(());
    }

    // 被叫到：@ / 引用回复 / 提到别名
    if let Some(kind) = trigger {
        let allowed = match kind {
            "at" | "reply" => cfg.groups.inevitable_at_reply,
            _ => cfg.groups.inevitable_when_named,
        };
        if allowed {
            respond_to_trigger(state, &cfg, group_id, display, &text, kind).await;
            return Ok(());
        }
        debug!("群 {group_id} 被叫到（{kind}）但配置为不必然回，转入概率判定");
    }

    // 没被叫到（或被叫到但不必然回）：进批量，交给 Planner 判断值不值得插话
    decision::on_message(
        state,
        group_id,
        decision::BatchItem {
            msg_id,
            user_id,
            name: display.to_string(),
            text: text.clone(),
            ts,
        },
        cfg.groups.debounce_quiet_ms,
        cfg.groups.debounce_active_ms,
    )
    .await;

    Ok(())
}

async fn respond_to_trigger(
    state: &Arc<AppState>,
    cfg: &Config,
    group_id: i64,
    sender: &str,
    text: &str,
    trigger: &str,
) {
    let policy = cfg.groups.policy(group_id);

    info!(
        group_id,
        trigger,
        interest = 1.0,
        reason = "被叫到",
        "直接回复"
    );

    match crate::reply::respond(state, group_id, sender, text).await {
        Ok(Some(r)) => {
            crate::reply::deliver(state, &policy, group_id, trigger, "被叫到", 1.0, r).await;
        }
        Ok(None) => {
            log_decision(
                state,
                group_id,
                trigger,
                "skip",
                "生成内容不可用",
                "[]",
                None,
            )
            .await;
        }
        Err(e) => {
            warn!("生成回复失败: {e:#}");
            log_decision(
                state,
                group_id,
                trigger,
                "error",
                &format!("{e:#}"),
                "[]",
                None,
            )
            .await;
        }
    }
}

async fn log_decision(
    state: &Arc<AppState>,
    group_id: i64,
    trigger: &str,
    action: &str,
    reason: &str,
    facts_used: &str,
    sent_msg_id: Option<i64>,
) {
    let row = db::NewDecision {
        group_id,
        ts: now_ts(),
        trigger,
        action,
        reason,
        confidence: 1.0,
        facts_used,
        sent_msg_id,
    };
    if let Err(e) = db::insert_decision(&state.db, &row).await {
        warn!("写决策日志失败: {e:#}");
    }
}

/// 把消息段转成「文本 + 净化后的段」：
/// 图片转述成文字、系统表情查表、原始图片引用直接丢弃（不落库）。
async fn enrich(
    state: &Arc<AppState>,
    cfg: &Config,
    group_id: i64,
    segments: &[event::Segment],
    raw: &str,
) -> (String, Vec<serde_json::Value>) {
    let mut text = String::new();
    let mut sanitized: Vec<serde_json::Value> = Vec::with_capacity(segments.len());
    let mut budget = cfg.vision.max_images_per_message;

    for seg in segments {
        match seg.kind.as_str() {
            "text" => {
                if let Some(t) = seg.data.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
                if let Ok(v) = serde_json::to_value(seg) {
                    sanitized.push(v);
                }
            }
            "at" => {
                let qq = seg.data.get("qq").and_then(|v| v.as_str()).unwrap_or("");
                text.push('@');
                text.push_str(qq);
                if let Ok(v) = serde_json::to_value(seg) {
                    sanitized.push(v);
                }
            }
            "reply" => {
                if let Ok(v) = serde_json::to_value(seg) {
                    sanitized.push(v);
                }
            }
            "face" => {
                let id = seg
                    .data
                    .get("id")
                    .map(|v| v.to_string().trim_matches('"').to_string())
                    .unwrap_or_default();
                let label = match vision::face_name(&id) {
                    Some(name) => format!("[表情:{name}]"),
                    None => format!("[表情:{id}]"),
                };
                text.push_str(&label);
                sanitized.push(serde_json::json!({ "type": "text", "data": { "text": label } }));
            }
            "image" | "mface" => {
                let url = seg
                    .data
                    .get("url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .or_else(|| {
                        seg.data
                            .get("file")
                            .and_then(|v| v.as_str())
                            .filter(|s| s.starts_with("http"))
                            .map(|s| s.to_string())
                    });

                let mut label = "[图片]".to_string();
                if cfg.vision.enabled && budget > 0 {
                    if let Some(url) = url {
                        budget -= 1;
                        if let Some(cap) = vision::describe(state, &url).await {
                            label = vision::render(&cap);
                            if vision::is_sticker(seg) {
                                crate::sticker::collect(
                                    state,
                                    group_id,
                                    &url,
                                    &cap.caption,
                                    &cap.emotion,
                                    cap.reusable,
                                )
                                .await;
                            }
                        }
                    }
                }
                text.push_str(&label);
                sanitized.push(serde_json::json!({ "type": "text", "data": { "text": label } }));
            }
            _ => {}
        }
    }

    if text.trim().is_empty() {
        text = raw.to_string();
    }
    (text.trim().to_string(), sanitized)
}
