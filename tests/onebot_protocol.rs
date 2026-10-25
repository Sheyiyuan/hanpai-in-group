//! OneBot 协议层的边界用例：字符串消息、缺 message_id、@ 判定、引用回复、
//! notice 不入库、自身消息过滤、心跳超时、鉴权 fail-closed、retcode 处理。

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hanpai_in_group::config::Config;
use hanpai_in_group::state::AppState;
use hanpai_in_group::{db, router};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Harness {
    state: Arc<AppState>,
    url: String,
    _dir: tempfile::TempDir,
}

async fn setup_with(configure: impl FnOnce(&mut Config)) -> Harness {
    let dir = tempfile::tempdir().expect("临时目录");
    let pool = db::connect(&dir.path().join("t.db")).await.expect("db");
    db::migrate(&pool).await.expect("migrate");

    let mut cfg = Config::default();
    cfg.groups.whitelist = vec![111];
    cfg.groups.dry_run = true;
    cfg.admin.qq = vec![9999];
    cfg.bot.qq = 10001;
    cfg.onebot.access_token_env = None;
    configure(&mut cfg);

    let state = AppState::new(cfg, pool, dir.path().join("config.toml"));
    let app = router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Harness {
        state,
        url: format!("ws://{addr}/onebot"),
        _dir: dir,
    }
}

async fn setup() -> Harness {
    setup_with(|_| {}).await
}

async fn connect(url: &str) -> Ws {
    let (ws, _) = tokio_tungstenite::connect_async(url).await.expect("连接");
    ws
}

