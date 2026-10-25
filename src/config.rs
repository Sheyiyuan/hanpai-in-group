//! 配置：单文件 TOML，全部字段有默认值，启动时校验，非法直接退出。

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub bot: Bot,
    pub onebot: OneBot,
    pub providers: BTreeMap<String, Provider>,
    pub models: BTreeMap<String, ModelDef>,
    pub model: Model,
    pub embedding: Embedding,
    pub groups: Groups,
    pub trigger: Trigger,
    pub planner: Planner,
    pub persona: Persona,
    pub memory: Memory,
    pub vision: Vision,
    pub sticker: Sticker,
    pub prompts: Prompts,
    pub logging: Logging,
    pub expression: Expression,
    pub admin: Admin,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bot {
    pub qq: i64,
    pub nickname: String,
    pub aliases: Vec<String>,
}

impl Default for Bot {
    fn default() -> Self {
        Self {
            qq: 0,
            nickname: "hanpai".into(),
            aliases: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct OneBot {
    pub listen: String,
    pub path: String,
    /// 从哪个环境变量读 token；None 或空字符串表示不鉴权。
    /// 配了变量名但环境变量缺失/为空时拒绝连接（fail-closed）。
    pub access_token_env: Option<String>,
    pub action_timeout_secs: u64,
    /// 收到过心跳后，多少秒没有任何帧就判定连接已死并主动断开；0 = 不检测
    pub heartbeat_timeout_secs: u64,
    /// 重复消息判定窗口（秒）：同一 message_id 只在窗口内算重复
    pub dedup_window_secs: u64,
}

impl Default for OneBot {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:3001".into(),
            path: "/onebot".into(),
            access_token_env: Some("ONEBOT_TOKEN".into()),
            action_timeout_secs: 10,
            heartbeat_timeout_secs: 30,
            dedup_window_secs: 600,
        }
    }
}

/// 一个模型提供商：OpenAI 兼容接口的地址与密钥来源。
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    /// 接口根地址，程序会拼 /chat/completions 或 /embeddings
    pub base_url: String,
    /// 密钥所在的环境变量；留空表示这个提供商不需要鉴权
    #[serde(default)]
    pub api_key_env: String,
}

impl Provider {
    /// 读密钥。None 表示不需要鉴权。
    pub fn api_key(&self) -> Option<String> {
        let var = self.api_key_env.trim();
        if var.is_empty() {
            return None;
        }
        std::env::var(var).ok().filter(|k| !k.trim().is_empty())
    }
}

/// 思考模式开关（DeepSeek / OpenAI 兼容格式里的 thinking 字段）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Thinking {
    On,
    Off,
    /// 不传，跟随提供商默认行为
    #[default]
    Auto,
}

/// 一个声明好的模型：调用方只写名字，具体走哪个提供商、什么能力都在这。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelDef {
    /// 引用 [providers.*] 里的名字
    pub provider: String,
    /// 传给提供商的真实模型 id
    pub model: String,
    /// 是否多模态，能直接吃图片
    pub visual: bool,
    /// on / off / auto（auto = 不传，跟随提供商默认）
    pub thinking: Thinking,
    /// 思考强度：minimal / low / medium / high / xhigh / max / ultra
    pub reasoning_effort: Option<String>,
    /// 采样温度；思考模式下会被忽略，所以只在非思考时下发
    pub temperature: Option<f32>,
}

impl Default for ModelDef {
    fn default() -> Self {
        Self {
            provider: String::new(),
            model: String::new(),
            visual: false,
            thinking: Thinking::Auto,
            reasoning_effort: None,
            temperature: None,
        }
    }
}

