# hanpai-in-group 需求规格 v0.1

> 一句话：一个用 Rust 写的、通过 NapCat(OneBot v11) 接入 QQ 群的中文聊天机器人。它在指定白名单群里长期记住聊过什么、谁说的，自己判断该不该插话，说话风格像群里的普通用户，而不是一个客服。

状态：**已确认（2026-10-23）**。这份文档是验收依据，改动走 diff。

---

## 1. 已确认的前提

| 项 | 结论 |
|---|---|
| 语言 / 运行时 | Rust（edition 2021，单二进制，tokio 异步） |
| 接入方式 | OneBot v11，**反向 WebSocket**，协议端为 **NapCat** |
| 模型接口 | **OpenAI 兼容、多提供商**；模型先声明后引用，每条路由可配 thinking / reasoning_effort |
| 部署 | 一台小服务器，常驻；SQLite 单文件存全部状态 |
| 规模 | 个位数群，单群 ≤ 100 人，日消息量 ~10³ 级 |
| 项目名 | `hanpai-in-group`，位于 `~/Desktop/code/hanpai-in-group` |
| embedding | 允许任意模型，`local` / OpenAI 兼容 / `none` 三档可配 |
| 群的性质 | 成员**明确知晓**这是 bot，中小规模熟人/半熟人群 |
| 发言方式 | 默认**不 @ 人**，像普通群友一样发纯文本 |
| 风格参考 | MaiBot（麦麦）的表达学习设计 |

以上若有一条不对，先改这张表。

---

## 2. 范围

### 2.1 第一版要做

1. NapCat 反向 WS 接入、断线重连、事件去重。
2. 群白名单：非白名单群**完全忽略**（不落库、不记忆、不判断、不回复）。
3. 消息与用户画像落库；同一人改昵称/群名片后仍能正确归因。
4. 三层长期记忆：原始消息 → 原子事实 → 群/人画像。
5. 发言决策：该不该说、说什么、什么时候说。
6. 表达学习：学"怎么说"，不是学"说了什么"。
7. 全量配置：TOML 分层 + 热重载；人物设定 `[persona]` 可自由增改。
8. 管理命令、干跑模式、决策回放（`/why`）。

### 2.2 第一版明确不做

- 私聊、群临时会话、好友申请、拉群/踢人。
- 语音、视频、文件、图片生成。图片**入库时转述成文本**，原始图片与 URL 不落库。
- 多账号、多平台、Web 管理面板（先 CLI + 群内命令）。
- 主动私聊、定时播报、爬群成员列表。
- 插件系统（发版后再说）。

---

## 3. 运行形态

```
NapCat(QQ) ──反向WS──▶ qqbot-rs (axum + tokio)
                         │
                         ├─ 白名单闸门 ──▶ SQLite (WAL)
                         │                  ├ messages  原始消息
                         │                  ├ facts     原子事实 + 向量
                         │                  ├ profiles  群/人画像
                         │                  ├ styles    表达样本 + 向量
                         │                  └ decisions 决策日志
                         │
                         └─ 决策链
                            ① 规则闸门（@/回复/点名/提问）→ 必答
                            ② 成本闸门（频率、冷却、预算、安静期）
                            ③ Planner（小模型，输出 reply / wait / stop + 理由）
                            ④ Replyer（主模型，带记忆 + 表达样本生成）
                            ⑤ 输出过滤（长度、复读、敏感、@ 抑制）
```

关键工程决策：

- **向量不用独立向量库**。记忆量 10⁵ 以内，1024 维暴力余弦在 Rust 里 < 50ms；`facts` 表存 BLOB，进程内 `rayon` 算。少一个部署组件。
- **OneBot 不用第三方 crate**。v11 协议就是 JSON 帧，手写薄客户端约 400 行，比绑一个半维护的库可控。协议层独立放在 `src/onebot/`（`event` / `api` / `server` 三个子模块），不依赖本项目配置、数据库和业务状态，可整体复用到别的项目。
- **提示词全部外置**为 `prompts/*.md`，热加载。Rust 编译慢，改 prompt 不该等编译。
- **词向量可选**。没配 embedding 时自动降级为「关键词 FTS5 + 时间衰减」，功能不中断。

---

## 4. 行为规格

### 4.1 接入层

