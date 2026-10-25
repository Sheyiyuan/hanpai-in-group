//! OneBot v11 协议的最小实现，和本项目的配置 / 数据库 / 业务逻辑完全解耦，方便整体复用。
//!
//! 三个子模块可以按需取用：
//! - [`event`]：事件结构、消息段、CQ 码解析、@ / 引用判定（纯函数，无框架依赖）
//! - [`api`]：反向 WS 上的 action 调用（`{action, params, echo}` + pending map 配对）
//! - [`server`]：反向 WS 服务端（鉴权、角色、心跳、事件队列），只依赖 axum 的 WebSocket 类型
//!
//! 典型用法：
//!
//! ```ignore
//! let server = onebot::Server::new(onebot::ServerConfig::default());
//! let api = server.api();                       // 业务层用它发消息
//! let meta = onebot::ConnMeta::from_headers(x_self_id, x_client_role);
//! let mut conn = server.accept(socket, meta);   // 在 axum upgrade 回调里
//! while let Some(ev) = conn.recv().await {      // 后台读循环已经处理好心跳和回包
//!     // 处理事件
//! }
//! conn.close().await;
//! ```

pub mod api;
pub mod event;
pub mod server;

pub use api::OneBotApi;
pub use event::{
    detect_trigger, mentions, parse_cq, render_text, reply_target, Incoming, Segment, Sender,
};
pub use server::{
    authorize, ConnMeta, Connection, Role, Server, ServerConfig, DEFAULT_QUEUE_CAPACITY,
};
