//! 反向 WebSocket 服务端：鉴权、连接角色、心跳、事件队列、echo 回包。
//!
//! 这一层刻意不依赖任何业务状态：
//! - 调用方用 [`Server::accept`] 接一条连接，拿到的 [`Connection`] 用 `recv()` 收事件；
//! - 读循环、写循环、心跳看门狗都在后台任务里跑，调用方处理事件再慢也不会堵住 action 回包；
//! - 发 action 用 [`Server::api`] 拿到的 [`OneBotApi`]。
//!
//! 目前只绑定 axum 的 `WebSocket` 类型；事件与 CQ 码解析（[`super::event`]）和
//! action 调用（[`super::api`]）本身与框架无关，可以单独复用。

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tracing::{debug, info, warn};

use super::api::OneBotApi;
use super::event::Incoming;

/// 事件消费队列默认上限。溢出丢弃并告警，绝不阻塞读循环。
pub const DEFAULT_QUEUE_CAPACITY: usize = 5000;

/// 反向 WS 连接的角色。缺省按 Universal 处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    Api,
    Event,
    #[default]
    Universal,
}

impl Role {
    pub fn from_header(value: Option<&str>) -> Role {
        match value.unwrap_or("").trim().to_ascii_lowercase().as_str() {
            "api" => Role::Api,
            "event" => Role::Event,
            _ => Role::Universal,
        }
    }

    /// 这条连接是否可以用来调 API（Event 专用连接不行）。
    pub fn handles_api(self) -> bool {
        !matches!(self, Role::Event)
    }
}

/// 握手阶段从 HTTP 头里能拿到的连接信息。
#[derive(Debug, Clone, Copy, Default)]
pub struct ConnMeta {
    /// 反向 WS 规范里的 `X-Self-ID`
    pub self_id: Option<i64>,
    pub role: Role,
}

impl ConnMeta {
    pub fn new(self_id: Option<i64>, role: Role) -> Self {
        Self { self_id, role }
    }

    /// 从 `X-Self-ID` / `X-Client-Role` 两个头的原始值解析。
    pub fn from_headers(self_id: Option<&str>, role: Option<&str>) -> Self {
        Self {
            self_id: self_id
                .and_then(|s| s.trim().parse::<i64>().ok())
                .filter(|id| *id > 0),
            role: Role::from_header(role),
        }
    }
}

/// 服务端配置。与业务配置解耦，方便整体搬到别的项目。
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// 单次 action 调用的超时
    pub action_timeout: Duration,
    /// 收到过心跳后，多久没有任何帧就判定连接已死；None = 不检测
    pub heartbeat_timeout: Option<Duration>,
    /// 事件消费队列上限
    pub queue_capacity: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            action_timeout: Duration::from_secs(10),
            heartbeat_timeout: Some(Duration::from_secs(30)),
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
        }
    }
}

/// 反向 WS 服务端。持有一个 action 客户端和连接配置；配置可热更新。
pub struct Server {
    api: Arc<OneBotApi>,
    cfg: RwLock<ServerConfig>,
}

impl Server {
    pub fn new(cfg: ServerConfig) -> Arc<Self> {
        let api = Arc::new(OneBotApi::new(cfg.action_timeout.as_secs().max(1)));
        Arc::new(Self {
            api,
            cfg: RwLock::new(cfg),
        })
    }

    /// 与 `accept` 共用同一个 action 客户端，业务层直接用它发消息。
    pub fn api(&self) -> Arc<OneBotApi> {
        self.api.clone()
    }

    pub fn is_connected(&self) -> bool {
        self.api.is_connected()
    }

    /// 热更新配置（action 超时立即生效；心跳/队列参数只影响之后接入的连接）。
    pub fn configure(&self, cfg: ServerConfig) {
        self.api.set_timeout(cfg.action_timeout.as_secs().max(1));
        *self.cfg.write().unwrap_or_else(|e| e.into_inner()) = cfg;
    }

