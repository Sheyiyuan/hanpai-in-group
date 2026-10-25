//! OpenAI 兼容的多提供商客户端。
//!
//! 每个功能（chat / planner / summary / caption）是一串「提供商:模型」，按书写顺序尝试：
//! 前一个失败（连接、超时、5xx、限流、鉴权）就换下一个，全部失败才算失败。
//!
//! 支持多模态：路由上标了 `visual = true` 时，消息里的图片会以 content parts 发出去；
//! 没标的模型只收到文本（图片在上游已经被转述成文字了）。

use std::collections::HashMap;
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::config::{Config, Model, Thinking};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt: i64,
    pub completion: i64,
}

impl Usage {
    pub fn total(&self) -> i64 {
        self.prompt + self.completion
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Chat,
    Planner,
    Summary,
    Caption,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Chat => "chat",
            Role::Planner => "planner",
            Role::Summary => "summary",
            Role::Caption => "caption",
        }
    }

    pub fn specs(self, model: &Model) -> &[String] {
        match self {
            Role::Chat => &model.chat,
            Role::Planner => &model.planner,
            Role::Summary => &model.summary,
            Role::Caption => &model.caption,
        }
    }
}

/// 一张待发送的图片。要么给 url（模型自己去取），要么给 bytes（内联 base64）。
#[derive(Debug, Clone, Default)]
pub struct ImagePart {
    pub url: Option<String>,
    pub bytes: Option<Vec<u8>>,
    pub mime: String,
}

impl ImagePart {
    pub fn from_url(url: impl Into<String>) -> Self {
        Self {
            url: Some(url.into()),
            bytes: None,
            mime: "image/jpeg".into(),
        }
    }

