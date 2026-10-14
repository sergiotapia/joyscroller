-- Playback copies (see `transcode.rs`), one row per downloaded video
-- once one has been tried. A row is about the download in
-- `download_folder`: a new download of the video deletes it, so the new
-- files get a new copy.

CREATE TABLE playback_copies (
    platform             TEXT NOT NULL,
    video_id             TEXT NOT NULL,
    download_folder      TEXT NOT NULL,   -- the download's folder it's made from, relative to the media root
    failures             INTEGER NOT NULL DEFAULT 0,  -- failed tries so far
    last_attempt_at      INTEGER,
    last_error           TEXT,
    finished_at          INTEGER,         -- NULL = not done yet
    file_name            TEXT,            -- the copy's name in the folder; NULL when the original plays everywhere
    size_bytes           INTEGER,
    method               TEXT,            -- 'none' | 'remux' | 'encode'
    original_config_json TEXT,            -- the original's MediaCapabilities video configuration
    PRIMARY KEY (platform, video_id)
);