- 监听 `0.0.0.0:3001`，路径 `/onebot`，支持 `Authorization: Bearer <token>`；配了变量名但环境变量缺失/为空时拒绝连接（`access_token_env = ""` 才关闭鉴权）。
- 反向 WS 握手头 `X-Self-ID`（机器人 QQ）与 `X-Client-Role`（API / Event / Universal）都要读；只有 API/Universal 连接用于调 API。
- `message` 两种格式都兼容：消息段数组，或字符串（CQ 码就地解析）。@ 判定按消息段判等，不做文本子串匹配。
- 心跳：收到过心跳后，超过 `onebot.heartbeat_timeout_secs`（默认 30s）没有任何帧即判定断线，主动 close 等协议端重连。反向 WS 断线期间事件到不了服务端，不存在"重连期队列"。
- 事件消费队列上限 5000，溢出丢弃并告警；队列满不得阻塞读循环（否则 action 回包会被堵死）。
- 去重键：`message_id`；只把落在 `onebot.dedup_window_secs`（默认 600s）窗口内的同一 ID 视为重复投递。
- 只消费 `message.group`、`meta_event.*`、`notice.*`(仅入日志)；`message_sent`、`request` 忽略。
- 发送走 `send_group_msg`；用消息段数组发送纯文本，不依赖 `auto_escape`，也无需自建 CQ 码转义。
- API 回包校验 `retcode`（`status=async` 视为成功）；失败必须上抛，不能当成功记账。

### 4.2 白名单与用户

- `groups.whitelist = [群号]`。未命中 → 记一条 TRACE 后直接 return，**不产生任何写操作**。
- 群内 `users.blacklist = [QQ]`：该用户消息不入记忆，也不作为回复目标。
- 用户主键 QQ 号；`users.aliases` 存历史昵称/群名片 `[{name, from, to}]`。检索历史消息时按**当时的**昵称展示，避免"张三说他..."其实那时他还叫李四。
- 被 @、被引用回复、正文包含 `bot.aliases` 任一词 → 标记 `addressed = true`。

### 4.3 记忆（P1 已实现）

三层，各管各的：

| 层 | 内容 | 生命周期 | 用途 |
|---|---|---|---|
| 原始消息 | 原文 + 发送者 + 时间 | 默认 30 天，可配 | 精确回溯、抽取来源、`/why` 证据 |
| 原子事实 | "老王养了只橘猫" 这类单句事实，带主体、时间、来源 msg_id、置信度 | 长期，随访问加权 | 语义检索的主力 |
| 画像 | 每群一份「群氛围」，每人一份「这个人是谁」 | 定期全量重写 | 提供背景，不直接进检索 |

**抽取**：每 80 条消息或 15 分钟（先到先触发）批处理一次，小模型输出严格 JSON 数组；失败重试 2 次后落 `pending` 表下次再抽。

**去重与矛盾**：新事实先做向量近邻（阈值 0.85）→ 命中则让模型判 `same / update / conflict`。`conflict` 时旧事实标 `superseded_by`，**不删除**，检索默认只返回未 superseded 的。

**重要度**：`importance = 基础分(模型给 1-5) × 时间衰减 × (1 + 0.3·被引用次数)`，半衰期 30 天。检索按 `0.55·向量 + 0.25·BM25 + 0.20·重要度` 融合排序，取 top 12 交给模型再筛。

**删除**：`/forget @某人` 清该人全部事实与画像；`/forget all` 清本群；原始消息按天自动清。删除立即生效并写审计日志。

### 4.4 发言决策

分两级，**便宜的先跑**：

**第一级 规则闸门（0 成本）**

- `addressed = true` → 直接进 Replyer（可配 `inevitable_at_reply`）。
- 已 `muted` → 停。
- 距上次发言 < `cooldown`（默认 **5s**）→ 不丢弃，进发件队列按序发出（上限 `groups.outbox_max`，默认 3）。
- 小时 / 日上限默认 **-1（不限）**；想收紧再设 `max_per_hour` / `max_per_day`。
- 当前处于「其他人正在快速对话」（近 20s 内 ≥ 3 条且来自 ≥ 2 人）→ 降权而非必停。

**第二级 Planner（小模型，~300 token）**

输入：最近 20 条消息、本群画像摘要、候选记忆 top 5、当前时间、自己的上次发言。输出严格 JSON：

