# hanpai-in-group

> 自用项目，未经过任何系统性测试，经常有破坏性更变，功能不通用，强烈不建议用于生产环境。
> 
> 本软件仅供学习参考，切勿用于违反其他平台相关规定的场景。

一个用 Rust 写的 QQ 群聊机器人：通过 OneBot v11 接入，在自己白名单群里有长期记忆、能判断该不该插话、说话像群里的普通用户。

> 当前进度：**P2 + 视觉**。接入、白名单、落库、长期记忆、被叫回答、自主插话、图片理解、表情包收藏都已可用；剩下是拿真实群数据调参。

完整需求见 [docs/REQUIREMENTS.md](docs/REQUIREMENTS.md)。

## 快速开始

[`config.example.toml`](config.example.toml) 里每个字段都有逐行注释（含义、类型、默认值、调大了会怎样），可以直接当字段手册用。

```bash
cp config.example.toml config.toml
# 改 config.toml：bot.qq / groups.whitelist / admin.qq
export ONEBOT_TOKEN=你的token
export DEEPSEEK_API_KEY=你的key
cargo run --release
```

启动后监听 `0.0.0.0:3001/onebot`，等 NapCat 连进来。

### NapCat 侧

网络配置 → 新建 **WebSocket 客户端**：

- URL：`ws://<你的服务器IP>:3001/onebot`
- Token：与 `ONEBOT_TOKEN` 一致（反向 WS 握手会带 `Authorization: Bearer`）
- 消息格式：`array`（NapCat 默认）；`string` 也兼容，CQ 码会被解析

> 鉴权是 fail-closed 的：只要 `onebot.access_token_env` 指向的变量缺失或为空，连接会被拒绝。
> 本机临时调试不想鉴权，把 `access_token_env` 设成 `""`。

连上后群里的消息会开始落库。

### 工具链

当前在 rustc 1.85 下编译通过。`Cargo.lock` 把 `url` / `idna` / `idna_adapter` 固定在兼容 1.85 的版本（新版 icu 依赖要求 1.88+）。等你把服务器上的 Rust 升上去，`cargo update` 放开即可。

```bash
cargo test          # 单元 + 端到端（mock NapCat + mock 模型服务）
cargo build --release
```

### 环境变量

| 变量 | 作用 | 默认 |
|---|---|---|
| `HANPAI_CONFIG` | 配置文件路径 | `./config.toml` |
| `HANPAI_DB` | SQLite 路径 | `./hanpai.db` |
| `ONEBOT_TOKEN` | WS 鉴权 token；配了变量名但变量缺失会拒绝连接 | 无则不鉴权 |
| `RUST_LOG` | 日志级别 | `info,hanpai_in_group=debug` |

## 群内命令

只有 `admin.qq` 里的人能用：

| 命令 | 作用 |
|---|---|
| `/ping` | 连通性 + API 延迟 |
| `/status` | 运行状态、近 24h 消息数、当前群策略 |
| `/persona` | 查看当前生效的人物设定（送进模型的那段） |
| `/mute [分钟]` | 静音本群（仍继续记录消息） |
| `/unmute` | 解除 |
| `/reload` | 热重载配置 |
| `/why` | 回放最近一次判断：为什么说/不说、用了哪些记忆 |
| `/forget all` / `/forget <QQ>` | 删掉本群（或某人的）记忆，原始消息不受影响 |

## 怎么决定说不说

```text
消息进来 → 落库 → 攒 2~5 秒（防抖）
                    ↓
            Planner 小模型判断：值不值得接？
                    ├─ wait  → 记日志，这次不吭声
                    └─ reply → 给出 interest
                                  ↓
                        触发函数 p = talk_value × (兴趣^γ + 沉默加权)
                                  ↓
                        掷骰子 → 生成 → 发送 / 干跑记录
```

- **被 @、被叫名字或被引用回复**：跳过 Planner 和概率，直接回（仍受冷却约束）
- **Planner 只判断，不说话**：它输出兴趣值，真正说不说由物理参数决定
- **每次判断都落库**：`/why` 能看到「为什么这次没说话」，这是调参的唯一依据

## 日志与数据

