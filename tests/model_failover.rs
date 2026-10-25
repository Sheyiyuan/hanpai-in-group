//! 多提供商路由：优先级、失败回退、密钥缺失跳过。

use axum::routing::post;
use axum::{Json, Router};
use hanpai_in_group::config::{Config, ModelDef, Provider};
use hanpai_in_group::llm::{LlmClient, Msg, Role};
use serde_json::{json, Value};

async fn mock_model() -> String {
    let app = Router::new().route("/chat/completions", post(chat));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

async fn chat(Json(body): Json<Value>) -> Json<Value> {
    let model = body["model"].as_str().unwrap_or("?");
    Json(json!({
        "choices": [{
            "message": { "role": "assistant", "content": format!("来自 mock:{model}") }
        }],
        "usage": { "prompt_tokens": 10, "completion_tokens": 5 }
    }))
}

fn provider(base_url: &str, key_env: &str) -> Provider {
    Provider {
        base_url: base_url.to_string(),
        api_key_env: key_env.to_string(),
    }
}

fn model(provider: &str, model: &str) -> ModelDef {
    ModelDef {
        provider: provider.to_string(),
        model: model.to_string(),
        ..Default::default()
    }
}

#[tokio::test]
async fn falls_back_to_second_provider() {
    std::env::set_var("TEST_LLM_KEY", "k");
    let mock = mock_model().await;

    let mut cfg = Config::default();
    // 1 端口必然连不上，用来模拟第一个提供商挂掉
    cfg.providers.insert(
        "dead".into(),
        provider("http://127.0.0.1:1", "TEST_LLM_KEY"),
    );
    cfg.providers
        .insert("mock".into(), provider(&mock, "TEST_LLM_KEY"));
    cfg.models.insert("dead".into(), model("dead", "x"));
    cfg.models.insert("live".into(), model("mock", "chat"));
    cfg.model.chat = vec!["dead".into(), "live".into()];
    cfg.model.max_retries = 0;

    let client = LlmClient::new(&cfg).unwrap().expect("应该有可用路由");
    let text = client
        .chat(Role::Chat, &[Msg::user("hi")], 0.0)
        .await
        .unwrap();
    assert_eq!(text, "来自 mock:chat");
    let desc = client.describe();
    assert!(desc.contains("dead(x) → live(chat)"), "{desc}");

    let usage = client.drain_usage();
    assert!(
        usage
            .iter()
            .any(|(role, u)| *role == Role::Chat && u.prompt == 10 && u.completion == 5),
        "应记录 token 用量: {usage:?}"
    );
}

#[tokio::test]
async fn skips_route_without_key() {
    std::env::set_var("TEST_LLM_KEY", "k");
    let mock = mock_model().await;

    let mut cfg = Config::default();
    cfg.providers
        .insert("nokey".into(), provider(&mock, "KEY_THAT_DOES_NOT_EXIST"));
    cfg.providers
        .insert("mock".into(), provider(&mock, "TEST_LLM_KEY"));
    cfg.models.insert("nokey".into(), model("nokey", "a"));
    cfg.models.insert("live".into(), model("mock", "chat"));
    cfg.model.chat = vec!["nokey".into(), "live".into()];
    cfg.model.max_retries = 0;

    let client = LlmClient::new(&cfg).unwrap().expect("应该有可用路由");
    let text = client
        .chat(Role::Chat, &[Msg::user("hi")], 0.0)
        .await
        .unwrap();
    assert_eq!(text, "来自 mock:chat");
    assert!(!client.describe().contains("nokey(a)"));
}

#[tokio::test]
async fn reports_all_routes_failed() {
    std::env::set_var("TEST_LLM_KEY", "k");

    let mut cfg = Config::default();
    cfg.providers.insert(
        "dead".into(),
        provider("http://127.0.0.1:1", "TEST_LLM_KEY"),
    );
    cfg.models.insert("dead".into(), model("dead", "x"));
    cfg.model.chat = vec!["dead".into()];
    cfg.model.planner = vec![];
    cfg.model.summary = vec![];
    cfg.model.max_retries = 0;

    let client = LlmClient::new(&cfg).unwrap().expect("路由存在但都不可达");
    let err = client
        .chat(Role::Chat, &[Msg::user("hi")], 0.0)
        .await
        .unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("所有模型路由都失败"), "{msg}");
}

#[test]
fn no_routes_means_no_client() {
    let mut cfg = Config::default();
    cfg.providers.clear();
    cfg.models.clear();
    cfg.model.chat.clear();
    assert!(LlmClient::new(&cfg).unwrap().is_none());
}