/// 模型配置。每个功能是一串模型名字，按先后顺序失败重试。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Model {
    /// 最终生成回复
    pub chat: Vec<String>,
    /// 判断该不该说话
    pub planner: Vec<String>,
    /// 记忆抽取 / 画像重写
    pub summary: Vec<String>,
    /// 图片转述；留空则复用 chat 的路由
    pub caption: Vec<String>,
    pub timeout_secs: u64,
    pub max_retries: u32,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            chat: vec!["flash".into()],
            planner: vec!["flash".into()],
            summary: vec!["flash".into()],
            caption: Vec::new(),
            timeout_secs: 30,
            max_retries: 2,
        }
    }
}

/// 图片理解：入库时转述一次，之后只留文本。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Vision {
    pub enabled: bool,
    /// url 直传（模型自己去取）或 data_uri（先下载再内联）
    pub image_input: String,
    /// 单条消息最多处理几张图
    pub max_images_per_message: u32,
    /// 转述文本上限
    pub caption_max_chars: u32,
    /// 下载图片的超时
    pub download_timeout_secs: u64,
    /// 单张图最大字节数
    pub max_bytes: u64,
}

impl Default for Vision {
    fn default() -> Self {
        Self {
            enabled: true,
            image_input: "url".into(),
            max_images_per_message: 4,
            caption_max_chars: 120,
            download_timeout_secs: 20,
            max_bytes: 8 * 1024 * 1024,
        }
    }
}

/// 表情包收藏与复用
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Sticker {
    pub enabled: bool,
    pub dir: String,
    pub max_count: u32,
    /// 回复时附带表情的概率
    pub send_probability: f64,
    /// 只在这些群收藏/使用；空 = 所有白名单群
    pub groups: Vec<i64>,
}

