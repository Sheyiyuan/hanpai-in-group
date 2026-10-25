-- 去重从「永久唯一」改成「时间窗口」。
--
-- 原来 messages.msg_id 是 UNIQUE，协议端重启后 message_id 复用、或 32 位 ID 撞车时，
-- 新消息会被当成重复静默丢掉。改成在 db::insert_message 里按
-- onebot.dedup_window_secs 判定窗口内的重复。
--
-- SQLite 不能直接删 UNIQUE 约束，只能重建表；FTS5 是 external content 表，
-- 需要在重建后 rebuild 一次。

DROP TRIGGER IF EXISTS messages_ai;
DROP TRIGGER IF EXISTS messages_ad;
DROP TRIGGER IF EXISTS messages_au;

CREATE TABLE messages_new (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    msg_id      INTEGER NOT NULL,
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

INSERT INTO messages_new
    (id, msg_id, group_id, user_id, nickname, card, ts, text, segments, reply_to, addressed)
SELECT id, msg_id, group_id, user_id, nickname, card, ts, text, segments, reply_to, addressed
FROM messages;

DROP TABLE messages;
ALTER TABLE messages_new RENAME TO messages;

CREATE INDEX IF NOT EXISTS idx_messages_group_ts ON messages(group_id, ts);
CREATE INDEX IF NOT EXISTS idx_messages_user ON messages(user_id);
CREATE INDEX IF NOT EXISTS idx_messages_msg_id_ts ON messages(msg_id, ts);

CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
END;

CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
END;

CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, text) VALUES ('delete', old.id, old.text);
    INSERT INTO messages_fts(rowid, text) VALUES (new.id, new.text);
END;

INSERT INTO messages_fts(messages_fts) VALUES ('rebuild');
