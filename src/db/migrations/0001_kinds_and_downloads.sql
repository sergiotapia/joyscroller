-- What kind of post each row is and when it was posted, and the
-- downloads table. TikTok rows already have both facts in raw_json.
-- YouTube's listing has no post date, so posted_at is filled in from the
-- download's metadata later.

ALTER TABLE videos ADD COLUMN kind TEXT NOT NULL DEFAULT 'video';  -- 'video' | 'photo' (TikTok slideshow)
ALTER TABLE videos ADD COLUMN posted_at INTEGER;                   -- unix seconds; NULL until known
UPDATE videos SET kind = 'photo'
  WHERE platform = 'tiktok' AND json_type(raw_json, '$.imagePost') IS NOT NULL;
UPDATE videos SET posted_at = json_extract(raw_json, '$.createTime')
  WHERE platform = 'tiktok';
CREATE TABLE downloads (
    platform        TEXT NOT NULL,
    video_id        TEXT NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,  -- failed tries so far
    last_attempt_at INTEGER,
    last_error      TEXT,
    downloaded_at   INTEGER,             -- NULL = not downloaded yet
    dir             TEXT,                -- the video's folder, relative to the media root
    files           TEXT,                -- JSON array of the file names in dir
    bytes           INTEGER,             -- total size of files
    format          TEXT,                -- the formats yt-dlp picked, e.g. '399+251'
    info_json       TEXT,                -- yt-dlp's metadata at download time, minus formats and URLs
    PRIMARY KEY (platform, video_id)
);