impl Default for Sticker {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: "data/stickers".into(),
            max_count: 64,
            send_probability: 0.15,
            groups: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Embedding {
    /// 关掉后检索自动降级为关键词 + 时间衰减
    pub enabled: bool,
    /// 引用 [models.*] 里的名字，只允许一个：换模型意味着换向量空间
    pub model: String,
    /// 必须和模型真实输出维度一致
    pub dim: u32,
}

impl Default for Embedding {
    fn default() -> Self {
        Self {
            enabled: false,
            model: "bge-m3".into(),
            dim: 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Groups {
    pub whitelist: Vec<i64>,
    pub blacklist_users: Vec<i64>,
    pub talk_value: f64,
    /// -1 表示不限
    pub max_per_hour: i64,
    /// -1 表示不限
    pub max_per_day: i64,
    pub cooldown_secs: u64,
    pub debounce_quiet_ms: u64,
    pub debounce_active_ms: u64,
    pub inevitable_at_reply: bool,
    pub inevitable_when_named: bool,
    pub max_reply_chars: usize,
    pub dry_run: bool,
    /// 回复前的随机延迟下限（毫秒），模拟打字
    pub reply_delay_min_ms: u64,
    /// 回复前的随机延迟上限（毫秒）
    pub reply_delay_max_ms: u64,
    /// 冷却期间最多堆积多少条待发消息
    pub outbox_max: usize,
    pub overrides: Vec<GroupOverride>,
}

impl Default for Groups {
    fn default() -> Self {
        Self {
            whitelist: Vec::new(),
            blacklist_users: Vec::new(),
            talk_value: 0.5,
            max_per_hour: -1,
            max_per_day: -1,
            cooldown_secs: 5,
            debounce_quiet_ms: 2000,
            debounce_active_ms: 5000,
            inevitable_at_reply: true,
            inevitable_when_named: true,
            max_reply_chars: 60,
            dry_run: true,
            reply_delay_min_ms: 600,
            reply_delay_max_ms: 1800,
            outbox_max: 3,
            overrides: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupOverride {
    pub group_id: i64,
    pub talk_value: Option<f64>,
    pub max_per_hour: Option<i64>,
    pub max_per_day: Option<i64>,
    pub cooldown_secs: Option<u64>,
    pub max_reply_chars: Option<usize>,
    pub dry_run: Option<bool>,
    pub outbox_max: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Memory {
    pub retain_days: u32,
    pub extract_batch_size: u32,
    pub extract_interval_mins: u64,
    pub recall_top_k: u32,
    pub vector_threshold: f64,
    /// 生成回复时带最近多少条群聊
    pub recent_context: u32,
}

impl Default for Memory {
    fn default() -> Self {
        Self {
            retain_days: 30,
            extract_batch_size: 80,
            extract_interval_mins: 15,
            recall_top_k: 12,
            vector_threshold: 0.85,
            recent_context: 20,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Expression {
    pub enabled: bool,
    pub learn: bool,
    #[serde(rename = "use")]
    pub use_learned: bool,
    pub checked_only: bool,
    pub max_per_reply: u32,
    pub persona_source: String,
}

impl Default for Expression {
    fn default() -> Self {
        Self {
            enabled: true,
            learn: true,
            use_learned: true,
            checked_only: false,
            max_per_reply: 2,
            persona_source: "group".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Admin {
    pub qq: Vec<i64>,
}

/// 触发函数参数：兴趣越高、越久没说话，越容易开口。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Trigger {
    /// 沉默多久后"手痒"程度到一半（秒）
    pub idle_half_life_secs: f64,
    /// 沉默加成占多大比重，0 = 只看兴趣，1 = 沉默足够久就一定开口
    pub idle_weight: f64,
    /// 兴趣曲线，<1 抬高中等兴趣，>1 更挑
    pub interest_gamma: f64,
    /// 概率下限（不想让它彻底哑掉时可以设 0.02 之类）
    pub min_probability: f64,
}

impl Default for Trigger {
    fn default() -> Self {
        Self {
            idle_half_life_secs: 600.0,
            idle_weight: 0.35,
            interest_gamma: 0.8,
            min_probability: 0.0,
        }
    }
}

/// Planner：判断「这批消息值不值得接」的小模型行为。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Planner {
    /// 置信度低于这个值一律不开口
    pub min_confidence: f64,
    /// 一次最多把多少条新消息送进 Planner
    pub max_batch: u32,
    /// Planner 参考多少条记忆
    pub recall_top_k: u32,
}

impl Default for Planner {
    fn default() -> Self {
        Self {
            min_confidence: 0.4,
            max_batch: 12,
            recall_top_k: 5,
        }
    }
}

/// 人物设定：它本来就是谁。留空的段落不会进提示词。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Persona {
    /// 角色名；留空则退回 bot.nickname
    pub name: String,
    /// 我是谁
    pub identity: String,
    /// 性格
    pub personality: String,
    /// 行为准则：什么时候插话、什么时候闭嘴
    pub behavior: String,
    /// 说话风格
    pub speech: String,
    /// 禁区，一条一句
    pub taboos: Vec<String>,
}

/// 日志落盘。`dir` 为空则只输出到控制台。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Logging {
    pub dir: String,
    pub level: String,
    /// 保留多少天的日志文件
    pub max_days: u32,
    /// 是否同时输出到控制台
    pub console: bool,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            dir: "logs".into(),
            level: "info,hanpai_in_group=debug".into(),
            max_days: 7,
            console: true,
        }
    }
}

/// 提示词目录：目录里有同名文件就用文件，没有就用编译进来的默认值。
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Prompts {
    pub dir: String,
}

impl Default for Prompts {
    fn default() -> Self {
        Self {
            dir: "prompts".into(),
        }
    }
}

/// 按群解析后的策略，已叠加 groups.overrides
#[derive(Debug, Clone)]
pub struct GroupPolicy {
    pub talk_value: f64,
    /// -1 表示不限
    pub max_per_hour: i64,
    /// -1 表示不限
    pub max_per_day: i64,
    pub cooldown_secs: u64,
    pub max_reply_chars: usize,
    pub dry_run: bool,
    pub reply_delay_min_ms: u64,
    pub reply_delay_max_ms: u64,
    pub outbox_max: usize,
}

impl Groups {
    pub fn policy(&self, group_id: i64) -> GroupPolicy {
        let mut p = GroupPolicy {
            talk_value: self.talk_value,
            max_per_hour: self.max_per_hour,
            max_per_day: self.max_per_day,
            cooldown_secs: self.cooldown_secs,
            max_reply_chars: self.max_reply_chars,
            dry_run: self.dry_run,
            reply_delay_min_ms: self.reply_delay_min_ms,
            reply_delay_max_ms: self.reply_delay_max_ms,
            outbox_max: self.outbox_max,
        };
        if let Some(o) = self.overrides.iter().find(|o| o.group_id == group_id) {
            if let Some(v) = o.talk_value {
                p.talk_value = v;
            }
            if let Some(v) = o.max_per_hour {
                p.max_per_hour = v;
            }
            if let Some(v) = o.max_per_day {
                p.max_per_day = v;
            }
            if let Some(v) = o.cooldown_secs {
                p.cooldown_secs = v;
            }
            if let Some(v) = o.max_reply_chars {
                p.max_reply_chars = v;
            }
            if let Some(v) = o.dry_run {
                p.dry_run = v;
            }
            if let Some(v) = o.outbox_max {
                p.outbox_max = v;
            }
        }
        p
    }

    pub fn is_allowed(&self, group_id: i64) -> bool {
        self.whitelist.contains(&group_id)
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("读不到配置文件 {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("解析配置文件 {} 失败", path.display()))?;
        let errors = cfg.validate();
        if !errors.is_empty() {
            bail!("配置校验失败:\n  - {}", errors.join("\n  - "));
        }
        Ok(cfg)
    }

    /// 返回所有问题，一次报全，避免改一个跑一次。
    pub fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();

        if self.onebot.listen.parse::<std::net::SocketAddr>().is_err() {
            e.push(format!(
                "onebot.listen 不是合法的 SocketAddr: {}",
                self.onebot.listen
            ));
        }
        if !self.onebot.path.starts_with('/') {
            e.push("onebot.path 必须以 / 开头".into());
        }
        if self.onebot.action_timeout_secs == 0 {
            e.push("onebot.action_timeout_secs 不能为 0".into());
        }
        if self.onebot.dedup_window_secs == 0 {
            e.push("onebot.dedup_window_secs 不能为 0".into());
        }
        if self.onebot.heartbeat_timeout_secs > 3600 {
            e.push("onebot.heartbeat_timeout_secs 不能超过 3600".into());
        }
        if !(0.0..=1.0).contains(&self.groups.talk_value) {
            e.push("groups.talk_value 必须在 0~1 之间".into());
        }
        for o in &self.groups.overrides {
            if let Some(v) = o.talk_value {
                if !(0.0..=1.0).contains(&v) {
                    e.push(format!(
                        "groups.overrides[{}].talk_value 必须在 0~1 之间",
                        o.group_id
                    ));
                }
            }
        }
        if self.groups.max_reply_chars == 0 {
            e.push("groups.max_reply_chars 不能为 0".into());
        }
        if self.groups.whitelist.is_empty() {
            e.push("groups.whitelist 为空，机器人不会响应任何群".into());
        }
        if self.admin.qq.is_empty() {
            e.push("admin.qq 为空，没有任何人能发管理命令".into());
        }
        for (name, v) in [
            ("groups.max_per_hour", self.groups.max_per_hour),
            ("groups.max_per_day", self.groups.max_per_day),
        ] {
            if v == 0 || v < -1 {
                e.push(format!("{name} 只能是 -1（不限）或 >=1"));
            }
        }
        if !(0.0..=1.0).contains(&self.trigger.idle_weight) {
            e.push("trigger.idle_weight 必须在 0~1 之间".into());
        }
        if self.trigger.idle_half_life_secs <= 0.0 {
            e.push("trigger.idle_half_life_secs 必须大于 0".into());
        }
        if self.trigger.interest_gamma <= 0.0 || self.trigger.interest_gamma > 5.0 {
            e.push("trigger.interest_gamma 必须在 (0, 5] 之间".into());
        }
        if !(0.0..=1.0).contains(&self.trigger.min_probability) {
            e.push("trigger.min_probability 必须在 0~1 之间".into());
        }
        if !(0.0..=1.0).contains(&self.planner.min_confidence) {
            e.push("planner.min_confidence 必须在 0~1 之间".into());
        }
        if self.planner.max_batch == 0 || self.planner.max_batch > 50 {
            e.push("planner.max_batch 必须在 1~50 之间".into());
        }
        if self.planner.recall_top_k > 20 {
            e.push("planner.recall_top_k 不能超过 20".into());
        }
        for o in &self.groups.overrides {
            for (name, v) in [
                ("max_per_hour", o.max_per_hour),
                ("max_per_day", o.max_per_day),
            ] {
                if let Some(v) = v {
                    if v == 0 || v < -1 {
                        e.push(format!(
                            "groups.overrides[{}].{name} 只能是 -1 或 >=1",
                            o.group_id
                        ));
                    }
                }
            }
        }
        if self.memory.retain_days == 0 {
            e.push("memory.retain_days 不能为 0".into());
        }
        if self.prompts.dir.trim().is_empty() {
            e.push("prompts.dir 不能为空".into());
        }
        if self.memory.recent_context == 0 {
            e.push("memory.recent_context 不能为 0".into());
        }
        // 提供商
        if self.providers.is_empty() {
            e.push("providers 为空，至少要配一个模型提供商".into());
        }
        for (name, p) in &self.providers {
            if name.trim().is_empty() {
                e.push("providers 里的名字不能为空".into());
            }
            if !p.base_url.starts_with("http://") && !p.base_url.starts_with("https://") {
                e.push(format!(
                    "providers.{name}.base_url 必须以 http:// 或 https:// 开头"
                ));
            }
        }

        // 模型声明
        if self.models.is_empty() {
            e.push("models 为空，至少声明一个模型".into());
        }
        for (name, m) in &self.models {
            if name.trim().is_empty() {
                e.push("models 里的名字不能为空".into());
            }
            if m.provider.trim().is_empty() {
                e.push(format!("models.{name}.provider 不能为空"));
            } else if !self.providers.contains_key(&m.provider) {
                e.push(format!("models.{name} 引用了未定义的提供商 {}", m.provider));
            }
            if m.model.trim().is_empty() {
                e.push(format!(
                    "models.{name}.model 不能为空（这是传给提供商的真实模型 id）"
                ));
            }
            if let Some(effort) = &m.reasoning_effort {
                if !matches!(
                    effort.as_str(),
                    "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
                ) {
                    e.push(format!(
                        "models.{name}.reasoning_effort 只能是 minimal/low/medium/high/xhigh/max/ultra"
                    ));
                }
            }
            if let Some(t) = m.temperature {
                if !(0.0..=2.0).contains(&t) {
                    e.push(format!("models.{name}.temperature 必须在 0~2 之间"));
                }
            }
        }

        // 每个功能的路由链
        for (role, list) in [
            ("chat", &self.model.chat),
            ("planner", &self.model.planner),
            ("summary", &self.model.summary),
            ("caption", &self.model.caption),
        ] {
            if list.is_empty() && role != "caption" {
                e.push(format!("model.{role} 至少配一个模型名字"));
            }
            for name in list {
                if !self.models.contains_key(name) {
                    e.push(format!(
                        "model.{role} 里的「{name}」没有在 [models.*] 里声明"
                    ));
                }
            }
        }
        for name in &self.model.caption {
            if let Some(m) = self.models.get(name) {
                if !m.visual {
                    e.push(format!("model.caption 用的 {name} 必须 visual = true"));
                }
            }
        }
        if self.vision.enabled {
            let has_visual = self
                .model
                .chat
                .iter()
                .chain(self.model.caption.iter())
                .any(|n| self.models.get(n).is_some_and(|m| m.visual));
            if !has_visual {
                e.push(
                    "vision.enabled = true，但 chat / caption 里没有任何 visual = true 的模型"
                        .into(),
                );
            }
        }
        if !matches!(self.vision.image_input.as_str(), "url" | "data_uri") {
            e.push("vision.image_input 只能是 url 或 data_uri".into());
        }
        if self.vision.max_images_per_message == 0 || self.vision.max_images_per_message > 10 {
            e.push("vision.max_images_per_message 必须在 1~10 之间".into());
        }
        if self.sticker.max_count == 0 {
            e.push("sticker.max_count 不能为 0".into());
        }
        if !(0.0..=1.0).contains(&self.sticker.send_probability) {
            e.push("sticker.send_probability 必须在 0~1 之间".into());
        }
        if self.groups.reply_delay_min_ms > self.groups.reply_delay_max_ms {
            e.push("groups.reply_delay_min_ms 不能大于 reply_delay_max_ms".into());
        }
        if self.groups.outbox_max == 0 || self.groups.outbox_max > 50 {
            e.push("groups.outbox_max 必须在 1~50 之间".into());
        }
        for o in &self.groups.overrides {
            if let Some(v) = o.outbox_max {
                if v == 0 || v > 50 {
                    e.push(format!(
                        "groups.overrides[{}].outbox_max 必须在 1~50 之间",
                        o.group_id
                    ));
                }
            }
        }
        if self.groups.reply_delay_max_ms > 30_000 {
            e.push("groups.reply_delay_max_ms 不能超过 30000（30 秒）".into());
        }
        if !matches!(
            self.logging.level.split(',').next().unwrap_or("").trim(),
            "trace" | "debug" | "info" | "warn" | "error"
        ) {
            e.push("logging.level 必须以 trace/debug/info/warn/error 开头".into());
        }
        if self.logging.max_days == 0 {
            e.push("logging.max_days 不能为 0".into());
        }
        if self.model.timeout_secs == 0 {
            e.push("model.timeout_secs 不能为 0".into());
        }
        if self.model.max_retries > 10 {
            e.push("model.max_retries 不能超过 10".into());
        }

        if self.expression.enabled {
            let p = &self.expression.persona_source;
            if p != "group" && !p.starts_with("member:") {
                e.push("expression.persona_source 只能是 \"group\" 或 \"member:<QQ>\"".into());
            }
        }
        e
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_errors() {
        let cfg = Config::default();
        let errs = cfg.validate();
        assert!(errs.iter().any(|e| e.contains("whitelist")));
    }

    #[test]
    fn parse_full_example() {
        let text = include_str!("../config.example.toml");
        let cfg: Config = toml::from_str(text).expect("示例配置必须能解析");
        let errs = cfg.validate();
        assert!(errs.is_empty(), "{errs:?}");
    }

    #[test]
    fn override_applies() {
        let cfg = Config::default();
        let p = cfg.groups.policy(987654321);
        assert!((p.talk_value - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn validate_catches_unknown_provider() {
        let mut cfg = Config::default();
        cfg.groups.whitelist = vec![1];
        cfg.admin.qq = vec![1];

        let errs = cfg.validate();
        assert!(
            errs.iter().any(|e| e.contains("providers 为空")),
            "{errs:?}"
        );

        cfg.providers.insert(
            "deepseek".into(),
            Provider {
                base_url: "https://api.deepseek.com".into(),
                api_key_env: "DEEPSEEK_API_KEY".into(),
            },
        );
        let errs = cfg.validate();
        assert!(!errs.iter().any(|e| e.contains("未定义")), "{errs:?}");
    }

    #[test]
    fn unknown_field_rejected() {
        let text = "[admin]\nqq = [1]\ntypo_field = true\n";
        let r: Result<Config, _> = toml::from_str(text);
        assert!(r.is_err());
    }
}
