//! OneBot action 调用。
//!
//! 反向 WS 下，事件从 NapCat 推过来；调 API 则往同一条连接里发 `{action, params, echo}`，
//! 按 echo 对应回包。这里用 pending map + oneshot 做请求/响应配对。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, bail, Result};
use arc_swap::ArcSwapOption;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{timeout, Duration};

pub struct OneBotApi {
    conn: ArcSwapOption<mpsc::UnboundedSender<String>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>,
    timeout_secs: AtomicU64,
    conn_id: AtomicU64,
    seq: AtomicU64,
}

impl OneBotApi {
    pub fn new(timeout_secs: u64) -> Self {
        Self {
            conn: ArcSwapOption::empty(),
            pending: Arc::new(Mutex::new(HashMap::new())),
            timeout_secs: AtomicU64::new(timeout_secs),
            conn_id: AtomicU64::new(0),
            seq: AtomicU64::new(1),
        }
    }

    pub fn set_timeout(&self, secs: u64) {
        self.timeout_secs.store(secs, Ordering::Relaxed);
    }

    pub fn is_connected(&self) -> bool {
        self.conn.load().is_some()
    }

    /// 新连接接入，返回连接 id（用于断开时只清理自己那条）
    pub fn attach(&self, tx: mpsc::UnboundedSender<String>) -> u64 {
        let id = self.conn_id.fetch_add(1, Ordering::SeqCst) + 1;
        self.conn.store(Some(Arc::new(tx)));
        id
    }

    pub async fn detach(&self, id: u64) {
        if self.conn_id.load(Ordering::SeqCst) == id {
            self.conn.store(None);
            self.pending.lock().await.clear();
        }
    }

    /// 收到带 echo 的回包。返回是否命中了等待中的调用。
    pub async fn complete(&self, echo: &str, value: Value) -> bool {
        if let Some(tx) = self.pending.lock().await.remove(echo) {
            let _ = tx.send(value);
            true
        } else {
            false
        }
    }

    pub async fn call(&self, action: &str, params: Value) -> Result<Value> {
        let Some(tx) = self.conn.load_full() else {
            bail!("OneBot 未连接");
        };
        let echo = format!("e{}", self.seq.fetch_add(1, Ordering::Relaxed));
        let (otx, orx) = oneshot::channel();
        self.pending.lock().await.insert(echo.clone(), otx);

        let frame = json!({ "action": action, "params": params, "echo": echo }).to_string();
        if tx.send(frame).is_err() {
            self.pending.lock().await.remove(&echo);
            bail!("OneBot 连接已断开");
        }

        let secs = self.timeout_secs.load(Ordering::Relaxed).max(1);
        let value = match timeout(Duration::from_secs(secs), orx).await {
            Ok(Ok(v)) => v,
            Ok(Err(_)) => return Err(anyhow!("OneBot 响应通道已关闭")),
            Err(_) => {
                self.pending.lock().await.remove(&echo);
                return Err(anyhow!("OneBot action `{action}` 超时（{secs}s）"));
            }
        };
        ensure_success(action, &value)?;
        Ok(value)
    }

    /// 发送任意消息段（文本 + 图片等）
    pub async fn send_group_segments(&self, group_id: i64, message: Vec<Value>) -> Result<Value> {
        self.call(
            "send_group_msg",
            json!({ "group_id": group_id, "message": message }),
        )
        .await
    }

    pub async fn send_group_msg(&self, group_id: i64, text: &str) -> Result<Value> {
        self.call(
            "send_group_msg",
            json!({
                "group_id": group_id,
                // 用消息段数组而不是字符串：天然不解析 CQ 码，不需要自建转义。
                "message": [{ "type": "text", "data": { "text": text } }]
            }),
        )
        .await
    }
}

/// OneBot 的响应里 `retcode != 0` 表示动作失败；`status = async` 是异步调用，算成功。
fn ensure_success(action: &str, value: &Value) -> Result<()> {
    let retcode = value.get("retcode").and_then(Value::as_i64).unwrap_or(0);
    let status = value.get("status").and_then(Value::as_str).unwrap_or("ok");
    let failed = retcode != 0 || status.eq_ignore_ascii_case("failed");
    if !failed || status.eq_ignore_ascii_case("async") {
        return Ok(());
    }
    let detail = value
        .get("wording")
        .or_else(|| value.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("");
    bail!("OneBot `{action}` 调用失败：retcode={retcode} status={status} {detail}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_response_passes() {
        assert!(ensure_success("x", &json!({"status":"ok","retcode":0,"data":{}})).is_ok());
        // 缺字段按成功处理，兼容精简实现
        assert!(ensure_success("x", &json!({"data":{}})).is_ok());
    }

    #[test]
    fn failed_retcode_is_error() {
        let e = ensure_success(
            "send_group_msg",
            &json!({"status":"failed","retcode":100,"wording":"bad"}),
        )
        .unwrap_err();
        assert!(e.to_string().contains("retcode=100"));
    }

    #[test]
    fn failed_status_is_error() {
        let e = ensure_success("x", &json!({"status":"failed","retcode":0})).unwrap_err();
        assert!(e.to_string().contains("status=failed"));
    }

    #[test]
    fn async_status_is_success() {
        assert!(ensure_success("x", &json!({"status":"async","retcode":1})).is_ok());
    }
}