    pub fn config(&self) -> ServerConfig {
        self.cfg.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// 接一条已经升级完成的 WebSocket。返回的连接在 `recv()` 返回 `None` 后
    /// 应调用 [`Connection::close`] 收尾（或直接 drop，drop 会兜底清理）。
    pub fn accept(self: &Arc<Self>, socket: WebSocket, meta: ConnMeta) -> Connection {
        let cfg = self.config();
        let (mut sink, mut stream) = socket.split();

        // 出站：action 帧从 API 客户端流到这里
        let (action_tx, mut action_rx) = mpsc::unbounded_channel::<String>();
        let conn_id = if meta.role.handles_api() {
            Some(self.api.attach(action_tx))
        } else {
            drop(action_tx);
            None
        };
        let writer = tokio::spawn(async move {
            while let Some(payload) = action_rx.recv().await {
                if sink.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
            }
        });
        info!("OneBot 已接入（conn={conn_id:?}, role={:?}）", meta.role);

        // 事件队列：读循环只做 try_send，消费速度由调用方决定
        let (ev_tx, ev_rx) = mpsc::channel::<Incoming>(cfg.queue_capacity.max(1));
        let api = self.api.clone();
        let hb_seen = Arc::new(AtomicBool::new(false));
        let last_seen = Arc::new(AtomicI64::new(now_ms()));

        // 心跳看门狗：只在收到过心跳后才开始计时，没开心跳的安静连接不会被误杀
        let heartbeat = cfg.heartbeat_timeout.filter(|d| !d.is_zero());
        let (close_tx, close_rx) = oneshot::channel::<()>();
        let watcher = match heartbeat {
            Some(timeout) => {
                let hb_seen = hb_seen.clone();
                let last_seen = last_seen.clone();
                let limit_ms = timeout.as_millis() as i64;
                Some(tokio::spawn(async move {
                    let mut tick = tokio::time::interval(Duration::from_secs(1));
                    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
                    tick.tick().await;
                    loop {
                        tick.tick().await;
                        if !hb_seen.load(Ordering::Relaxed) {
                            continue;
                        }
                        let idle_ms = now_ms() - last_seen.load(Ordering::Relaxed);
                        if idle_ms >= limit_ms {
                            warn!(
                                "OneBot 心跳超时：{}ms 未收到任何帧，主动断开等待重连",
                                idle_ms
                            );
                            let _ = close_tx.send(());
                            return;
                        }
                    }
                }))
            }
            None => {
                drop(close_tx);
                None
            }
        };

        let reader = tokio::spawn(async move {
            let close = async move {
                if heartbeat.is_some() {
                    let _ = close_rx.await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::pin!(close);

            loop {
                tokio::select! {
                    maybe = stream.next() => {
                        let Some(frame) = maybe else { break };
                        last_seen.store(now_ms(), Ordering::Relaxed);
                        match frame {
                            Ok(Message::Text(text)) => {
                                process_frame(&api, &ev_tx, &hb_seen, text.as_str()).await
                            }
                            Ok(Message::Close(_)) => break,
                            Ok(_) => {}
                            Err(e) => {
                                warn!("OneBot 读取出错: {e}");
                                break;
                            }
                        }
                    }
                    _ = &mut close => break,
                }
            }
        });

        Connection {
            server: self.clone(),
            meta,
            conn_id,
            events: ev_rx,
            reader,
            writer,
            watcher,
            closed: false,
        }
    }
}

/// 一条反向 WS 连接。事件已经被读循环放进队列，`recv()` 不会阻塞心跳与回包处理。
pub struct Connection {
    server: Arc<Server>,
    meta: ConnMeta,
    conn_id: Option<u64>,
    events: mpsc::Receiver<Incoming>,
    reader: JoinHandle<()>,
    writer: JoinHandle<()>,
    watcher: Option<JoinHandle<()>>,
    closed: bool,
}

impl Connection {
    pub fn meta(&self) -> ConnMeta {
        self.meta
    }

    pub fn conn_id(&self) -> Option<u64> {
        self.conn_id
    }

    /// 取下一条事件；连接断开且队列排空后返回 `None`。
    pub async fn recv(&mut self) -> Option<Incoming> {
        self.events.recv().await
    }

    /// 正常收尾：停后台任务、注销 action 通道、尽量把出站帧发完。
    pub async fn close(mut self) {
        self.shutdown().await;
    }

    async fn shutdown(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;

        if let Some(w) = self.watcher.take() {
            w.abort();
        }
        self.reader.abort();

        let id = self.conn_id.take();
        if let Some(id) = id {
            self.server.api.detach(id).await;
            info!("OneBot 断开（conn={id}）");
        } else {
            info!("OneBot 断开（Event 连接）");
        }

        if tokio::time::timeout(Duration::from_secs(2), &mut self.writer)
            .await
            .is_err()
        {
            self.writer.abort();
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        self.reader.abort();
        if let Some(w) = self.watcher.take() {
            w.abort();
        }
        self.writer.abort();

        // detach 是 async 的，Drop 里只能交给运行时兜底；正常情况下走 close()
        if let Some(id) = self.conn_id.take() {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let api = self.server.api.clone();
                handle.spawn(async move { api.detach(id).await });
            }
        }
    }
}

async fn process_frame(
    api: &OneBotApi,
    ev_tx: &mpsc::Sender<Incoming>,
    hb_seen: &AtomicBool,
    raw: &str,
) {
    let value: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            warn!("无法解析的 OneBot 帧（已忽略）: {e}");
            return;
        }
    };

    // 带 echo 的是 action 回包，就地唤醒等待方；echo 可能是字符串/数字/对象
    if let Some(echo) = value.get("echo") {
        let key = match echo {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        api.complete(&key, value).await;
        return;
    }

    let ev = match serde_json::from_value::<Incoming>(value) {
        Ok(ev) => ev,
        Err(e) => {
            warn!("事件字段不合法（已忽略）: {e}");
            return;
        }
    };

    if ev.post_type.as_deref() == Some("meta_event")
        && ev.meta_event_type.as_deref() == Some("heartbeat")
    {
        hb_seen.store(true, Ordering::Relaxed);
    }

    match ev_tx.try_send(ev) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            warn!("事件队列已满，丢弃事件");
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            debug!("事件接收端已关闭，丢弃事件");
        }
    }
}

