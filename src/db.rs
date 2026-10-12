//! The local archive: a SQLite database of every video the user has liked,
//! across platforms. A row's presence means "liked". Rows keep what the video
//! looked like when we first saw it; only `available` is refreshed on each
//! sync, so a video that goes private later still has its title on file.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS videos (
    platform         TEXT NOT NULL,      -- 'youtube' | 'instagram' | 'tiktok'
    video_id         TEXT NOT NULL,      -- the platform's own id
    url              TEXT NOT NULL,
    title            TEXT,
    author_id        TEXT,
    author_name      TEXT,
    duration_seconds INTEGER,
    available        INTEGER NOT NULL DEFAULT 1,  -- 0 = private or deleted on the platform
    raw_json         TEXT NOT NULL,      -- full yt-dlp entry, as first seen
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
";

/// A liked video, in the platform-neutral shape the `videos` table stores.
pub struct NewVideo<'a> {
    pub video_id: &'a str,
    pub url: &'a str,
    pub title: Option<&'a str>,
    pub author_id: Option<&'a str>,
    pub author_name: Option<&'a str>,
    pub duration_seconds: Option<u64>,
    pub available: bool,
    pub raw_json: &'a str,
}

/// `~/.local/share/joyscroller/joyscroller.db`
pub fn default_path() -> PathBuf {
    dirs::data_dir()
        .expect("no XDG data dir")
        .join("joyscroller/joyscroller.db")
}

pub fn open(path: &std::path::Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
}

/// Record the start of a sync; returns its id for `finish_sync`.
pub fn start_sync(conn: &Connection, platform: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO syncs (platform, started_at) VALUES (?1, ?2)",
        params![platform, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn finish_sync(conn: &Connection, sync_id: i64, added: usize) -> Result<()> {
    conn.execute(
        "UPDATE syncs SET finished_at = ?1, added = ?2 WHERE id = ?3",
        params![now(), added as i64, sync_id],
    )?;
    Ok(())
}

/// Every video id already stored for `platform`.
pub fn video_ids(conn: &Connection, platform: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT video_id FROM videos WHERE platform = ?1")?;
    let ids = stmt.query_map([platform], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Insert the videos we haven't seen before; for existing rows only
/// `available` is refreshed. Pass them oldest-first so rowid order matches
/// like order.
/// Returns how many were new.
pub fn insert_new(conn: &mut Connection, platform: &str, videos: &[NewVideo]) -> Result<usize> {
    let liked_at = now();
    let tx = conn.transaction()?;
    let mut added = 0;
    {
        let mut insert = tx.prepare(
            "INSERT OR IGNORE INTO videos \
             (platform, video_id, url, title, author_id, author_name, duration_seconds, available, raw_json, liked_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        )?;
        let mut set_available =
            tx.prepare("UPDATE videos SET available = ?1 WHERE platform = ?2 AND video_id = ?3")?;
        for v in videos {
            let inserted = insert.execute(params![
                platform,
                v.video_id,
                v.url,
                v.title,
                v.author_id,
                v.author_name,
                v.duration_seconds.map(|d| d as i64),
                v.available,
                v.raw_json,
                liked_at,
            ])?;
            if inserted == 0 {
                set_available.execute(params![v.available, platform, v.video_id])?;
            }
            added += inserted;
        }
    }
    tx.commit()?;
    Ok(added)
}
