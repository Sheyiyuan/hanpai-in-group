-- 收藏的表情包：只存被判定为「可复用」的那些，普通图片转述完即弃
CREATE TABLE IF NOT EXISTS stickers (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id   INTEGER NOT NULL,
    hash       TEXT NOT NULL,
    file       TEXT NOT NULL,
    caption    TEXT NOT NULL DEFAULT '',
    emotion    TEXT NOT NULL DEFAULT '',
    usage      INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    last_used  INTEGER NOT NULL DEFAULT 0
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_stickers_hash ON stickers(group_id, hash);
CREATE INDEX IF NOT EXISTS idx_stickers_usage ON stickers(group_id, usage, last_used);
