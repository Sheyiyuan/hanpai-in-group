//! 思考模式参数：确认发给提供商的是 DeepSeek 文档里的格式。

use axum::routing::post;
use axum::{Json, Router};
use hanpai_in_group::config::{Config, ModelDef, Provider, Thinking};
use hanpai_in_group::llm::{LlmClient, Msg, Role};
use serde_json::{json, Value};

async fn echo_server() -> String {
    // 把收到的请求体原样塞进 content，测试再解析回来
    async fn echo(Json(body): Json<Value>) -> Json<Value> {
        Json(json!({
            "choices": [{ "message": { "role": "assistant", "content": body.to_string() } }]
        }))
    }

    let app = Router::new().route("/chat/completions", post(echo));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn client_with(base: &str, thinking: Thinking, effort: Option<&str>) -> LlmClient {
    std::env::set_var("TEST_LLM_KEY", "k");

    let mut cfg = Config::default();
    cfg.providers.insert(
        "mock".into(),
        Provider {
            base_url: base.to_string(),
            api_key_env: "TEST_LLM_KEY".into(),
        },
    );
    cfg.models.insert(
        "m".into(),
        ModelDef {
            provider: "mock".into(),
            model: "deepseek-v4.1-flash".into(),
            thinking,
            reasoning_effort: effort.map(str::to_string),
            ..Default::default()
        },
    );
    cfg.model.chat = vec!["m".into()];
    cfg.model.max_retries = 0;

    LlmClient::new(&cfg).unwrap().expect("应该有路由")
}

async fn sent_body(client: &LlmClient) -> Value {
    let raw = client
        .chat(Role::Chat, &[Msg::user("hi")], 0.7)
        .await
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

#[tokio::test]
async fn thinking_off_sends_disabled_and_temperature() {
    let base = echo_server().await;
    let client = client_with(&base, Thinking::Off, None).await;
    let body = sent_body(&client).await;

    assert_eq!(body["thinking"]["type"], "disabled");
    assert!(body.get("reasoning_effort").is_none());
    let temp = body["temperature"].as_f64().unwrap();
    assert!((temp - 0.7).abs() < 1e-5, "{temp}");
    assert_eq!(body["model"], "deepseek-v4.1-flash");
}

#[tokio::test]
async fn thinking_on_sends_enabled_with_effort_and_no_temperature() {
    let base = echo_server().await;
    let client = client_with(&base, Thinking::On, Some("high")).await;
    let body = sent_body(&client).await;

    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["reasoning_effort"], "high");
    assert!(
        body.get("temperature").is_none(),
        "思考模式下不应下发 temperature: {body}"
    );
}

#[tokio::test]
async fn auto_sends_nothing_and_keeps_temperature() {
    let base = echo_server().await;
    let client = client_with(&base, Thinking::Auto, None).await;
    let body = sent_body(&client).await;

    assert!(body.get("thinking").is_none(), "{body}");
    assert!(body.get("reasoning_effort").is_none(), "{body}");
    let temp = body["temperature"].as_f64().unwrap();
    assert!((temp - 0.7).abs() < 1e-5, "{temp}");
}

#[tokio::test]
async fn auto_still_sends_explicit_effort() {
    let base = echo_server().await;
    let client = client_with(&base, Thinking::Auto, Some("max")).await;
    let body = sent_body(&client).await;

    assert!(body.get("thinking").is_none());
    assert_eq!(body["reasoning_effort"], "max");
}