```json
{"action":"reply|wait|stop","reason":"...","target_msg_id":"...","interest":0.0,"confidence":0.0}
```

`confidence < 0.4` 一律降为 wait。

**第三级 触发函数（决定这次到底开不开口）**

Planner 给的 `interest` 和「距上次发言的沉默时长」一起进函数：

```text
base  = interest ^ gamma
idle  = 1 - 0.5 ^ (silence / half_life)
p     = talk_value × (base + (1 - base) × idle × idle_weight)
```

- 两个输入都单调：越感兴趣、越久没说话 → p 越大
- 兴趣拉满时 p = talk_value；兴趣为零但沉默很久时 p = talk_value × idle_weight
- 概率是**每次决策机会**的，不是每条消息的——防抖已经把消息合并成批

默认参数（`talk_value=0.5`、`half_life=600s`、`idle_weight=0.35`、`gamma=0.8`）下实测：

| interest \ 沉默 | 0s | 30s | 5min | 30min |
|---|---|---|---|---|
| 0.2 | 0.138 | 0.142 | 0.175 | 0.249 |
| 0.5 | 0.287 | 0.290 | 0.309 | 0.352 |
| 0.8 | 0.418 | 0.419 | 0.427 | 0.443 |

参数在 `[trigger]` 段：`idle_half_life_secs` / `idle_weight` / `interest_gamma` / `min_probability`。

**防抖**：新消息进来后等 `debounce`（安静 2s / 活跃 5s，可配）成批再判，避免把一句拆成三段追着回。

**连续不发言退避**：连续 3 次 wait 后，检查间隔指数退避（15s → 30s → 60s，上限 300s）；有新 @ 则立即打断退避。

**输出过滤**：长度上限（默认 60 字，可配）、去掉开头 `@`、与最近 30 条群消息做 8-gram 重合度检查（> 0.6 视为复读，重写或丢弃）、命中敏感词直接丢弃并记日志。

### 4.5 表达学习（参考 MaiBot）

学的是**表达方式**，不是**表达内容**。四步：

1. **抽取**：从消息流里挑"有表达价值"的句子（不是每条都学），抽成 `{intent, sample, tags}`。`intent` 是"这句话在干什么"——吐槽、附和、阴阳、接梗、报怨、装傻，不是原话。
2. **自检**：入库前让模型判 `keep / reject`，拒掉人身攻击、隐私、露骨、纯复读、无信息量的。默认 `expression_checked_only = false`，可切成"只用人工精选"。
3. **存**：`intent` 做 embedding，`sample` 存原文，附 `source_qq`（仅统计用，不对外）。
4. **召回**：生成前拿当前 `intent` 向量检索 top 20，让模型挑 1–3 条作为 few-shot，**要求改写不复读**。

**风格画像**：每 500 条消息或每天重写一次本群画像——平均句长、标点习惯、语气词分布、常用梗、表情使用率。画像进 system prompt，权重高于样本。

**关于"模仿某个普通用户"**：支持 `persona.source = "group" | "member:<QQ>"`。选 `member` 时，抽取阶段只取该用户的样本，并额外生成一份「此人语言习惯」摘要；同时硬性保留三条边界——**不冒名、不复读原话、不替他表态**。这条是设计约束，不是可配开关。

### 4.6 黑话/梗

单独一类，带一句话解释，避免模型望文生义。命中时作为 `jargon` 块注入上下文，与表达样本分开管理（梗会过期，表达方式不会）。

---

### 4.7 人物设定

`[persona]` 定义「它本来就是谁」：`identity` / `personality` / `behavior` / `speech` / `taboos`，
渲染成一段 system 块送进模型，留空的段落不会输出。`/persona` 可查看当前生效的文本。

与表达学习的分工：**persona 定骨架，表达学习补皮肉**。persona 优先，学来的样本不能覆盖 `taboos`。

---

### 4.8 图片与表情包（P2 之后追加）

- 图片在入库时由多模态模型转述一次，写成 `[图片:画面 / 文字:OCR]`，原始引用丢弃
- 转述模型可单独配（`model.caption`），留空则复用 `chat` 的视觉路由
- 系统表情（`face`）走内置 id 表，不调用模型
- 判定为「可复用」的表情包：临时落盘 → sha256 → 命中则删临时文件，未命中则转正
- 收藏上限 `sticker.max_count`，按 usage/last_used 淘汰；发送用 `base64://`
- 外链图片被提供商拒绝（400）时自动降级为内联 base64，并记住该提供商的选择