    pub fn from_bytes(bytes: Vec<u8>, mime: impl Into<String>) -> Self {
        Self {
            url: None,
            bytes: Some(bytes),
            mime: mime.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Msg {
    pub role: String,
    pub content: String,
    pub images: Vec<ImagePart>,
}

impl Msg {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".into(),
            content: content.into(),
            images: Vec::new(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: content.into(),
            images: Vec::new(),
        }
    }

    pub fn with_image(mut self, image: ImagePart) -> Self {
        self.images.push(image);
        self
    }

    pub fn with_images(mut self, images: Vec<ImagePart>) -> Self {
        self.images.extend(images);
        self
    }
}

#[derive(Clone)]
struct Route {
    /// 配置里声明的模型名
    name: String,
    provider: String,
    model: String,
    base_url: String,
    api_key: Option<String>,
    max_retries: u32,
    visual: bool,
    thinking: Thinking,
    reasoning_effort: Option<String>,
    temperature: Option<f32>,
}

impl Route {
    fn label(&self) -> String {
        let mut label = format!("{}({})", self.name, self.model);
        if self.visual {
            label.push_str("·视觉");
        }
        if self.thinking != Thinking::Auto {
            label.push_str(if self.thinking == Thinking::On {
                "·思考"
            } else {
                "·不思考"
            });
        }
        label
    }
}

pub struct LlmClient {
    http: reqwest::Client,
    routes: HashMap<Role, Vec<Route>>,
    timeout_secs: u64,
    max_image_bytes: u64,
    /// 每个提供商吃不吃外链 URL；第一次 400 之后自动改成内联
    modes: RwLock<HashMap<String, InputMode>>,
    /// 按角色累计 token，由 maintenance 定时落库
    usage: Mutex<HashMap<Role, Usage>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Url,
    DataUri,
}

impl LlmClient {
    /// 一个能用的路由都没有时返回 None：不阻断启动，只是没脑子。
    pub fn new(cfg: &Config) -> Result<Option<Self>> {
        let timeout_secs = cfg.model.timeout_secs.max(1);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .build()
            .context("创建 HTTP 客户端失败")?;

        let mut warned = std::collections::HashSet::new();
        let chat = build_routes(cfg, Role::Chat, &mut warned);
        let mut routes = HashMap::new();
        routes.insert(Role::Chat, chat.clone());

        for role in [Role::Planner, Role::Summary, Role::Caption] {
            let mut list = build_routes(cfg, role, &mut warned);
            if list.is_empty() {
                debug!("model.{} 没有可用路由，复用 chat", role.as_str());
                list = chat.clone();
            }
            routes.insert(role, list);
        }

        if routes.values().all(|v| v.is_empty()) {
            warn!("没有任何可用的模型路由，模型功能关闭");
            return Ok(None);
        }

        let default_mode = if cfg.vision.image_input == "data_uri" {
            InputMode::DataUri
        } else {
            InputMode::Url
        };
        let modes = cfg
            .providers
            .keys()
            .map(|k| (k.clone(), default_mode))
            .collect();

        Ok(Some(Self {
            http,
            routes,
            timeout_secs,
            max_image_bytes: cfg.vision.max_bytes.max(1024),
            modes: RwLock::new(modes),
            usage: Mutex::new(HashMap::new()),
        }))
    }

    pub async fn chat(&self, role: Role, messages: &[Msg], temperature: f32) -> Result<String> {
        let routes = self
            .routes
            .get(&role)
            .filter(|r| !r.is_empty())
            .or_else(|| self.routes.get(&Role::Chat).filter(|r| !r.is_empty()))
            .ok_or_else(|| anyhow!("{} 没有可用的模型路由", role.as_str()))?;

        let mut errors: Vec<String> = Vec::new();
        for route in routes {
            match self.try_route(route, messages, temperature).await {
                Ok((text, usage)) => {
                    if let Some(u) = usage {
                        self.record_usage(role, u);
                    }
                    return Ok(text);
                }
                Err(e) => {
                    warn!("模型路由 {} 失败，换下一个: {e:#}", route.label());
                    errors.push(format!("{}：{e:#}", route.label()));
                }
            }
        }

        bail!("所有模型路由都失败：{}", errors.join(" ｜ "))
    }

    /// 让模型输出 JSON，并容忍它套一层 ```json 或者多说两句话。
    pub async fn chat_json(&self, role: Role, messages: &[Msg]) -> Result<Value> {
        let raw = self.chat(role, messages, 0.1).await?;
        extract_json(&raw)
    }

    fn record_usage(&self, role: Role, usage: Usage) {
        if let Ok(mut map) = self.usage.lock() {
            let entry = map.entry(role).or_default();
            entry.prompt += usage.prompt;
            entry.completion += usage.completion;
        }
    }

    /// 取走累计值，交给 maintenance 写库
    pub fn drain_usage(&self) -> Vec<(Role, Usage)> {
        let Ok(mut map) = self.usage.lock() else {
            return Vec::new();
        };
        let drained: Vec<(Role, Usage)> = map
            .iter()
            .filter(|(_, u)| u.total() > 0)
            .map(|(r, u)| (*r, *u))
            .collect();
        map.clear();
        drained
    }

    /// 给 /status 用：每个功能当前的路由链
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        for role in [Role::Chat, Role::Planner, Role::Summary, Role::Caption] {
            let Some(list) = self.routes.get(&role) else {
                continue;
            };
            let names: Vec<String> = list.iter().map(|r| r.label()).collect();
            parts.push(format!(
                "{}={}",
                role.as_str(),
                if names.is_empty() {
                    "无".to_string()
                } else {
                    names.join(" → ")
                }
            ));
        }
        parts.join("｜")
    }

    fn mode_of(&self, provider: &str) -> InputMode {
        self.modes
            .read()
            .ok()
            .and_then(|m| m.get(provider).copied())
            .unwrap_or(InputMode::Url)
    }

    fn set_mode(&self, provider: &str, mode: InputMode) {
        if let Ok(mut m) = self.modes.write() {
            m.insert(provider.to_string(), mode);
        }
    }

    async fn try_route(
        &self,
        route: &Route,
        messages: &[Msg],
        temperature: f32,
    ) -> Result<(String, Option<Usage>)> {
        let url = format!("{}/chat/completions", route.base_url);
        let has_images = messages.iter().any(|m| !m.images.is_empty());
        let mut attempt = 0u32;
        let mut forced_inline = false;

        loop {
            let rendered = self.render_messages(route, messages, forced_inline).await?;
            let mut body = json!({
                "model": route.model,
                "messages": rendered,
                "stream": false,
            });
            // 思考开关与强度：DeepSeek 用 thinking.type + reasoning_effort
            match route.thinking {
                Thinking::On => {
                    body["thinking"] = json!({ "type": "enabled" });
                    if let Some(effort) = &route.reasoning_effort {
                        body["reasoning_effort"] = json!(effort);
                    }
                }
                Thinking::Off => {
                    body["thinking"] = json!({ "type": "disabled" });
                }
                Thinking::Auto => {
                    if let Some(effort) = &route.reasoning_effort {
                        body["reasoning_effort"] = json!(effort);
                    }
                }
            }
            // 思考模式下 temperature 不生效，干脆不传
            if route.thinking != Thinking::On {
                body["temperature"] = json!(route.temperature.unwrap_or(temperature));
            }

            let mut req = self.http.post(&url).json(&body);
            if let Some(key) = &route.api_key {
                req = req.bearer_auth(key);
            }

            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();

                    // 多模态路由吃不下外链图片：改成内联再试一次
                    if status.as_u16() == 400
                        && route.visual
                        && has_images
                        && !forced_inline
                        && self.mode_of(&route.provider) == InputMode::Url
                    {
                        warn!(
                            "提供商 {} 拒绝外链图片（HTTP 400），改用内联 base64 重试一次",
                            route.provider
                        );
                        self.set_mode(&route.provider, InputMode::DataUri);
                        forced_inline = true;
                        continue;
                    }

                    if status.is_success() {
                        let value: Value = serde_json::from_str(&text).with_context(|| {
                            format!("模型返回不是 JSON: {}", truncate(&text, 300))
                        })?;
                        let content = value["choices"][0]["message"]["content"]
                            .as_str()
                            .ok_or_else(|| {
                                anyhow!("模型返回缺少 content: {}", truncate(&text, 300))
                            })?;
                        let usage = value.get("usage").map(|u| Usage {
                            prompt: u.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                            completion: u
                                .get("completion_tokens")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0),
                        });
                        return Ok((content.to_string(), usage));
                    }

                    let retryable = status.is_server_error() || status.as_u16() == 429;
                    if retryable && attempt < route.max_retries {
                        attempt += 1;
                        backoff(attempt).await;
                        continue;
                    }
                    bail!("HTTP {status}: {}", truncate(&text, 300));
                }
                Err(e) => {
                    if attempt < route.max_retries {
                        attempt += 1;
                        backoff(attempt).await;
                        continue;
                    }
                    return Err(e)
                        .with_context(|| format!("请求失败（超时 {}s）", self.timeout_secs));
                }
            }
        }
    }

    async fn render_messages(
        &self,
        route: &Route,
        messages: &[Msg],
        forced_inline: bool,
    ) -> Result<Vec<Value>> {
        let mode = if forced_inline {
            InputMode::DataUri
        } else {
            self.mode_of(&route.provider)
        };

        let mut out = Vec::with_capacity(messages.len());
        for msg in messages {
            if msg.images.is_empty() || !route.visual {
                out.push(json!({ "role": msg.role, "content": msg.content }));
                continue;
            }

            let mut parts = vec![json!({ "type": "text", "text": msg.content })];
            for image in &msg.images {
                let url = match mode {
                    InputMode::Url => match &image.url {
                        Some(u) => u.clone(),
                        None => self.data_uri(image).await?,
                    },
                    InputMode::DataUri => self.data_uri(image).await?,
                };
                parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": url }
                }));
            }
            out.push(json!({ "role": msg.role, "content": parts }));
        }
        Ok(out)
    }

    /// 图片 → data URI。没有字节就先下载（只在内存里，不落盘）。
    async fn data_uri(&self, image: &ImagePart) -> Result<String> {
        let bytes = match &image.bytes {
            Some(b) => b.clone(),
            None => {
                let url = image
                    .url
                    .as_ref()
                    .ok_or_else(|| anyhow!("图片既没有 url 也没有内容"))?;
                let resp = self.http.get(url).send().await.context("下载图片失败")?;
                if !resp.status().is_success() {
                    bail!("下载图片失败：HTTP {}", resp.status());
                }
                let bytes = resp.bytes().await.context("读取图片失败")?;
                if bytes.len() as u64 > self.max_image_bytes {
                    bail!("图片太大：{} 字节", bytes.len());
                }
                bytes.to_vec()
            }
        };
        let mime = if image.mime.is_empty() {
            "image/jpeg"
        } else {
            &image.mime
        };
        Ok(format!(
            "data:{mime};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ))
    }
}

fn build_routes(
    cfg: &Config,
    role: Role,
    warned: &mut std::collections::HashSet<String>,
) -> Vec<Route> {
    let mut out = Vec::new();
    for name in role.specs(&cfg.model) {
        let Some(def) = cfg.models.get(name) else {
            warn!(
                "model.{} 里的「{name}」没有在 [models.*] 里声明，已跳过",
                role.as_str()
            );
            continue;
        };
        let Some(provider) = cfg.providers.get(&def.provider) else {
            warn!("模型 {name} 引用了未定义的提供商 {}，已跳过", def.provider);
            continue;
        };

        let needs_key = !provider.api_key_env.trim().is_empty();
        let api_key = provider.api_key();
        if needs_key && api_key.is_none() {
            if warned.insert(def.provider.clone()) {
                warn!(
                    "提供商 {} 的密钥环境变量 {} 未设置，跳过该路由",
                    def.provider, provider.api_key_env
                );
            }
            continue;
        }

        out.push(Route {
            name: name.clone(),
            provider: def.provider.clone(),
            model: def.model.clone(),
            base_url: normalize_base(&provider.base_url),
            api_key,
            max_retries: cfg.model.max_retries,
            visual: def.visual,
            thinking: def.thinking,
            reasoning_effort: def.reasoning_effort.clone(),
            temperature: def.temperature,
        });
    }
    out
}

pub fn normalize_base(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

pub fn extract_json(raw: &str) -> Result<Value> {
    let text = raw
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    if let Ok(v) = serde_json::from_str::<Value>(text) {
        return Ok(v);
    }

    let start = text
        .find('{')
        .ok_or_else(|| anyhow!("回复里没有 JSON: {}", truncate(text, 200)))?;
    let end = text
        .rfind('}')
        .ok_or_else(|| anyhow!("回复里没有完整的 JSON: {}", truncate(text, 200)))?;
    if end <= start {
        bail!("JSON 结构不完整: {}", truncate(text, 200));
    }
    serde_json::from_str(&text[start..=end])
        .with_context(|| format!("解析 JSON 失败: {}", truncate(&text[start..=end], 300)))
}

async fn backoff(attempt: u32) {
    let ms = 400u64.saturating_mul(1 << attempt.min(5));
    tokio::time::sleep(Duration::from_millis(ms.min(8_000))).await;
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect::<String>() + "…"
    }
}

/// 从多模态回复里提取纯文本（有些模型返回数组形式）。
pub fn content_text(value: &Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    let arr = value.as_array()?;
    let mut out = String::new();
    for part in arr {
        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
            out.push_str(t);
        }
    }
    Some(out)
}

