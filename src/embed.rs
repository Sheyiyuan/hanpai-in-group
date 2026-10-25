//! 向量客户端。
//!
//! 复用 `[providers.*]`：`embedding.model = "提供商:模型"`，任何 OpenAI 兼容的
//! `/embeddings` 端点都行（Ollama、vLLM、官方 API…）。
//!
//! 只允许配一个：换 embedding 模型意味着换向量空间，历史向量会全部失效，
//! 所以这里不做多提供商降级——降级只能降到「不用向量」。

use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::json;
use tracing::warn;

use crate::config::Config;

pub struct Embedder {
    inner: Option<Inner>,
    dim: usize,
}

struct Inner {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
}

impl Embedder {
    pub fn from_config(cfg: &Config) -> Result<Self> {
        let dim = cfg.embedding.dim.max(1) as usize;
        if !cfg.embedding.enabled {
            return Ok(Self::disabled());
        }

        let Some(def) = cfg.models.get(&cfg.embedding.model) else {
            warn!(
                "embedding.model 引用了未声明的模型 {}，向量功能关闭",
                cfg.embedding.model
            );
            return Ok(Self::disabled());
        };
        let provider_name = def.provider.clone();
        let Some(provider) = cfg.providers.get(&provider_name) else {
            warn!("embedding.model 指向未定义的提供商 {provider_name}，向量功能关闭");
            return Ok(Self::disabled());
        };

        let api_key = provider.api_key();
        if !provider.api_key_env.trim().is_empty() && api_key.is_none() {
            warn!(
                "提供商 {provider_name} 的密钥环境变量 {} 未设置，向量功能关闭",
                provider.api_key_env
            );
            return Ok(Self::disabled());
        }

        Ok(Self {
            inner: Some(Inner {
                http: build_http()?,
                base_url: crate::llm::normalize_base(&provider.base_url),
                api_key,
                model: def.model.clone(),
            }),
            dim,
        })
    }

    /// 关闭向量，检索降级为关键词
    pub fn disabled() -> Self {
        Self {
            inner: None,
            dim: 1,
        }
    }

    pub fn available(&self) -> bool {
        self.inner.is_some()
    }

    pub fn dim(&self) -> usize {
        self.dim
    }

    /// 批量取向量。失败时返回错误，由调用方决定降级策略。
    pub async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let Some(inner) = &self.inner else {
            bail!("向量功能未启用");
        };
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let url = format!("{}/embeddings", inner.base_url.trim_end_matches('/'));
        let mut req = inner
            .http
            .post(&url)
            .json(&json!({ "model": inner.model, "input": texts }));
        if let Some(key) = &inner.api_key {
            req = req.bearer_auth(key);
        }

        let resp = req.send().await.context("请求 embedding 接口失败")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!(
                "embedding 接口返回 HTTP {status}: {}",
                crate::llm::truncate(&text, 300)
            );
        }

        let value: serde_json::Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "embedding 返回不是 JSON: {}",
                crate::llm::truncate(&text, 200)
            )
        })?;
        let data = value["data"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("embedding 返回缺少 data 字段"))?;

        let mut out = Vec::with_capacity(data.len());
        for item in data {
            let arr = item["embedding"]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("embedding 缺少向量"))?;
            let vec: Vec<f32> = arr
                .iter()
                .map(|v| v.as_f64().unwrap_or(0.0) as f32)
                .collect();
            out.push(vec);
        }
        if out.len() != texts.len() {
            bail!(
                "embedding 返回条数不匹配：请求 {} 条，返回 {} 条",
                texts.len(),
                out.len()
            );
        }
        Ok(out)
    }
}

fn build_http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("创建 embedding HTTP 客户端失败")
}

/// 余弦相似度。长度不一致或零向量返回 0。
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= f32::EPSILON || nb <= f32::EPSILON {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

pub fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

pub fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_identical() {
        let v = vec![1.0, 2.0, 3.0];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_orthogonal() {
        assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    }

    #[test]
    fn cosine_zero_vector() {
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
    }

    #[test]
    fn blob_roundtrip() {
        let v = vec![0.5, -1.25, 3.0];
        assert_eq!(blob_to_vec(&vec_to_blob(&v)), v);
    }

    #[test]
    fn disabled_when_not_enabled() {
        let cfg = Config::default();
        assert!(!Embedder::from_config(&cfg).unwrap().available());
    }
}
