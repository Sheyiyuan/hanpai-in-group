//! P0 验收：白名单闸门、去重、管理命令、鉴权。
//!
//! 用真实 TCP + 真实 SQLite 跑端到端，协议端用 mock WS 客户端扮演 NapCat。

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hanpai_in_group::config::Config;
use hanpai_in_group::state::AppState;
use hanpai_in_group::{db, router};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

struct Harness {
    state: Arc<AppState>,
    url: String,
    _dir: tempfile::TempDir,
}

async fn setup(whitelist: Vec<i64>, token_env: Option<&str>) -> Harness {
    let dir = tempfile::tempdir().expect("创建临时目录");
    let db_path = dir.path().join("test.db");
    let pool = db::connect(&db_path).await.expect("打开测试数据库");
    db::migrate(&pool).await.expect("迁移");

    let mut cfg = Config::default();
    cfg.groups.whitelist = whitelist;
    cfg.groups.dry_run = true;
    cfg.admin.qq = vec![9999];
    cfg.onebot.access_token_env = token_env.map(|s| s.to_string());

    let config_path = dir.path().join("config.toml");
    let state = AppState::new(cfg, pool, config_path);

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
        "sender": { "user_id": user_id, "nickname": "测试用户", "card": "" }
    })
    .to_string()
}

async fn send_event(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    payload: String,
) {
    ws.send(Message::Text(payload)).await.expect("发送事件");
}

/// 一边喂 mock 回包，一边等机器人发出 send_group_msg
async fn expect_send_group_msg(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let frame = tokio::time::timeout(left, ws.next())
            .await
            .expect("等待机器人回包超时")
            .expect("连接已关闭")
            .expect("ws 读错误");
        let Message::Text(text) = frame else {
            continue;
        };
        let v: Value = serde_json::from_str(text.as_str()).unwrap();

        // 先看是不是机器人发起的 action；是就应答它的 echo
        if let Some(action) = v.get("action").and_then(|a| a.as_str()) {
            if action == "send_group_msg" {
                return v;
            }
            if let Some(echo) = v.get("echo").cloned() {
                let ack = json!({ "status": "ok", "retcode": 0, "data": {}, "echo": echo });
                ws.send(Message::Text(ack.to_string())).await.unwrap();
            }
            continue;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn whitelist_gate_blocks_writes() {
    let h = setup(vec![111], None).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(h.url.as_str())
        .await
        .unwrap();

    send_event(&mut ws, group_event(111, 1, 1001, "白名单群的消息")).await;
    send_event(&mut ws, group_event(222, 2, 1002, "非白名单群的消息")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(db::count_since(&h.state.db, 111, 0).await.unwrap(), 1);
    assert_eq!(
        db::count_since(&h.state.db, 222, 0).await.unwrap(),
        0,
        "非白名单群必须零写入"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_message_ignored() {
    let h = setup(vec![111], None).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(h.url.as_str())
        .await
        .unwrap();

    send_event(&mut ws, group_event(111, 1, 2001, "同一条")).await;
    send_event(&mut ws, group_event(111, 1, 2001, "同一条")).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert_eq!(db::count_since(&h.state.db, 111, 0).await.unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_ping_gets_reply() {
    let h = setup(vec![111], None).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(h.url.as_str())
        .await
        .unwrap();

    send_event(&mut ws, group_event(111, 9999, 3001, "/ping")).await;

    let sent = expect_send_group_msg(&mut ws).await;
    let text = sent["params"]["message"][0]["data"]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(text.starts_with("pong"), "期望 pong 回复，实际: {text:?}");
    assert_eq!(sent["params"]["group_id"], 111);
}

#[tokio::test(flavor = "multi_thread")]
async fn non_admin_command_is_ignored() {
    let h = setup(vec![111], None).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(h.url.as_str())
        .await
        .unwrap();

    // 普通群友发的 /ping 不能触发回复
    send_event(&mut ws, group_event(111, 1, 4001, "/ping")).await;
    let res =
        tokio::time::timeout(Duration::from_millis(800), expect_send_group_msg(&mut ws)).await;
    assert!(res.is_err(), "非管理员不应收到回复");
}

#[tokio::test(flavor = "multi_thread")]
async fn unauthorized_connection_rejected() {
    std::env::set_var("TEST_ONEBOT_TOKEN", "s3cret");
    let h = setup(vec![111], Some("TEST_ONEBOT_TOKEN")).await;

    let res = tokio_tungstenite::connect_async(h.url.as_str()).await;
    assert!(res.is_err(), "缺 token 应被拒绝");
}
