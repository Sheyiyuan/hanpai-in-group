//! 图片入库转述 + 表情包收藏复用 + 外链图片自动降级。

use std::sync::Arc;
use std::time::Duration;

use axum::http::header;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use hanpai_in_group::config::{Config, ModelDef, Provider};
use hanpai_in_group::state::AppState;
use hanpai_in_group::{db, router};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

const PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4,
    0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE,
    0x42, 0x60, 0x82,
];

async fn mock_server(reject_url_images: bool) -> String {
    let state = Arc::new(reject_url_images);
    let app = Router::new()
        .route("/chat/completions", post(chat))
        .route("/img.png", get(img))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn img() -> impl axum::response::IntoResponse {
    ([(header::CONTENT_TYPE, "image/png")], PNG)
}

async fn chat(
    axum::extract::State(reject_url_images): axum::extract::State<Arc<bool>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let model = body["model"].as_str().unwrap_or("").to_string();
    if model == "caption" {
        if *reject_url_images {
            let raw = body.to_string();
            if raw.contains("\"url\":\"http") {
                return (axum::http::StatusCode::BAD_REQUEST, "image url not allowed")
                    .into_response();
            }
        }
        let content = json!({
            "caption": "一只橘猫趴在键盘上",
            "ocr": "",
            "emotion": "可爱",
            "reusable": true
        })
        .to_string();
        return Json(json!({
            "choices": [{ "message": { "role": "assistant", "content": content } }]
        }))
        .into_response();
    }

    let content = match model.as_str() {
        "planner" => {
            json!({"action":"wait","interest":0.1,"confidence":0.9,"reason":"无关"}).to_string()
        }
        "summary" => json!({"facts":[]}).to_string(),
        _ => "好可爱".to_string(),
    };
    Json(json!({
        "choices": [{ "message": { "role": "assistant", "content": content } }]
    }))
    .into_response()
}

fn image_event(group_id: i64, msg_id: i64, url: &str) -> String {
    json!({
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "time": 1_760_000_000i64,
        "self_id": 10001,
        "group_id": group_id,
        "user_id": 1001,
        "message_id": msg_id,
        "raw_message": "[图片]",
        "message": [{
            "type": "image",
            "data": { "file": "abc.png", "url": url, "sub_type": 1 }
        }],
        "sender": { "user_id": 1001, "nickname": "老王", "card": "" }
    })
    .to_string()
}

fn text_event(group_id: i64, msg_id: i64, text: &str) -> String {
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

async fn expect_send(ws: &mut Ws) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
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
                                      "data": { "message_id": 77 }, "echo": echo });
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

async fn setup(group_id: i64, reject_url_images: bool) -> (Arc<AppState>, Ws, tempfile::TempDir) {
    let base = mock_server(reject_url_images).await;
    std::env::set_var("TEST_LLM_KEY", "test-key");

    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect(&dir.path().join("vision.db")).await.unwrap();
    db::migrate(&pool).await.unwrap();

    let mut cfg = Config::default();
    cfg.groups.whitelist = vec![group_id];
    cfg.groups.dry_run = false;
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
    for (alias, model, visual) in [
        ("chat", "chat", true),
        ("planner", "planner", false),
        ("summary", "summary", false),
        ("caption", "caption", true),
    ] {
        cfg.models.insert(
            alias.into(),
            ModelDef {
                provider: "mock".into(),
                model: model.into(),
                visual,
                ..Default::default()
            },
        );
    }
    cfg.model.chat = vec!["chat".into()];
    cfg.model.planner = vec!["planner".into()];
    cfg.model.summary = vec!["summary".into()];
    cfg.model.caption = vec!["caption".into()];
    cfg.model.max_retries = 0;
    cfg.embedding.enabled = false;

    cfg.vision.enabled = true;
    cfg.vision.image_input = "url".into();
    cfg.vision.download_timeout_secs = 5;

    cfg.sticker.enabled = true;
    cfg.sticker.dir = dir.path().join("stickers").to_string_lossy().to_string();
    cfg.sticker.max_count = 8;
    cfg.sticker.send_probability = 1.0;

    let state = AppState::new(cfg, pool, dir.path().join("config.toml"));

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

async fn wait_message_text(state: &Arc<AppState>, group_id: i64) -> String {
    for _ in 0..60 {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT text FROM messages WHERE group_id = ? ORDER BY id DESC LIMIT 1")
                .bind(group_id)
                .fetch_optional(&state.db)
                .await
                .unwrap();
        if let Some((text,)) = row {
            if text.contains("[图片:") || text.contains("[图片]") {
                return text;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("等不到图片消息入库");
}

#[tokio::test(flavor = "multi_thread")]
async fn image_is_captioned_stored_and_reused_as_sticker() {
    let (state, mut ws, dir) = setup(666, false).await;
    let base = state.cfg().providers.get("mock").unwrap().base_url.clone();

    ws.send(Message::Text(image_event(
        666,
        900,
        &format!("{base}/img.png"),
    )))
    .await
    .unwrap();

    let text = wait_message_text(&state, 666).await;
    assert!(text.contains("橘猫"), "入库文本应包含转述: {text}");
    assert!(text.contains("[图片:"), "{text}");
    // 原始 URL 不应留在库里
    let (segments,): (String,) =
        sqlx::query_as("SELECT segments FROM messages WHERE group_id = ? ORDER BY id DESC LIMIT 1")
            .bind(666)
            .fetch_one(&state.db)
            .await
            .unwrap();
    assert!(
        !segments.contains("http"),
        "原始图片引用不该落库: {segments}"
    );

    // 表情包应已被收藏
    let mut collected = false;
    for _ in 0..30 {
        if db::count_stickers(&state.db, 666).await.unwrap() > 0 {
            collected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(collected, "可复用表情应该被收藏");

    let file: (String,) = sqlx::query_as("SELECT file FROM stickers WHERE group_id = 666")
        .fetch_one(&state.db)
        .await
        .unwrap();
    assert!(std::path::Path::new(&file.0).exists(), "表情文件应存在");

    // 被叫到时回复，应该带上这张表情
    ws.send(Message::Text(text_event(666, 901, "半拍 你看这是啥")))
        .await
        .unwrap();
    let sent = expect_send(&mut ws).await;
    let segments = sent["params"]["message"].as_array().unwrap();
    assert!(segments.len() >= 2, "回复应带表情段: {sent}");
    assert_eq!(segments[1]["type"], "image");
    let file = segments[1]["data"]["file"].as_str().unwrap_or("");
    assert!(
        file.starts_with("base64://"),
        "表情应以 base64 发送: {file}"
    );

    drop(dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn external_url_rejected_falls_back_to_inline() {
    let (state, mut ws, _dir) = setup(777, true).await;
    let base = state.cfg().providers.get("mock").unwrap().base_url.clone();

    ws.send(Message::Text(image_event(
        777,
        910,
        &format!("{base}/img.png"),
    )))
    .await
    .unwrap();

    let text = wait_message_text(&state, 777).await;
    assert!(
        text.contains("橘猫"),
        "外链被拒后应自动改用内联 base64: {text}"
    );
}