## 5. 数据模型（SQLite）

```sql
-- msg_id 不在库上做 UNIQUE：去重按 msg_id + 时间窗口（onebot.dedup_window_secs），
-- 避免协议端重启后 ID 复用把新消息误判成重复。
messages(id INTEGER PK, msg_id INTEGER, group_id INT, user_id INT,
         nickname TEXT, card TEXT, ts INT, text TEXT, segments TEXT,
         reply_to INT, addressed INT)
users(qq INTEGER PK, aliases TEXT, first_seen INT, last_seen INT,
      msg_count INT, style_weight REAL)
facts(id INTEGER PK, group_id INT, subject_qq INT, scope TEXT,
      content TEXT, confidence REAL, importance REAL, base_score INT,
      source_msg_ids TEXT, created_at INT, last_access INT, superseded_by INT)
fact_vec(fact_id INTEGER PK, dim INT, vec BLOB)
profiles(id INTEGER PK, group_id INT, kind TEXT, target_qq INT,
         content TEXT, updated_at INT, version INT)
styles(id INTEGER PK, group_id INT, intent TEXT, sample TEXT, tags TEXT,
       source_qq INT, status TEXT, created_at INT)
style_vec(style_id INTEGER PK, dim INT, vec BLOB)
jargon(id INTEGER PK, group_id INT, term TEXT, meaning TEXT, first_seen INT)
decisions(id INTEGER PK, group_id INT, ts INT, trigger TEXT, action TEXT,
          reason TEXT, confidence REAL, facts_used TEXT, sent_msg_id INT)
```

`messages.text` 另建 FTS5 虚拟表做关键词。所有表带 `group_id` 便于按群清理。

---

## 6. 配置（单文件 TOML，分层 + 热重载）

```toml
[bot]
qq = 10001
nickname = "阿鱼"
aliases = ["鱼鱼", "阿鱼"]

[onebot]
listen = "0.0.0.0:3001"
path = "/onebot"
access_token_env = "ONEBOT_TOKEN"   # 变量缺失/为空 → 拒绝连接；设成 "" 才关闭鉴权
action_timeout_secs = 10
heartbeat_timeout_secs = 30        # 0 = 不检测
dedup_window_secs = 600

[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"

[models.flash]
provider = "deepseek"
model = "deepseek-v4.1-flash"
visual = true
thinking = "off"

[models.flash-think]
provider = "deepseek"
model = "deepseek-v4.1-flash"
thinking = "on"
reasoning_effort = "high"

[model]
chat    = ["flash"]
planner = ["flash"]
summary = ["flash"]
caption = ["flash"]
timeout_secs = 30
max_retries = 2

[embedding]
enabled = false
model = "bge"          # 引用 [models.bge]
dim = 1024

[groups]
whitelist = [123456789]
blacklist_users = []
talk_value = 0.5                  # 0 完全闭嘴，1 想说就说
max_per_hour = -1                 # -1 = 不限
max_per_day = -1                  # -1 = 不限
cooldown_secs = 5
debounce_quiet_ms = 2000
debounce_active_ms = 5000
inevitable_at_reply = true
inevitable_when_named = true
max_reply_chars = 60
dry_run = false

[[groups.overrides]]
group_id = 987654321
talk_value = 0.8
max_per_hour = 10

[planner]
min_confidence = 0.4
max_batch = 12
recall_top_k = 5

[trigger]
idle_half_life_secs = 600
idle_weight = 0.35
interest_gamma = 0.8
min_probability = 0.0

[persona]
name = "半拍"
identity = "一个在群里混了很久的普通群友，不是客服，也不是助手。"
personality = "话不多，偶尔毒舌，对具体的事比对人有耐心。"
behavior = "不硬找话题；别人聊得起劲时不插嘴；被问到就答，答不上来就说不知道。"
speech = "短句为主，少用书面语，不滥用感叹号。"
taboos = ["不替别人表态", "不复读别人的原话", "不聊政治和隐私"]

[memory]
retain_days = 30
extract_batch_size = 80
extract_interval_mins = 15
recall_top_k = 12
vector_threshold = 0.85
recent_context = 20

[prompts]
dir = "prompts"

[expression]
enabled = true
learn = true
use = true
checked_only = false
max_per_reply = 2
persona_source = "group"          # 或 "member:10086"

[style]
profile_rebuild_every = 500       # 条消息

[admin]
qq = [10000]
```