/// 校验 `Authorization: Bearer <token>`。
///
/// `access_token` 为 `None` 或空串表示不鉴权；常量时间比较，避免时序差异。
pub fn authorize(access_token: Option<&str>, authorization: Option<&str>) -> bool {
    let Some(token) = access_token.map(str::trim).filter(|t| !t.is_empty()) else {
        return true;
    };
    let Some(got) = authorization else {
        return false;
    };
    let expected = format!("Bearer {token}");
    ct_eq(got.as_bytes(), expected.as_bytes())
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_parsing() {
        assert_eq!(Role::from_header(Some("API")), Role::Api);
        assert_eq!(Role::from_header(Some("event")), Role::Event);
        assert_eq!(Role::from_header(Some("Universal")), Role::Universal);
        assert_eq!(Role::from_header(None), Role::Universal);
        assert!(!Role::Event.handles_api());
        assert!(Role::Universal.handles_api());
    }

    #[test]
    fn conn_meta_from_headers() {
        let meta = ConnMeta::from_headers(Some("10001"), Some("API"));
        assert_eq!(meta.self_id, Some(10001));
        assert_eq!(meta.role, Role::Api);

        // 非法 / 0 / 负数都当作没有
        assert_eq!(ConnMeta::from_headers(Some("abc"), None).self_id, None);
        assert_eq!(ConnMeta::from_headers(Some("0"), None).self_id, None);
        assert_eq!(ConnMeta::from_headers(Some("-1"), None).self_id, None);
    }

    #[test]
    fn authorization() {
        // 不配置 token = 不鉴权
        assert!(authorize(None, None));
        assert!(authorize(Some(""), None));
        assert!(authorize(Some("  "), None));

        assert!(authorize(Some("abc"), Some("Bearer abc")));
        assert!(!authorize(Some("abc"), Some("Bearer abd")));
        assert!(!authorize(Some("abc"), Some("abc")));
        assert!(!authorize(Some("abc"), None));
    }

    #[test]
    fn constant_time_compare() {
        assert!(ct_eq(b"Bearer abc", b"Bearer abc"));
        assert!(!ct_eq(b"Bearer abc", b"Bearer abd"));
        assert!(!ct_eq(b"Bearer abc", b"Bearer abcd"));
    }
}
