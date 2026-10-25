PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    msg_id      INTEGER NOT NULL UNIQUE,
    group_id    INTEGER NOT NULL,
    user_id     INTEGER NOT NULL,
    nickname    TEXT NOT NULL DEFAULT '',
    card        TEXT NOT NULL DEFAULT '',
    ts          INTEGER NOT NULL,
    text        TEXT NOT NULL DEFAULT '',
    segments    TEXT NOT NULL DEFAULT '[]',
    reply_to    INTEGER,
    addressed   INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_messages_group_ts ON messages(group_id, ts);
CREATE INDEX IF NOT EXISTS idx_messages_user ON messages(user_id);

CREATE TABLE IF NOT EXISTS users (
    qq          INTEGER PRIMARY KEY,
    aliases     TEXT NOT NULL DEFAULT '[]',
    first_seen  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL,
    msg_count   INTEGER NOT NULL DEFAULT 0,
    style_weight REAL NOT NULL DEFAULT 1.0
);

CREATE TABLE IF NOT EXISTS facts (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id     INTEGER NOT NULL,
    subject_qq   INTEGER NOT NULL DEFAULT 0,
    scope        TEXT NOT NULL DEFAULT 'group',
    content      TEXT NOT NULL,
    confidence   REAL NOT NULL DEFAULT 0.5,
    importance   REAL NOT NULL DEFAULT 0.5,
    base_score   INTEGER NOT NULL DEFAULT 3,
    source_msg_ids TEXT NOT NULL DEFAULT '[]',
    created_at   INTEGER NOT NULL,
    last_access  INTEGER NOT NULL DEFAULT 0,
    superseded_by INTEGER
);

CREATE INDEX IF NOT EXISTS idx_facts_group ON facts(group_id, superseded_by);
CREATE INDEX IF NOT EXISTS idx_facts_subject ON facts(group_id, subject_qq);

CREATE TABLE IF NOT EXISTS fact_vec (
    fact_id INTEGER PRIMARY KEY REFERENCES facts(id) ON DELETE CASCADE,
    dim     INTEGER NOT NULL,
    vec     BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS profiles (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id   INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    target_qq  INTEGER NOT NULL DEFAULT 0,
    content    TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    version    INTEGER NOT NULL DEFAULT 1
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_profiles_unique ON profiles(group_id, kind, target_qq);

CREATE TABLE IF NOT EXISTS styles (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id   INTEGER NOT NULL,
    intent     TEXT NOT NULL,
    sample     TEXT NOT NULL,
    tags       TEXT NOT NULL DEFAULT '[]',
    source_qq  INTEGER NOT NULL DEFAULT 0,
    status     TEXT NOT NULL DEFAULT 'pending',
    created_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_styles_group ON styles(group_id, status);

CREATE TABLE IF NOT EXISTS style_vec (
    style_id INTEGER PRIMARY KEY REFERENCES styles(id) ON DELETE CASCADE,
    dim      INTEGER NOT NULL,
    vec      BLOB NOT NULL
);

CREATE TABLE IF NOT EXISTS jargon (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id   INTEGER NOT NULL,
    term       TEXT NOT NULL,
    meaning    TEXT NOT NULL DEFAULT '',
    first_seen INTEGER NOT NULL
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_jargon_unique ON jargon(group_id, term);

CREATE TABLE IF NOT EXISTS decisions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id    INTEGER NOT NULL,
    ts          INTEGER NOT NULL,
    trigger     TEXT NOT NULL,
    action      TEXT NOT NULL,
    reason      TEXT NOT NULL DEFAULT '',
    confidence  REAL NOT NULL DEFAULT 0.0,
    facts_used  TEXT NOT NULL DEFAULT '[]',
    sent_msg_id INTEGER
);

CREATE INDEX IF NOT EXISTS idx_decisions_group_ts ON decisions(group_id, ts);

-- 中文关键词检索：trigram 分词对中文比 unicode61 靠谱
CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    text,
    content='messages',
    content_rowid='id',
    tokenize='trigram'
);

CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
END;

CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;

CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
    INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
END;
