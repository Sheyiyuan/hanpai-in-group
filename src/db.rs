//! SQLite 持久化。P0 只用到消息与用户，其余表已按规格建好，后续阶段直接用。

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;

pub async fn connect(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await
        .with_context(|| format!("打开数据库失败 {}", path.display()))?;
    Ok(pool)
}

pub async fn migrate(pool: &SqlitePool) -> Result<()> {
    sqlx::migrate!("./migrations")
        .run(pool)
        .await
        .context("执行数据库迁移失败")?;
    Ok(())
}

pub struct NewMessage<'a> {
    pub msg_id: i64,
    pub group_id: i64,
    pub user_id: i64,
    pub nickname: &'a str,
    pub card: &'a str,
    pub ts: i64,
    pub text: &'a str,
    pub segments: &'a str,
    pub reply_to: Option<i64>,
    pub addressed: bool,
}

/// 返回 true 表示是新消息；false 表示同一 msg_id 在去重窗口内已经收过了。
///
/// 去重不靠 UNIQUE 约束，而是「同一 msg_id 且 ts 落在窗口内」才算重复：
/// 协议端重启 / message_id 复用时不至于把新消息误判成重复（见迁移 0003）。
pub async fn insert_message(
    pool: &SqlitePool,
    m: &NewMessage<'_>,
    window_secs: u64,
) -> Result<bool> {
    let since = m.ts.saturating_sub(window_secs as i64);
    let dup: Option<(i64,)> =
        sqlx::query_as("SELECT 1 FROM messages WHERE msg_id = ? AND ts >= ? LIMIT 1")
            .bind(m.msg_id)
            .bind(since)
            .fetch_optional(pool)
            .await?;
    if dup.is_some() {
        return Ok(false);
    }

    let res = sqlx::query(
        r#"INSERT INTO messages
           (msg_id, group_id, user_id, nickname, card, ts, text, segments, reply_to, addressed)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
    )
    .bind(m.msg_id)
    .bind(m.group_id)
    .bind(m.user_id)
    .bind(m.nickname)
    .bind(m.card)
    .bind(m.ts)
    .bind(m.text)
    .bind(m.segments)
    .bind(m.reply_to)
    .bind(i64::from(m.addressed))
    .execute(pool)
    .await?;

    Ok(res.rows_affected() > 0)
}

/// 这条消息是谁发的。用于判断「引用回复的是不是机器人自己」。
pub async fn message_sender(pool: &SqlitePool, msg_id: i64) -> Result<Option<i64>> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT user_id FROM messages WHERE msg_id = ? ORDER BY ts DESC LIMIT 1")
            .bind(msg_id)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(id,)| id))
}

/// 用户画像的骨架：QQ 号为主键，昵称/群名片变化记进 aliases 历史。
pub async fn upsert_user(pool: &SqlitePool, qq: i64, name: &str, ts: i64) -> Result<()> {
    let existing: Option<(String,)> = sqlx::query_as("SELECT aliases FROM users WHERE qq = ?")
        .bind(qq)
        .fetch_optional(pool)
        .await?;

    match existing {
        None => {
            let aliases = json!([{ "name": name, "from": ts }]).to_string();
            sqlx::query(
                "INSERT INTO users (qq, aliases, first_seen, last_seen, msg_count)
                 VALUES (?, ?, ?, ?, 1)
                 ON CONFLICT(qq) DO UPDATE SET
                   last_seen = excluded.last_seen,
                   msg_count = users.msg_count + 1",
            )
            .bind(qq)
            .bind(aliases)
            .bind(ts)
            .bind(ts)
            .execute(pool)
            .await?;
        }
        Some((raw,)) => {
            let mut arr: Vec<Value> = serde_json::from_str(&raw).unwrap_or_default();
            let last_name = arr
                .last()
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let changed = !name.trim().is_empty() && last_name != name;

            if changed {
                arr.push(json!({ "name": name, "from": ts }));
                let aliases = serde_json::to_string(&arr)?;
                sqlx::query(
                    "UPDATE users SET aliases = ?, last_seen = ?, msg_count = msg_count + 1 WHERE qq = ?",
                )
                .bind(aliases)
                .bind(ts)
                .bind(qq)
                .execute(pool)
                .await?;
            } else {
                sqlx::query(
                    "UPDATE users SET last_seen = ?, msg_count = msg_count + 1 WHERE qq = ?",
                )
                .bind(ts)
                .bind(qq)
                .execute(pool)
                .await?;
            }
        }
    }
    Ok(())
}

