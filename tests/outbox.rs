//! 冷却期不是丢弃：回复进入队列，冷却结束后按序发出。

use std::time::{Duration, Instant};

use axum::routing::post;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use hanpai_in_group::config::{Config, ModelDef, Provider};
use hanpai_in_group::state::AppState;
use hanpai_in_group::{db, reply, router};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .try_init();
}

async fn mock_model() -> String {
    async fn chat(Json(body): Json<Value>) -> Json<Value> {
        let model = body["model"].as_str().unwrap_or("");
        let content = match model {
            "planner" => {
                json!({"action":"wait","interest":0.1,"confidence":0.9,"reason":"无关"}).to_string()
            }
            "summary" => json!({"facts":[]}).to_string(),
            _ => {
                // 两次回复必须不同，否则会被"复读最近消息"过滤掉
                if body.to_string().contains("再说一句") {
                    "第二句".to_string()
                } else {
                    "第一句".to_string()
                }
            }
        };
        Json(json!({
            "choices": [{ "message": { "role": "assistant", "content": content } }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 1 }
        }))
    }

    let app = Router::new().route("/chat/completions", post(chat));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn event(group_id: i64, msg_id: i64, text: &str) -> String {
    json!({
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "time": 1_760_000_000i64,
        "self_id": 10001,
        "group_id": group_id,
        "user_id": 1001,
        "message_id": msg_id,
        "raw_message": text,
        "message": [{ "type": "text", "data": { "text": text } }],
        "sender": { "user_id": 1001, "nickname": "老王", "card": "" }
    })
    .to_string()
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn expect_send(ws: &mut Ws) -> (Value, Instant) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let frame = tokio::time::timeout(left, ws.next())
            .await
            .expect("等待回包超时")
            .expect("连接关闭")
            .expect("ws 错误");
        let Message::Text(text) = frame else { continue };
        let v: Value = serde_json::from_str(text.as_str()).unwrap();
        if let Some(action) = v.get("action").and_then(|a| a.as_str()) {
            if action == "send_group_msg" {
                if let Some(echo) = v.get("echo").cloned() {
                    let ack = json!({ "status": "ok", "retcode": 0,
                                      "data": { "message_id": 9 }, "echo": echo });
                    ws.send(Message::Text(ack.to_string())).await.unwrap();
                }
                return (v, Instant::now());
            }
            if let Some(echo) = v.get("echo").cloned() {
                let ack = json!({ "status": "ok", "retcode": 0, "data": {}, "echo": echo });
                ws.send(Message::Text(ack.to_string())).await.unwrap();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn second_reply_waits_for_cooldown_instead_of_being_dropped() {
    init_logs();
    let base = mock_model().await;
    std::env::set_var("TEST_LLM_KEY", "k");

    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect(&dir.path().join("outbox.db")).await.unwrap();
    db::migrate(&pool).await.unwrap();

    let mut cfg = Config::default();
    cfg.groups.whitelist = vec![888];
    cfg.groups.dry_run = false;
    cfg.groups.cooldown_secs = 2;
    cfg.groups.reply_delay_min_ms = 0;
    cfg.groups.reply_delay_max_ms = 0;
    cfg.bot.aliases = vec!["半拍".into()];
    cfg.onebot.access_token_env = None;
    cfg.providers.insert(
        "mock".into(),
        Provider {
            base_url: base,
            api_key_env: "TEST_LLM_KEY".into(),
        },
    );
    for (alias, model) in [
        ("chat", "chat"),
        ("planner", "planner"),
        ("summary", "summary"),
    ] {
        cfg.models.insert(
            alias.into(),
            ModelDef {
                provider: "mock".into(),
                model: model.into(),
                ..Default::default()
            },
        );
    }
    cfg.model.chat = vec!["chat".into()];
    cfg.model.planner = vec!["planner".into()];
    cfg.model.summary = vec!["summary".into()];
    cfg.model.max_retries = 0;
    cfg.embedding.enabled = false;

    let state = AppState::new(cfg, pool, dir.path().join("config.toml"));
    tokio::spawn(reply::outbox_ticker(state.clone()));

    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let url = format!("ws://{addr}/onebot");
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .unwrap();

    // 第一条：立即回
    ws.send(Message::Text(event(888, 1, "半拍 在吗")))
        .await
        .unwrap();
    let (_first, t1) = expect_send(&mut ws).await;

    // 第二条：冷却中，应排队而不是丢弃
    ws.send(Message::Text(event(888, 2, "半拍 再说一句")))
        .await
        .unwrap();

    let (_second, t2) = expect_send(&mut ws).await;
    let gap = t2.duration_since(t1);
    assert!(
        gap >= Duration::from_millis(1000),
        "第二条应等冷却结束，实际间隔 {gap:?}"
    );

    // 两条都应该落盘成 reply，第二条带「排队」（发送和落库之间有一小段异步）
    let mut rows: Vec<(String, String)> = Vec::new();
    for _ in 0..40 {
        rows =
            sqlx::query_as("SELECT action, reason FROM decisions WHERE group_id = 888 ORDER BY id")
                .fetch_all(&state.db)
                .await
                .unwrap();
        if rows.len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(rows.len(), 2, "两次回复都应有决策记录: {rows:?}");
    assert!(rows.iter().all(|(a, _)| a == "reply"), "{rows:?}");
    assert!(
        rows.iter().any(|(_, r)| r.contains("排队")),
        "第二条应标记排队: {rows:?}"
    );
}
