use std::path::PathBuf;

use anyhow::{Context, Result};
use hanpai_in_group::{config::Config, db, router, state::AppState};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let config_path = std::env::var("HANPAI_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.toml"));
    let cfg = Config::load(&config_path).context("加载配置失败")?;
    let _log_guard = init_logging(&cfg);

    if cfg.groups.whitelist.is_empty() {
        warn!("groups.whitelist 为空，机器人不会响应任何群");
    }
    if cfg.admin.qq.is_empty() {
        warn!("admin.qq 为空，管理命令无人可用");
    }
    if cfg.bot.qq == 0 {
        warn!("bot.qq = 0：self_id 将依赖反向 WS 的 X-Self-ID 头或事件里的 self_id，建议显式配置");
    }

    let db_path = std::env::var("HANPAI_DB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("hanpai.db"));
    let pool = db::connect(&db_path).await?;
    db::migrate(&pool).await?;

    let listen = cfg.onebot.listen.clone();
    let ws_path = cfg.onebot.path.clone();
    let state = AppState::new(cfg, pool, config_path);

    tokio::spawn(hanpai_in_group::memory::ticker(state.clone()));
    tokio::spawn(hanpai_in_group::decision::ticker(state.clone()));
    tokio::spawn(hanpai_in_group::maintenance::ticker(state.clone()));
    tokio::spawn(hanpai_in_group::reply::outbox_ticker(state.clone()));

    let app = router(state);
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .with_context(|| format!("监听 {listen} 失败"))?;

    info!("hanpai-in-group 已启动，反向 WS 地址 ws://{listen}{ws_path}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    info!("已退出");
    Ok(())
}

/// 初始化日志：控制台 + 按天切分的文件。文件建不起来就只输出控制台。
fn init_logging(cfg: &Config) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(cfg.logging.level.clone()));
    let console = cfg.logging.console.then(tracing_subscriber::fmt::layer);

    if cfg.logging.dir.trim().is_empty() {
        tracing_subscriber::registry()
            .with(filter)
            .with(console)
            .init();
        return None;
    }

    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("hanpai")
        .filename_suffix("log")
        .max_log_files(cfg.logging.max_days.max(1) as usize)
        .build(&cfg.logging.dir);

    let appender = match appender {
        Ok(a) => a,
        Err(e) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(console)
                .init();
            warn!("日志文件初始化失败（{e}），只输出到控制台");
            return None;
        }
    };

    let (writer, guard) = tracing_appender::non_blocking(appender);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(writer);

    tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file_layer)
        .init();
    Some(guard)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("收到 Ctrl-C，准备退出");
}
