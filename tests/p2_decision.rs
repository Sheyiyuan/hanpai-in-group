//! P2 端到端：非 @ 消息进批量 → Planner 判断 → 触发函数 → 生成/沉默。
//!
//! 两条路径都要覆盖：Planner 说「可以接」时要真的发出去，说「不接」时要安静。

use std::sync::Arc;
use std::time::Duration;

use axum::routing::post;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use hanpai_in_group::config::{Config, ModelDef, Provider};
use hanpai_in_group::state::AppState;
use hanpai_in_group::{db, decision, router};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

async fn mock_llm() -> String {
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .route("/embeddings", post(embed));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn chat(Json(body): Json<Value>) -> Json<Value> {
    let model = body["model"].as_str().unwrap_or("");
    let content = match model {
        "planner" => {
            let user = body["messages"]
                .as_array()
                .and_then(|a| a.iter().rev().find(|m| m["role"] == "user"))
                .and_then(|m| m["content"].as_str())
                .unwrap_or("");
            if user.contains('猫') {
                json!({"action":"reply","interest":1.0,"confidence":0.9,"reason":"聊到猫了"})
                    .to_string()
            } else {
                json!({"action":"wait","interest":0.1,"confidence":0.9,"reason":"跟我无关"})
                    .to_string()
            }
        }
        _ => "确实，猫就爱这样。".to_string(),
    };

    Json(json!({
        "choices": [{ "message": { "role": "assistant", "content": content } }]
    }))
}

async fn embed(Json(body): Json<Value>) -> Json<Value> {
    let inputs = body["input"].as_array().cloned().unwrap_or_default();
    let data: Vec<Value> = inputs
        .iter()
        .map(|v| {
            let s = v.as_str().unwrap_or("");
            let mut seed = 0u64;
            for b in s.bytes() {
                seed = seed.wrapping_mul(131).wrapping_add(b as u64);
            }
            let vec: Vec<f64> = (0..8)
                .map(|i| (((seed >> (i * 3)) & 0xff) as f64) / 255.0 - 0.5)
                .collect();
            json!({ "embedding": vec })
        })
        .collect();
    Json(json!({ "data": data }))
}

fn group_event(group_id: i64, user_id: i64, msg_id: i64, text: &str) -> String {
    json!({
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "time": 1_760_000_000i64,
        "self_id": 10001,
        "group_id": group_id,
        "user_id": user_id,
        "message_id": msg_id,
        "raw_message": text,
        "message": [{ "type": "text", "data": { "text": text } }],
        "sender": { "user_id": user_id, "nickname": "群友", "card": "" }
    })
    .to_string()
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn expect_send_group_msg(ws: &mut Ws) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(left, ws.next())
            .await
            .expect("等待机器人回包超时")
            .expect("连接已关闭")
            .expect("ws 读错误");
        let Message::Text(text) = frame else { continue };
        let v: Value = serde_json::from_str(text.as_str()).unwrap();

        if let Some(action) = v.get("action").and_then(|a| a.as_str()) {
            if action == "send_group_msg" {
                if let Some(echo) = v.get("echo").cloned() {
                    let ack = json!({ "status": "ok", "retcode": 0,
                                      "data": { "message_id": 5555 }, "echo": echo });
                    ws.send(Message::Text(ack.to_string())).await.unwrap();
                }
                return v;
            }
            if let Some(echo) = v.get("echo").cloned() {
                let ack = json!({ "status": "ok", "retcode": 0, "data": {}, "echo": echo });
                ws.send(Message::Text(ack.to_string())).await.unwrap();
            }
        }
    }
}

async fn setup(group_id: i64, dry_run: bool) -> (Arc<AppState>, Ws, tempfile::TempDir) {
    let base = mock_llm().await;
    std::env::set_var("TEST_LLM_KEY", "test-key");

    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect(&dir.path().join("p2.db")).await.unwrap();
    db::migrate(&pool).await.unwrap();

    let mut cfg = Config::default();
    cfg.groups.whitelist = vec![group_id];
    cfg.groups.dry_run = dry_run;
    cfg.groups.talk_value = 1.0;
    cfg.groups.cooldown_secs = 0;
    cfg.groups.debounce_quiet_ms = 150;
    cfg.groups.debounce_active_ms = 150;
    cfg.groups.max_per_hour = -1;
    cfg.groups.max_per_day = -1;
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
        ("summary", "summary"),
        ("planner", "planner"),
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
    cfg.model.summary = vec!["summary".into()];
    cfg.model.planner = vec!["planner".into()];
    cfg.model.max_retries = 0;
    cfg.embedding.enabled = false;

    let state = AppState::new(cfg, pool, dir.path().join("config.toml"));
    tokio::spawn(decision::ticker(state.clone()));

    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let url = format!("ws://{addr}/onebot");
    let (ws, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .unwrap();
    (state, ws, dir)
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_replies_when_interested() {
    let (state, mut ws, _dir) = setup(333, false).await;

    for (i, text) in ["我家猫今天又把杯子打翻了", "真的服了，这都第三次了"]
        .iter()
        .enumerate()
    {
        ws.send(Message::Text(group_event(333, 1001, 600 + i as i64, text)))
            .await
            .unwrap();
    }

    let sent = expect_send_group_msg(&mut ws).await;
    let text = sent["params"]["message"][0]["data"]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains('猫'), "应该接上猫的话题，实际: {text}");

    let mut decision_row = None;
    for _ in 0..40 {
        if let Some(d) = db::last_decision(&state.db, 333).await.unwrap() {
            decision_row = Some(d);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let d = decision_row.expect("应该有决策记录");
    assert_eq!(d.trigger, "batch");
    assert_eq!(d.action, "reply");
    assert!(
        d.reason.contains("interest"),
        "理由里应带兴趣值: {}",
        d.reason
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn planner_waits_when_not_interested() {
    let (state, mut ws, _dir) = setup(444, false).await;

    ws.send(Message::Text(group_event(444, 1001, 700, "今天天气不错")))
        .await
        .unwrap();

    let res =
        tokio::time::timeout(Duration::from_millis(1500), expect_send_group_msg(&mut ws)).await;
    assert!(res.is_err(), "Planner 说 wait 时不该发言");

    let mut decision_row = None;
    for _ in 0..40 {
        if let Some(d) = db::last_decision(&state.db, 444).await.unwrap() {
            decision_row = Some(d);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let d = decision_row.expect("应该有决策记录");
    assert_eq!(d.action, "wait");
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_batch_does_not_send() {
    let (state, mut ws, _dir) = setup(555, true).await;

    ws.send(Message::Text(group_event(555, 1001, 800, "我家猫又闯祸了")))
        .await
        .unwrap();

    let res =
        tokio::time::timeout(Duration::from_millis(2000), expect_send_group_msg(&mut ws)).await;
    assert!(res.is_err(), "dry_run 下不该发送");

    let mut decision_row = None;
    for _ in 0..40 {
        if let Some(d) = db::last_decision(&state.db, 555).await.unwrap() {
            decision_row = Some(d);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let d = decision_row.expect("应该有决策记录");
    assert_eq!(d.action, "dry_run");
}
