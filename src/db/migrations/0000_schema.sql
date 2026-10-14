-- The original schema, as the first release made it. Never edit it, nor any
-- step that has shipped: changes are new steps, so new databases and old
-- ones end up in the same shape by the same steps.

CREATE TABLE IF NOT EXISTS videos (
    platform         TEXT NOT NULL,      -- 'youtube' | 'instagram' | 'tiktok'
    video_id         TEXT NOT NULL,      -- the platform's own id
    url              TEXT NOT NULL,
    title            TEXT,
    author_id        TEXT,
    author_name      TEXT,
    duration_seconds INTEGER,
    available        INTEGER NOT NULL DEFAULT 1,  -- 0 = private or deleted on the platform
    raw_json         TEXT NOT NULL,      -- full entry from yt-dlp or the platform's API, as first seen
    liked_at         INTEGER NOT NULL,   -- unix seconds: the sync that first found it
    PRIMARY KEY (platform, video_id)
);

CREATE TABLE IF NOT EXISTS syncs (
    id          INTEGER PRIMARY KEY,
    platform    TEXT NOT NULL,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER,                 -- NULL = run crashed or is still going
    added       INTEGER
);