/// 图片描述的结构。字段名与提示词一致。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Caption {
    #[serde(default)]
    pub caption: String,
    #[serde(default)]
    pub ocr: String,
    #[serde(default)]
    pub emotion: String,
    #[serde(default)]
    pub reusable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_plain() {
        let v = extract_json(r#"{"facts":[]}"#).unwrap();
        assert_eq!(v["facts"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn json_with_fence() {
        let v = extract_json("```json\n{\"facts\":[{\"content\":\"a\"}]}\n```").unwrap();
        assert_eq!(v["facts"][0]["content"], "a");
    }

    #[test]
    fn json_with_chatter() {
        let v = extract_json("好的，结果如下：\n{\"facts\":[]}\n希望有帮助").unwrap();
        assert!(v["facts"].is_array());
    }

    #[test]
    fn json_broken_errors() {
        assert!(extract_json("完全没有 JSON").is_err());
    }

    #[test]
    fn base_url_normalized() {
        assert_eq!(normalize_base("https://x.com/v1/"), "https://x.com/v1");
    }

    #[test]
    fn role_specs_pick_right_list() {
        let model = Model {
            chat: vec!["a".into()],
            planner: vec!["b".into()],
            summary: vec!["c".into()],
            caption: vec!["d".into()],
            ..Default::default()
        };
        assert_eq!(Role::Chat.specs(&model)[0], "a");
        assert_eq!(Role::Planner.specs(&model)[0], "b");
        assert_eq!(Role::Summary.specs(&model)[0], "c");
        assert_eq!(Role::Caption.specs(&model)[0], "d");
    }

    #[test]
    fn caption_text_extraction() {
        let v = json!([{ "type": "text", "text": "你好" }, { "type": "text", "text": "世界" }]);
        assert_eq!(content_text(&v).as_deref(), Some("你好世界"));
    }
}
