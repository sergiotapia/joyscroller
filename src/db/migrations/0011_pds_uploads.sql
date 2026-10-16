-- Uploading the archive to the user's account on an AT Protocol PDS
-- (`pds.rs`), as an off-site copy.

-- The connected account, one at a time: connecting another replaces it.
-- Its app password is kept in the OS keyring, never here.
CREATE TABLE pds_accounts (
    did              TEXT PRIMARY KEY,
    handle           TEXT NOT NULL,
    service_url      TEXT NOT NULL,   -- where it logs in, as typed: its PDS, or an entryway like bsky.social
    pds_url          TEXT NOT NULL,   -- where its repo lives, from its DID document
    platforms_json   TEXT NOT NULL,   -- the platforms whose posts upload, e.g. ["youtube","tiktok"]
    blob_limit_bytes INTEGER,         -- the largest file the PDS takes; NULL = not known
    added_at         INTEGER NOT NULL
);

-- One row per downloaded post and account once its upload has been tried.
-- Like a playback copy, a row is about the download in `download_folder`:
-- a new download of the post uploads again, replacing the record.
CREATE TABLE pds_uploads (
    platform        TEXT NOT NULL,
    video_id        TEXT NOT NULL,
    did             TEXT NOT NULL,
    download_folder TEXT NOT NULL,   -- the download's folder it was uploaded from, relative to the media root
    failures        INTEGER NOT NULL DEFAULT 0,  -- failed tries so far
    last_attempt_at INTEGER,
    last_error      TEXT,
    uploaded_at     INTEGER,         -- NULL = not uploaded yet
    too_big_at      INTEGER,         -- set instead when a file is over the PDS's limit; not tried again
    record_uri      TEXT,            -- at://<did>/<collection>/<platform>-<video_id>
    record_cid      TEXT,
    blobs_json      TEXT,            -- [{"file", "cid", "mime_type", "size_bytes"}]
    size_bytes      INTEGER,         -- every file's, uploaded or too big
    PRIMARY KEY (platform, video_id, did)
);
