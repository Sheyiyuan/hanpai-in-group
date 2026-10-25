//! P1 端到端：消息落库 → 记忆抽取 → 检索 → 被叫时带记忆回答。
//!
//! 模型和向量都用本地 mock 服务顶替，不花钱、不联网，但走的是真实 HTTP。

use std::time::Duration;

use axum::routing::post;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use hanpai_in_group::config::{Config, ModelDef, Provider};
use hanpai_in_group::state::AppState;
use hanpai_in_group::{db, memory, router};
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
    let content = if model == "summary" {
        json!({
            "facts": [
                {"content": "老王(1001) 下周要去成都出差", "subject_qq": 1001, "importance": 4, "confidence": 0.9},
                {"content": "老王(1001) 养了一只叫豆豆的橘猫", "subject_qq": 1001, "importance": 3, "confidence": 0.8}
            ]
        })
        .to_string()
    } else {
        "记得，你下周要去成都出差。".to_string()
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
        "sender": { "user_id": user_id, "nickname": "老王", "card": "老王" }
    })
    .to_string()
}

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn expect_send_group_msg(ws: &mut Ws) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
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
                // 机器人会等这条的 echo 回包才会继续往下走（写决策日志）
                if let Some(echo) = v.get("echo").cloned() {
                    let ack = json!({ "status": "ok", "retcode": 0,
                                      "data": { "message_id": 4242 }, "echo": echo });
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

#[tokio::test(flavor = "multi_thread")]
async fn remembers_then_answers_with_memory() {
    let base = mock_llm().await;
    std::env::set_var("TEST_LLM_KEY", "test-key");

    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect(&dir.path().join("p1.db")).await.unwrap();
    db::migrate(&pool).await.unwrap();

    let mut cfg = Config::default();
    cfg.groups.whitelist = vec![111];
    cfg.groups.dry_run = false;
    cfg.groups.reply_delay_min_ms = 0;
    cfg.groups.reply_delay_max_ms = 0;
    cfg.bot.aliases = vec!["半拍".into()];
    cfg.admin.qq = vec![9999];
    cfg.onebot.access_token_env = None;
    cfg.providers.insert(
        "mock".into(),
        Provider {
            base_url: base.clone(),
            api_key_env: "TEST_LLM_KEY".into(),
        },
    );
    for (alias, model) in [
        ("chat", "chat"),
        ("summary", "summary"),
        ("planner", "planner"),
        ("embed", "embed"),
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
    cfg.embedding.enabled = true;
    cfg.embedding.model = "embed".into();
    cfg.embedding.dim = 8;

    let state = AppState::new(cfg, pool, dir.path().join("config.toml"));
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

    for (i, text) in [
        "我下周要去成都出差",
        "大概去五天，那边有个会",
        "对了，我养的橘猫叫豆豆",
    ]
    .iter()
    .enumerate()
    {
        ws.send(Message::Text(group_event(111, 1001, 100 + i as i64, text)))
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(600)).await;

    // 手动触发一次抽取（生产环境由 ticker 按批量/时间触发）
    let inserted = memory::extract_group(&state, 111).await.expect("抽取记忆");
    assert_eq!(inserted, 2, "mock 返回两条事实，应该都写入");
    assert_eq!(db::count_facts(&state.db, 111).await.unwrap(), 2);

    let facts = db::facts_for_group(&state.db, 111, 10).await.unwrap();
    assert!(facts.iter().any(|f| f.content.contains("成都")));
    assert!(facts.iter().any(|f| f.content.contains("豆豆")));

    // 被叫到时应该带着记忆回答
    ws.send(Message::Text(group_event(
        111,
        1001,
        200,
        "半拍 你还记得我下周要去哪吗",
    )))
    .await
    .unwrap();

    let sent = expect_send_group_msg(&mut ws).await;
    let text = sent["params"]["message"][0]["data"]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("成都"), "回复应带上记忆，实际: {text}");

    // 发送和落库之间有一小段异步，轮询等一下
    let mut decision = None;
    for _ in 0..40 {
        if let Some(d) = db::last_decision(&state.db, 111).await.unwrap() {
            decision = Some(d);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let decision = decision.expect("应该有决策记录");
    assert_eq!(decision.action, "reply");
    assert!(
        decision.facts_used.contains("成都"),
        "决策日志里应记录用到的记忆: {}",
        decision.facts_used
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_does_not_send_but_logs() {
    let base = mock_llm().await;
    std::env::set_var("TEST_LLM_KEY", "test-key");

    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect(&dir.path().join("dry.db")).await.unwrap();
    db::migrate(&pool).await.unwrap();

    let mut cfg = Config::default();
    cfg.groups.whitelist = vec![222];
    cfg.groups.dry_run = true;
    cfg.groups.reply_delay_min_ms = 0;
    cfg.groups.reply_delay_max_ms = 0;
    cfg.bot.aliases = vec!["半拍".into()];
    cfg.onebot.access_token_env = None;
    cfg.providers.insert(
        "mock".into(),
        Provider {
            base_url: base.clone(),
            api_key_env: "TEST_LLM_KEY".into(),
        },
    );
    for (alias, model) in [("chat", "chat"), ("summary", "summary")] {
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
    cfg.embedding.enabled = false;

    let state = AppState::new(cfg, pool, dir.path().join("config.toml"));
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

    ws.send(Message::Text(group_event(222, 1001, 300, "半拍在吗")))
        .await
        .unwrap();

    let res =
        tokio::time::timeout(Duration::from_millis(1500), expect_send_group_msg(&mut ws)).await;
    assert!(res.is_err(), "dry_run 下不应该真的发送");

    let decision = db::last_decision(&state.db, 222)
        .await
        .unwrap()
        .expect("应有决策");
    assert_eq!(decision.action, "dry_run");
}