```text
工作目录/
├── hanpai.db          SQLite：消息、记忆、决策、token 统计
├── logs/hanpai.YYYY-MM-DD.log   按天切分，保留 log.max_days 天
└── data/stickers/     收藏的表情包
```

- 决策数值（`interest` / `probability` / `roll` / 沉默时长 / 当日已发条数）打在 **debug** 级别，
  默认配置已把 `hanpai_in_group` 开到 debug，所以会进文件
- `[logging] dir` 设为空字符串则只输出控制台
- 超过 `memory.retain_days` 的原始消息每 6 小时自动清理一次（启动时也会清一遍）
- `/status` 最后一行显示最近 24 小时的 token（输入/输出）
- 备份前先 `sqlite3 hanpai.db "PRAGMA wal_checkpoint(TRUNCATE);"`，再 `.backup`

## 图片与表情包

图片只在**入库时**被看一眼，之后永远只是文本：

```text
图片消息 → 取临时 URL → 多模态模型转述
            ├ 普通图：就变成 [图片:画面描述 / 文字:OCR]
            └ 表情包：额外判断「可复用」
                        ├ 是 → 下载一次存进 data/stickers/
                        └ 否 → 什么都不留
```

- 原始图片和 URL **不落库**，消息表里只有转述后的文字
- 记忆抽取、检索、Planner、回复全都只读文本，天然支持图片内容
- 系统表情（`face` 段）查内置 id 表，不花模型调用
- 表情包收藏有上限（默认 64 个/群），LRU 淘汰；发送时用 `base64://`，跨机也能发
- 提供商不吃外链图片时（HTTP 400），会自动改成内联 base64 重试一次，并记住这个选择

## 模型与提供商

先声明、后调用。`[providers.*]` 定义接口地址，`[models.*]` 给模型起名字，`[model]` 里只写名字：

```toml
[providers.deepseek]
base_url = "https://api.deepseek.com"
api_key_env = "DEEPSEEK_API_KEY"

[models.flash]
provider = "deepseek"
model = "deepseek-v4.1-flash"   # 传给提供商的真实 id
visual = true                   # 能直接吃图片
thinking = "off"                # 群聊回复，关掉思考

[models.flash-think]            # 同一个模型，开思考，给需要动脑的场合
provider = "deepseek"
model = "deepseek-v4.1-flash"
thinking = "on"
reasoning_effort = "high"

[model]
chat = ["flash"]
planner = ["flash", "flash-think"]   # 按顺序失败重试
summary = ["flash"]
caption = ["flash"]
```

- 路由是**静态优先级**：写在前面的永远先试，失败才轮到下一个
- 同一个真实模型可以声明多份，区别只在 thinking / temperature 这些参数上
- 密钥缺失的提供商会启动时被跳过并告警，不会拖垮整个进程
- `/status` 最后一行打印当前生效的链路，例如 `chat=flash(deepseek-v4.1-flash)·视觉·不思考`

### 思考模式