async fn connect_with_role(url: &str, role: &str) -> Ws {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("x-client-role", role.parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws
}

fn group_event(group_id: i64, user_id: i64, msg_id: Option<i64>, text: &str) -> String {
    let mut v = json!({
        "post_type": "message",
        "message_type": "group",
        "sub_type": "normal",
        "time": 1_760_000_000i64,
        "self_id": 10001,
        "group_id": group_id,
        "user_id": user_id,
        "raw_message": text,
        "message": [{ "type": "text", "data": { "text": text } }],
        "sender": { "user_id": user_id, "nickname": "群友", "card": "" }
    });
    if let Some(id) = msg_id {
        v["message_id"] = json!(id);
    }
    v.to_string()
}

async fn wait(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

async fn count(h: &Harness) -> i64 {
    db::count_since(&h.state.db, 111, 0).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn string_format_message_is_stored() {
    let h = setup().await;
    let mut ws = connect(&h.url).await;

    let mut v: Value = serde_json::from_str(&group_event(111, 2, Some(9001), "你好")).unwrap();
    // OneBot 规范默认 message_format=string
    v["message"] = json!("你好 [CQ:at,qq=10001]");
    v["raw_message"] = json!("你好 [CQ:at,qq=10001]");
    ws.send(Message::Text(v.to_string())).await.unwrap();
    wait(400).await;

    assert_eq!(count(&h).await, 1, "字符串格式的消息不应被丢弃");
    let row: (String, i64) =
        sqlx::query_as("SELECT text, addressed FROM messages WHERE msg_id = 9001")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(row.0, "你好 @10001");
    assert_eq!(row.1, 1, "字符串里的 CQ:at 也应被识别为 addressed");
}

#[tokio::test(flavor = "multi_thread")]
async fn at_does_not_match_longer_qq() {
    let h = setup().await;
    let mut ws = connect(&h.url).await;

    // @100010 不是机器人（10001），不应被当成 at
    let v = json!({
        "post_type": "message", "message_type": "group", "sub_type": "normal",
        "time": 1_760_000_000i64, "self_id": 10001, "group_id": 111, "user_id": 2,
        "message_id": 9002, "raw_message": "@100010 在吗",
        "message": [{ "type": "text", "data": { "text": "@100010 在吗" } }],
        "sender": { "nickname": "群友", "card": "" }
    });
    ws.send(Message::Text(v.to_string())).await.unwrap();
    wait(400).await;

    let (addressed,): (i64,) = sqlx::query_as("SELECT addressed FROM messages WHERE msg_id = 9002")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(addressed, 0, "子串匹配会把 @100010 误判成 @10001");
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_message_id_is_dropped() {
    let h = setup().await;
    let mut ws = connect(&h.url).await;

    ws.send(Message::Text(group_event(111, 2, None, "没有 id")))
        .await
        .unwrap();
    ws.send(Message::Text(group_event(111, 2, None, "还没有 id")))
        .await
        .unwrap();
    wait(400).await;

    assert_eq!(
        count(&h).await,
        0,
        "缺 message_id 的消息应被丢弃，而不是都落成 0"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bot_own_message_is_ignored() {
    let h = setup().await;
    let mut ws = connect(&h.url).await;

    ws.send(Message::Text(group_event(111, 10001, Some(9003), "我自己")))
        .await
        .unwrap();
    wait(400).await;

    assert_eq!(count(&h).await, 0, "机器人自己的消息不应被当群友消息入库");
}

#[tokio::test(flavor = "multi_thread")]
async fn notice_is_not_stored() {
    let h = setup().await;
    let mut ws = connect(&h.url).await;

    ws.send(Message::Text(
        json!({
            "post_type": "notice", "notice_type": "group_recall",
            "time": 1_760_000_000i64, "self_id": 10001, "group_id": 111,
            "user_id": 2, "operator_id": 2, "message_id": 9004
        })
        .to_string(),
    ))
    .await
    .unwrap();
    wait(300).await;

    assert_eq!(count(&h).await, 0, "notice 只入日志，不写业务表");
}

#[tokio::test(flavor = "multi_thread")]
async fn reply_to_bot_sets_addressed() {
    let h = setup().await;
    let mut ws = connect(&h.url).await;

    // 假装机器人之前发过一条消息（真实链路由 reply::deliver 回写）
    db::insert_message(
        &h.state.db,
        &db::NewMessage {
            msg_id: 4242,
            group_id: 111,
            user_id: 10001,
            nickname: "半拍",
            card: "",
            ts: 1_760_000_000,
            text: "我先说一句",
            segments: "[]",
            reply_to: None,
            addressed: false,
        },
        600,
    )
    .await
    .unwrap();

    ws.send(Message::Text(
        json!({
            "post_type": "message", "message_type": "group", "sub_type": "normal",
            "time": 1_760_000_000i64, "self_id": 10001, "group_id": 111, "user_id": 2,
            "message_id": 9005, "raw_message": "这是啥",
            "message": [
                { "type": "reply", "data": { "id": "4242" } },
                { "type": "text", "data": { "text": "这是啥" } }
            ],
            "sender": { "nickname": "群友", "card": "" }
        })
        .to_string(),
    ))
    .await
    .unwrap();
    wait(500).await;

    let (addressed, reply_to): (i64, Option<i64>) =
        sqlx::query_as("SELECT addressed, reply_to FROM messages WHERE msg_id = 9005")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(reply_to, Some(4242));
    assert_eq!(addressed, 1, "引用机器人的消息应视为 addressed");

    let decision = db::last_decision(&h.state.db, 111)
        .await
        .unwrap()
        .expect("应有决策记录");
    assert_eq!(decision.trigger, "reply");
}

#[tokio::test(flavor = "multi_thread")]
async fn x_self_id_header_is_used() {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let h = setup().await;
    let mut req = h.url.as_str().into_client_request().unwrap();
    req.headers_mut()
        .insert("x-self-id", "12345".parse().unwrap());
    req.headers_mut()
        .insert("x-client-role", "Universal".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    assert_eq!(
        h.state.self_id.load(std::sync::atomic::Ordering::Relaxed),
        12345,
        "握手头 X-Self-ID 应立即成为 self_id，不必等 meta_event"
    );

    ws.send(Message::Text(group_event(111, 2, Some(9006), "hi")))
        .await
        .unwrap();
    wait(400).await;
    assert_eq!(count(&h).await, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_missing_env_rejects() {
    std::env::remove_var("HANPAI_TEST_MISSING_TOKEN");
    let h = setup_with(|cfg| {
        cfg.onebot.access_token_env = Some("HANPAI_TEST_MISSING_TOKEN".into());
    })
    .await;

    let res = tokio_tungstenite::connect_async(h.url.as_str()).await;
    assert!(res.is_err(), "配了 token 但环境变量缺失时必须拒绝连接");
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_reports_api_failure() {
    let h = setup().await;
    let mut ws = connect(&h.url).await;

    ws.send(Message::Text(group_event(111, 9999, Some(9007), "/ping")))
        .await
        .unwrap();

    // get_login_info 返回失败，随后机器人应发出 send_group_msg
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(left, ws.next())
            .await
            .expect("等待回复超时")
            .expect("连接关闭")
            .expect("ws 错误");
        let Message::Text(text) = frame else { continue };
        let v: Value = serde_json::from_str(text.as_str()).unwrap();
        let Some(action) = v.get("action").and_then(Value::as_str) else {
            continue;
        };
        if action == "get_login_info" {
            let ack = json!({
                "status": "failed", "retcode": 100, "wording": "not logged in",
                "echo": v["echo"].clone()
            });
            ws.send(Message::Text(ack.to_string())).await.unwrap();
        } else if action == "send_group_msg" {
            let text = v["params"]["message"][0]["data"]["text"]
                .as_str()
                .unwrap_or("");
            assert!(
                text.contains("不通"),
                "retcode 非 0 时 /ping 不能报 pong，实际: {text}"
            );
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_timeout_closes_stale_connection() {
    let h = setup_with(|cfg| cfg.onebot.heartbeat_timeout_secs = 1).await;
    let mut ws = connect(&h.url).await;

    ws.send(Message::Text(
        json!({
            "post_type": "meta_event", "meta_event_type": "heartbeat",
            "time": 1_760_000_000i64, "self_id": 10001, "interval": 1000,
            "status": { "online": true, "good": true }
        })
        .to_string(),
    ))
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(left, ws.next()).await {
            Ok(Some(Ok(Message::Text(_)))) => continue,
            // Close / Ping / Pong / Binary：连接已被服务端收掉
            Ok(Some(Ok(_))) => break,
            Ok(Some(Err(_))) => break,
            Ok(None) => break,
            // 超时还没断开 = 看门狗没生效
            Err(_) => panic!("心跳超时后连接应在 5s 内被服务端断开"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn event_role_does_not_take_over_api() {
    let h = setup().await;
    // Universal 先连上并注册 API 通道
    let mut api = connect_with_role(&h.url, "Universal").await;
    // Event 专用连接后连上：它只能推事件，不能抢走 action 通道
    let mut event = connect_with_role(&h.url, "Event").await;

    event
        .send(Message::Text(group_event(111, 9999, Some(9008), "/ping")))
        .await
        .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(left, api.next())
            .await
            .expect("action 应走 Universal 连接，而不是被 Event 连接抢走")
            .expect("连接关闭")
            .expect("ws 错误");
        let Message::Text(text) = frame else { continue };
        let v: Value = serde_json::from_str(text.as_str()).unwrap();
        if v.get("action").and_then(Value::as_str) == Some("get_login_info") {
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dedup_window_allows_id_reuse() {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect(&dir.path().join("d.db")).await.unwrap();
    db::migrate(&pool).await.unwrap();

    let first = db::NewMessage {
        msg_id: 77,
        group_id: 1,
        user_id: 2,
        nickname: "a",
        card: "",
        ts: 1000,
        text: "第一条",
        segments: "[]",
        reply_to: None,
        addressed: false,
    };
    assert!(db::insert_message(&pool, &first, 600).await.unwrap());
    assert!(
        !db::insert_message(&pool, &first, 600).await.unwrap(),
        "窗口内同一 id 应判重复"
    );

    let later = db::NewMessage {
        ts: 1000 + 601,
        text: "窗口外的新消息",
        ..first
    };
    assert!(
        db::insert_message(&pool, &later, 600).await.unwrap(),
        "窗口外允许同一 id 重新入库"
    );
    assert_eq!(db::message_sender(&pool, 77).await.unwrap(), Some(2));
}
