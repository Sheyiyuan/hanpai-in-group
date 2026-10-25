//! hanpai-in-group：一个长期记忆、会自己判断要不要插话的 QQ 群聊机器人。

pub mod admin;
pub mod config;
pub mod db;
pub mod decision;
pub mod embed;
pub mod llm;
pub mod maintenance;
pub mod memory;
pub mod onebot;
pub mod persona;
pub mod prompts;
pub mod reply;
pub mod state;
pub mod sticker;
pub mod vision;
pub mod ws;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;

use state::AppState;

pub fn router(state: Arc<AppState>) -> Router {
    let path = state.cfg().onebot.path.clone();
    Router::new()
        .route(&path, get(ws::ws_handler))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state)
}