对齐 DeepSeek 的 [思考模式文档](https://api-docs.deepseek.com/zh-cn/guides/thinking_mode/)：

| `thinking` | 下发的字段 | 说明 |
|---|---|---|
| `"on"` | `{"thinking":{"type":"enabled"}}` + `reasoning_effort` | 开思考，强度由 effort 控制 |
| `"off"` | `{"thinking":{"type":"disabled"}}` | 关思考，最快最便宜 |
| `"auto"` | 什么都不发 | 跟随提供商默认（DeepSeek 默认开，effort=high） |

`reasoning_effort` 支持 `minimal / low / medium / high / xhigh / max / ultra`，DeepSeek 侧会把
minimal/low 映射成 low，medium/high/xhigh 映射成 high，max/ultra 映射成 max。

思考模式下 `temperature` 不生效，所以这时我们**不下发**这个字段；其余情况照常传。

## 记忆怎么工作

1. 消息先落库，原样保留 `retain_days` 天。
2. 后台每 `extract_batch_size` 条（或每 `extract_interval_mins` 分钟）抽一次原子事实：小模型读一批聊天记录，输出结构化 JSON，向量去重后入库。
3. 被叫到时检索：向量相似度 0.55 + 关键词命中 0.25 + 重要度与时间衰减 0.20，取 top-k 交给模型。
4. 生成的回复会带上这些记忆，`/why` 能回放这次到底用了哪几条。

水位（处理到哪条消息）存在 `meta` 表里，重启不会重复抽。抽取是异步的，刚说的话最多等一个批次才会进长期记忆。

提示词在 `prompts/` 下（`extract.md` / `reply.md`），改完 `/reload` 生效，不用重新编译。

## 人物设定

`[persona]` 是"它本来就是谁"：身份、性格、行为准则、说话风格、禁区。留空的段落不会进提示词，
用多行字符串写长一点也没问题。群内 `/persona` 可以随时看当前生效的文本。

```toml
[persona]
name = "半拍"
identity = "一个在群里混了很久的普通群友，不是客服。"
personality = "话不多，偶尔毒舌。"
behavior = "不硬找话题；别人聊得起劲时不插嘴。"
speech = "短句为主，不滥用感叹号。"
taboos = ["不替别人表态", "不复读别人的原话"]
```

## 触发概率

发言不是"判到就发"，而是先算一个概率：

```text
base = interest ^ gamma
idle = 1 - 0.5 ^ (沉默秒数 / half_life)
p    = talk_value × (base + (1 - base) × idle × idle_weight)
```

越感兴趣、越久没说话，p 越大；`talk_value` 是总闸，`[trigger]` 里三个旋钮调形状。
默认参数下 interest=0.5、刚说完话时约 29%，沉默 30 分钟后升到 35%。概率按**每次决策机会**算，
不是每条消息——防抖已经把消息合并成批了。

小时/日发言上限默认 **-1（不限）**，需要收紧再设 `max_per_hour` / `max_per_day`。

## 代码结构

```
src/
├── onebot/          # 可复用的 OneBot v11 协议层（不依赖本项目配置/数据库）
│   ├── event.rs     # 事件、消息段、CQ 码、@ / 引用判定
│   ├── api.rs       # action 调用（echo + pending map）
│   └── server.rs    # 反向 WS：鉴权、角色、心跳、事件队列
├── ws.rs            # 业务适配：白名单、落库、决策链
├── decision.rs      # Planner + 触发函数
├── memory.rs        # 记忆抽取与检索
└── ...
```

`onebot` 只依赖 serde / tokio / axum 的 WebSocket 类型，换项目时整个目录拷走即可；
`Server::accept()` 返回的 `Connection` 用 `recv()` 取事件，读循环在后台跑，业务处理慢也不会堵住 action 回包。

## 设计要点

- **白名单是硬闸门**：非白名单群在解析后立刻返回，不产生任何数据库写入。
- **反向 WS 即双向通道**：事件从 NapCat 推来，调用 API 也走同一条连接（`{action, params, echo}` + pending map 配对），不需要额外的 HTTP 端口。
- **协议层按规范做防守**：`message` 兼容数组/字符串（CQ 码就地解析）；@ 判定按消息段判等而不是文本子串；引用回复通过自身 message_id 归因；校验 `retcode`；`X-Self-ID` / `X-Client-Role` 都认；本地发言也回写，避免复读自己。
- **消息去重是时间窗口**：同一 `message_id` 只在 `dedup_window_secs`（默认 10 分钟）内算重复，协议端重启复用 ID 不会吞新消息。
- **SQLite 单文件 + WAL**：目标规模是个位数群、单群百人，不需要 Redis/Postgres。
- **不需要向量数据库**：记忆量 10⁵ 以内，暴力余弦完全够。
- **配置校验一次报全**：非法配置启动即失败，不允许静默用默认值。
- **dry_run 默认开**：先只记决策不发出去。

## 测试

```bash
cargo test
```

集成测试用真实 TCP + 真实 SQLite，mock 一个 NapCat 客户端，覆盖白名单闸门、消息去重、管理命令、鉴权。

## 路线图

- [x] **P0** 接入 / 白名单 / 落库 / 管理命令
- [x] **P1** 记忆抽取、检索、被叫时带记忆回答
- [x] **P2** 发言决策（Planner + 触发函数 + 防抖 + 决策日志），调参待真实数据
- [ ] **P3** 表达学习、群风格画像、模仿

## 许可证

[GPL-3.0-only](LICENSE)。

简单说：你可以自由使用、修改、分发，但分发时必须开源且保持同样的许可证。本软件按“现状”提供，不附带任何担保。
