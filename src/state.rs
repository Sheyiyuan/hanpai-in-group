//! 进程内共享状态

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use arc_swap::{ArcSwap, ArcSwapOption};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, RwLock};
use tracing::warn;

use crate::config::Config;
use crate::embed::Embedder;
use crate::llm::LlmClient;
use crate::onebot::api::OneBotApi;
use crate::onebot::server::{Server, ServerConfig};
use crate::prompts::PromptSet;

pub struct AppState {
    pub cfg: ArcSwap<Config>,
    pub config_path: PathBuf,
    pub db: SqlitePool,
    /// 反向 WS 服务端（协议层），负责连接、心跳、action 通道
    pub onebot: Arc<Server>,
    /// 与 `onebot` 共用的 action 客户端，业务层直接发消息
    pub api: Arc<OneBotApi>,
    pub started_at: Instant,
    /// group_id -> 静音截止时间戳
    pub mutes: RwLock<HashMap<i64, i64>>,
    pub self_id: AtomicI64,
    /// 没 key 时是 None：机器人还能记录，只是没脑子
    pub llm: ArcSwapOption<LlmClient>,
    pub embedder: ArcSwap<Embedder>,
    pub prompts: ArcSwap<PromptSet>,
    /// group_id -> 上次发言时间（毫秒），用于冷却
    pub last_spoke: RwLock<HashMap<i64, i64>>,
    /// 抽取串行化，避免同一批消息被抽两遍
    pub extract_lock: Mutex<()>,
    /// 每个群正在攒的消息批
    pub batches: RwLock<HashMap<i64, crate::decision::Batch>>,
    /// 正在评估的群，避免同一批被并发判断两次
    pub evaluating: Mutex<HashSet<i64>>,
    /// 冷却期间攒下的待发消息，按群 FIFO
    pub outbox: RwLock<HashMap<i64, VecDeque<OutboxItem>>>,
}

/// 一条等待发送的回复。冷却结束后由 outbox ticker 按序发出。
#[derive(Debug, Clone)]
pub struct OutboxItem {
    pub group_id: i64,
    pub text: String,
    pub sticker: Option<crate::db::StickerRow>,
    pub trigger: String,
    pub reason: String,
    pub confidence: f32,
    pub facts_json: String,
    pub queued_at: i64,
}

impl AppState {
    pub fn new(cfg: Config, db: SqlitePool, config_path: PathBuf) -> Arc<Self> {
        let llm = match LlmClient::new(&cfg) {
            Ok(v) => v,
            Err(e) => {
                warn!("初始化模型客户端失败: {e:#}");
                None
            }
        };
        let embedder = match Embedder::from_config(&cfg) {
            Ok(v) => v,
            Err(e) => {
                warn!("初始化向量客户端失败: {e:#}，检索降级为关键词");
                Embedder::disabled()
            }
        };
        let prompts = PromptSet::load(&cfg.prompts.dir);
        let onebot = Server::new(onebot_config(&cfg));
        let api = onebot.api();
        // 协议端的 X-Self-ID 到达之前，先用配置里的 QQ 兜底
        let bot_qq = cfg.bot.qq;

        Arc::new(Self {
            cfg: ArcSwap::from_pointee(cfg),
            config_path,
            db,
            onebot,
            api,
            started_at: Instant::now(),
            mutes: RwLock::new(HashMap::new()),
            self_id: AtomicI64::new(bot_qq),
            llm: ArcSwapOption::from(llm.map(Arc::new)),
            embedder: ArcSwap::from_pointee(embedder),
            prompts: ArcSwap::from_pointee(prompts),
            last_spoke: RwLock::new(HashMap::new()),
            extract_lock: Mutex::new(()),
            batches: RwLock::new(HashMap::new()),
            evaluating: Mutex::new(HashSet::new()),
            outbox: RwLock::new(HashMap::new()),
        })
    }

    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.load_full()
    }

    /// 配置热重载时同步协议层参数（action 超时、心跳超时、队列上限）。
    pub fn reload_onebot(&self, cfg: &Config) {
        self.onebot.configure(onebot_config(cfg));
    }

    /// 机器人自己的 QQ：协议端上报的优先，没有就退回配置。
    pub fn self_qq(&self, cfg: &Config) -> i64 {
        let id = self.self_id.load(Ordering::Relaxed);
        if id > 0 {
            id
        } else {
            cfg.bot.qq
        }
    }

    pub fn llm(&self) -> Option<Arc<LlmClient>> {
        self.llm.load_full()
    }

    pub fn embedder(&self) -> Arc<Embedder> {
        self.embedder.load_full()
    }

    pub fn prompts(&self) -> Arc<PromptSet> {
        self.prompts.load_full()
    }

    pub async fn is_muted(&self, group_id: i64) -> bool {
        let now = now_ts();
        let m = self.mutes.read().await;
        m.get(&group_id).is_some_and(|&until| until > now)
    }

    pub async fn mute_until(&self, group_id: i64, until: i64) {
        self.mutes.write().await.insert(group_id, until);
    }

    pub async fn unmute(&self, group_id: i64) {
        self.mutes.write().await.remove(&group_id);
    }

    pub async fn muted_until(&self, group_id: i64) -> Option<i64> {
        let m = self.mutes.read().await;
        m.get(&group_id).copied()
    }

    pub async fn is_cooling(&self, group_id: i64, cooldown_secs: u64) -> bool {
        let last = self.last_spoke.read().await.get(&group_id).copied();
        match last {
            Some(ts) => now_ms() - ts < (cooldown_secs as i64) * 1000,
            None => false,
        }
    }

    pub async fn mark_spoke(&self, group_id: i64) {
        self.last_spoke.write().await.insert(group_id, now_ms());
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 业务配置 → 协议层配置。放在这里，启动和 /reload 共用同一套换算。
fn onebot_config(cfg: &Config) -> ServerConfig {
    let hb = cfg.onebot.heartbeat_timeout_secs;
    ServerConfig {
        action_timeout: Duration::from_secs(cfg.onebot.action_timeout_secs.max(1)),
        heartbeat_timeout: (hb > 0).then(|| Duration::from_secs(hb)),
        queue_capacity: crate::onebot::DEFAULT_QUEUE_CAPACITY,
    }
}
