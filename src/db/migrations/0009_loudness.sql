-- How loud each downloaded video's sound is (EBU R128, measured by
-- `loudness.rs`), so /tv can play the platforms' very differently mixed
-- posts at one level. One row per downloaded video once it has been
-- measured or tried. Like a playback copy, a row is about the download in
-- `download_folder`: a new download of the video deletes it.

CREATE TABLE loudness (
    platform        TEXT NOT NULL,
    video_id        TEXT NOT NULL,
    download_folder TEXT NOT NULL,   -- the download's folder it was measured in, relative to the media root
    failures        INTEGER NOT NULL DEFAULT 0,  -- failed tries so far
    last_attempt_at INTEGER,
    last_error      TEXT,
    measured_at     INTEGER,         -- NULL = not measured yet
    integrated_lufs REAL,            -- NULL when it has no sound, or only silence
    true_peak_dbtp  REAL,
    PRIMARY KEY (platform, video_id)
);