pub async fn count_since(pool: &SqlitePool, group_id: i64, since: i64) -> Result<i64> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM messages WHERE group_id = ? AND ts >= ?")
            .bind(group_id)
            .bind(since)
            .fetch_one(pool)
            .await?;
    Ok(n)
}

/// 只数别人说的话（排除机器人自己回写的那部分），给 /status 用。
pub async fn count_incoming_since(
    pool: &SqlitePool,
    group_id: i64,
    since: i64,
    self_id: i64,
) -> Result<i64> {
    if self_id <= 0 {
        return count_since(pool, group_id, since).await;
    }
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM messages WHERE group_id = ? AND ts >= ? AND user_id != ?",
    )
    .bind(group_id)
    .bind(since)
    .bind(self_id)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// 元数据
// ---------------------------------------------------------------------------

pub async fn meta_get(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM meta WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(v,)| v))
}

pub async fn meta_set(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO meta (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 记忆事实
// ---------------------------------------------------------------------------

pub struct NewFact<'a> {
    pub group_id: i64,
    pub subject_qq: i64,
    pub scope: &'a str,
    pub content: &'a str,
    pub confidence: f32,
    pub importance: f32,
    pub base_score: i64,
    pub source_msg_ids: &'a str,
    pub created_at: i64,
}

pub async fn insert_fact(pool: &SqlitePool, f: &NewFact<'_>) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO facts
         (group_id, subject_qq, scope, content, confidence, importance, base_score,
          source_msg_ids, created_at, last_access, superseded_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL)",
    )
    .bind(f.group_id)
    .bind(f.subject_qq)
    .bind(f.scope)
    .bind(f.content)
    .bind(f.confidence)
    .bind(f.importance)
    .bind(f.base_score)
    .bind(f.source_msg_ids)
    .bind(f.created_at)
    .bind(f.created_at)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn set_fact_vec(pool: &SqlitePool, fact_id: i64, dim: i64, blob: &[u8]) -> Result<()> {
    sqlx::query(
        "INSERT INTO fact_vec (fact_id, dim, vec) VALUES (?, ?, ?)
         ON CONFLICT(fact_id) DO UPDATE SET dim = excluded.dim, vec = excluded.vec",
    )
    .bind(fact_id)
    .bind(dim)
    .bind(blob)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct FactRow {
    pub id: i64,
    pub content: String,
    pub importance: f32,
    pub created_at: i64,
    pub subject_qq: i64,
}

pub async fn facts_for_group(pool: &SqlitePool, group_id: i64, limit: i64) -> Result<Vec<FactRow>> {
    let rows = sqlx::query_as::<_, FactRow>(
        "SELECT id, content, importance, created_at, subject_qq
         FROM facts
         WHERE group_id = ? AND superseded_by IS NULL
         ORDER BY created_at DESC
         LIMIT ?",
    )
    .bind(group_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn fact_vecs_for_group(pool: &SqlitePool, group_id: i64) -> Result<Vec<(i64, Vec<u8>)>> {
    let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT v.fact_id, v.vec
         FROM fact_vec v
         JOIN facts f ON f.id = v.fact_id
         WHERE f.group_id = ? AND f.superseded_by IS NULL",
    )
    .bind(group_id)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn count_facts(pool: &SqlitePool, group_id: i64) -> Result<i64> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM facts WHERE group_id = ? AND superseded_by IS NULL")
            .bind(group_id)
            .fetch_one(pool)
            .await?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// 消息读取
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MsgRow {
    pub id: i64,
    pub user_id: i64,
    pub nickname: String,
    pub card: String,
    pub text: String,
    pub ts: i64,
}

impl MsgRow {
    pub fn display(&self) -> &str {
        if self.card.trim().is_empty() {
            &self.nickname
        } else {
            &self.card
        }
    }
}

pub async fn messages_after(
    pool: &SqlitePool,
    group_id: i64,
    after_id: i64,
    limit: i64,
) -> Result<Vec<MsgRow>> {
    let rows = sqlx::query_as::<_, MsgRow>(
        "SELECT id, user_id, nickname, card, text, ts
         FROM messages
         WHERE group_id = ? AND id > ?
         ORDER BY id ASC
         LIMIT ?",
    )
    .bind(group_id)
    .bind(after_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn recent_messages(pool: &SqlitePool, group_id: i64, limit: i64) -> Result<Vec<MsgRow>> {
    let mut rows = sqlx::query_as::<_, MsgRow>(
        "SELECT id, user_id, nickname, card, text, ts
         FROM messages
         WHERE group_id = ?
         ORDER BY id DESC
         LIMIT ?",
    )
    .bind(group_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    rows.reverse();
    Ok(rows)
}

pub async fn count_messages_after(pool: &SqlitePool, group_id: i64, after_id: i64) -> Result<i64> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM messages WHERE group_id = ? AND id > ?")
            .bind(group_id)
            .bind(after_id)
            .fetch_one(pool)
            .await?;
    Ok(n)
}

pub async fn oldest_ts_after(
    pool: &SqlitePool,
    group_id: i64,
    after_id: i64,
) -> Result<Option<i64>> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT ts FROM messages WHERE group_id = ? AND id > ? ORDER BY id ASC LIMIT 1",
    )
    .bind(group_id)
    .bind(after_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(ts,)| ts))
}

// ---------------------------------------------------------------------------
// 决策日志
// ---------------------------------------------------------------------------

pub struct NewDecision<'a> {
    pub group_id: i64,
    pub ts: i64,
    pub trigger: &'a str,
    pub action: &'a str,
    pub reason: &'a str,
    pub confidence: f32,
    pub facts_used: &'a str,
    pub sent_msg_id: Option<i64>,
}

pub async fn insert_decision(pool: &SqlitePool, d: &NewDecision<'_>) -> Result<()> {
    sqlx::query(
        "INSERT INTO decisions
         (group_id, ts, trigger, action, reason, confidence, facts_used, sent_msg_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(d.group_id)
    .bind(d.ts)
    .bind(d.trigger)
    .bind(d.action)
    .bind(d.reason)
    .bind(d.confidence)
    .bind(d.facts_used)
    .bind(d.sent_msg_id)
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DecisionRow {
    pub ts: i64,
    pub trigger: String,
    pub action: String,
    pub reason: String,
    pub facts_used: String,
    pub sent_msg_id: Option<i64>,
}

pub async fn last_decision(pool: &SqlitePool, group_id: i64) -> Result<Option<DecisionRow>> {
    let row = sqlx::query_as::<_, DecisionRow>(
        "SELECT ts, trigger, action, reason, facts_used, sent_msg_id
         FROM decisions WHERE group_id = ? ORDER BY id DESC LIMIT 1",
    )
    .bind(group_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn sent_count_since(pool: &SqlitePool, group_id: i64, since: i64) -> Result<i64> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM decisions WHERE group_id = ? AND ts >= ? AND action = 'reply'",
    )
    .bind(group_id)
    .bind(since)
    .fetch_one(pool)
    .await?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// 表情包
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StickerRow {
    pub id: i64,
    pub hash: String,
    pub file: String,
    pub caption: String,
    pub emotion: String,
}

pub async fn sticker_by_hash(pool: &SqlitePool, group_id: i64, hash: &str) -> Result<Option<i64>> {
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM stickers WHERE group_id = ? AND hash = ?")
            .bind(group_id)
            .bind(hash)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(id,)| id))
}

pub async fn insert_sticker(
    pool: &SqlitePool,
    group_id: i64,
    hash: &str,
    file: &str,
    caption: &str,
    emotion: &str,
    ts: i64,
) -> Result<i64> {
    let res = sqlx::query(
        "INSERT INTO stickers (group_id, hash, file, caption, emotion, usage, created_at, last_used)
         VALUES (?, ?, ?, ?, ?, 0, ?, 0)
         ON CONFLICT(group_id, hash) DO UPDATE SET
           file = excluded.file,
           caption = excluded.caption,
           emotion = excluded.emotion",
    )
    .bind(group_id)
    .bind(hash)
    .bind(file)
    .bind(caption)
    .bind(emotion)
    .bind(ts)
    .execute(pool)
    .await?;
    Ok(res.last_insert_rowid())
}

pub async fn count_stickers(pool: &SqlitePool, group_id: i64) -> Result<i64> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM stickers WHERE group_id = ?")
        .bind(group_id)
        .fetch_one(pool)
        .await?;
    Ok(n)
}

/// 淘汰最久没用过的一张，返回它的文件路径
pub async fn evict_oldest_sticker(pool: &SqlitePool, group_id: i64) -> Result<Option<String>> {
    let row: Option<(i64, String)> = sqlx::query_as(
        "SELECT id, file FROM stickers WHERE group_id = ?
         ORDER BY usage ASC, last_used ASC, id ASC LIMIT 1",
    )
    .bind(group_id)
    .fetch_optional(pool)
    .await?;

    if let Some((id, file)) = row {
        sqlx::query("DELETE FROM stickers WHERE id = ?")
            .bind(id)
            .execute(pool)
            .await?;
        return Ok(Some(file));
    }
    Ok(None)
}

/// 挑一张最冷门的表情
pub async fn pick_sticker(pool: &SqlitePool, group_id: i64) -> Result<Option<StickerRow>> {
    let row = sqlx::query_as::<_, StickerRow>(
        "SELECT id, hash, file, caption, emotion FROM stickers
         WHERE group_id = ?
         ORDER BY usage ASC, last_used ASC, RANDOM() LIMIT 1",
    )
    .bind(group_id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn touch_sticker(pool: &SqlitePool, id: i64, ts: i64) -> Result<()> {
    sqlx::query("UPDATE stickers SET usage = usage + 1, last_used = ? WHERE id = ?")
        .bind(ts)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn sticker_count_all(pool: &SqlitePool) -> Result<i64> {
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM stickers")
        .fetch_one(pool)
        .await?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// 调用量与清理
// ---------------------------------------------------------------------------

pub async fn insert_usage(
    pool: &SqlitePool,
    ts: i64,
    role: &str,
    prompt_tokens: i64,
    completion_tokens: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO usage_log (ts, role, prompt_tokens, completion_tokens) VALUES (?, ?, ?, ?)",
    )
    .bind(ts)
    .bind(role)
    .bind(prompt_tokens)
    .bind(completion_tokens)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn usage_since(pool: &SqlitePool, since: i64) -> Result<(i64, i64)> {
    let row: (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0)
         FROM usage_log WHERE ts >= ?",
    )
    .bind(since)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

/// 删掉早于 cutoff 的原始消息，返回删除条数。FTS 由触发器同步。
pub async fn delete_messages_before(pool: &SqlitePool, cutoff: i64) -> Result<u64> {
    let res = sqlx::query("DELETE FROM messages WHERE ts < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

/// 删记忆：subject 为 None 时删整群，否则只删这个人。
pub async fn delete_facts(pool: &SqlitePool, group_id: i64, subject: Option<i64>) -> Result<u64> {
    let res = match subject {
        Some(qq) => {
            sqlx::query("DELETE FROM facts WHERE group_id = ? AND subject_qq = ?")
                .bind(group_id)
                .bind(qq)
                .execute(pool)
                .await?
        }
        None => {
            sqlx::query("DELETE FROM facts WHERE group_id = ?")
                .bind(group_id)
                .execute(pool)
                .await?
        }
    };
    Ok(res.rows_affected())
}