规则：所有叶子字段有默认值；启动时校验类型与范围，非法直接退出并指出行号（防止"配置写错静默用默认值"）。敏感值只认环境变量，不落配置文件。改文件 500ms 内热重载，标 `restart_required` 的字段除外。

---

## 7. 管理命令（群内，仅 `admin.qq`）

| 命令 | 作用 |
|---|---|
| `/ping` | 连通性 + 当前延迟 |
| `/status` | 今日消息数、发言数、token、预算剩余 |
| `/why` | 回放最近一次决策：命中哪条规则、用了哪些记忆、为什么说/不说 |
| `/why <msg_id>` | 指定某条 |
| `/mute [分钟]` | 静音（默认 30 分钟） |
| `/unmute` | 解除 |
| `/forget <@某人\|all>` | 删记忆（已实现） |
| `/reload` | 重载配置与提示词 |
| `/persona group\|member <QQ>` | 切换模仿对象 |
| `/stats 7d` | 近 7 天发言/沉默比、平均置信度 |

---

## 8. 里程碑与验收

每个里程碑单独可跑、可回滚。

**P0 骨架（已完成 2026-10-23）**
- [x] NapCat 连上，群里 `/ping` 有回应（mock 集成测试覆盖）
- [x] 白名单外的群消息**零写库**（测试：加群 A 白名单，群 B 发 10 条，`messages` 里 B 为 0）
- [ ] 断线重连（反向 WS 下由 NapCat 主动重连，需真机联调确认）
- [x] `/mute` 生效

**P1 记忆 + 被叫才答（机制已完成 2026-10-23，验收待真实群跑）**
- [ ] 群里聊过的具体事实，3 天后 @ 它能答对（准备 20 条考题，≥ 15 条正确）——待真实群验证
- [ ] 换昵称后仍能正确归因（改群名片后提问"我以前说过啥"）
- [x] `/why` 能列出支撑该回答的记忆
- [ ] `/forget` 后相关事实检索不到

**P2 主动插话（机制已完成 2026-10-23，调参待真实数据）**
- [ ] 干跑 3 天，`decisions` 表里 `reply` 占比落在 3%–8%（待真实群数据）
- [ ] 人工抽检 50 条 `reply` 决策，误插率 < 20%（待真实群数据）
- [ ] 单群 24h 发言数落在人工预期区间（默认不设硬上限，靠 `talk_value` 调）
- [x] 被 @ 必回（除非 muted）

**P3 表达学习（约 2 天）**
- [ ] 盲测：把 bot 的 10 条回复混进真实聊天记录，让人猜哪条是 bot，正确率 < 70%
- [ ] 与最近群消息 8-gram 重合度 > 0.6 的回复为 0
- [ ] 人工精选开关生效

**长期**：跑两周后，群友主动跟它说话的比例上升 = 成功；有人要求"把它踢了" = 失败。

---

## 9. 已定事项（2026-10-23 确认）

| # | 事项 | 结论 |
|---|---|---|
| 1 | 模型接口 | OpenAI 兼容、多提供商；每个功能可用「提供商:模型」列表配优先级，失败自动下一个 |
| 2 | embedding | **任意模型**：`local` / OpenAI 兼容 / `none` 三档，配置切换；`none` 时降级为关键词 + 时间衰减 |
| 3 | 项目名 | `hanpai-in-group` |
| 4 | 存储 | SQLite 单文件 + WAL，不做 Redis/Postgres |
| 5 | 记忆保留 | 默认 30 天 |
| 6 | 管理界面 | 第一版无 WebUI，TOML + 群内命令 |

---

## 10. 已知限制

- 单进程单账号；进程挂了就掉线，靠 systemd `Restart=always` 兜。
- 无分布式锁，别对同一个 QQ 同时跑两份。
- 表达学习会"学坏"：群里骂战多，它也会变冲。有自检和精选开关兜底，但不能保证。
- 决策质量取决于 `talk_value` 调参，这部分只能靠干跑数据慢慢磨，不是写完就好的。
- 记忆抽取是异步的，刚说完的话可能 1–15 分钟后才进长期记忆。
