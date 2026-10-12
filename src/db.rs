//! The local archive: a SQLite database of every video the user has liked,
//! across platforms. A row's presence means "liked". Rows keep what the video
//! looked like when we first saw it; only `available` is refreshed on each
//! sync, so a video that goes private later still has its title on file.
//! `downloads` tracks which videos have their media saved to disk.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

/// The original schema. Never edit it: changes go in `MIGRATIONS`, so that new
/// databases and old ones end up in the same shape by the same steps.
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
";

/// Schema changes since `SCHEMA`, in order. A database's `user_version`
/// says how many it has had; `open` applies the rest, each in a transaction.
/// Append new steps; never edit or reorder applied ones.
const MIGRATIONS: &[&str] = &[
    // 1: what kind of post each row is and when it was posted, and the
    // downloads table. TikTok rows already have both facts in raw_json.
    // YouTube's listing has no post date, so posted_at is filled in from the
    // download's metadata later.
    "ALTER TABLE videos ADD COLUMN kind TEXT NOT NULL DEFAULT 'video';  -- 'video' | 'photo' (TikTok slideshow)
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
     );",
];

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
    /// `video`, or `photo` for a TikTok slideshow.
    pub kind: &'a str,
    pub posted_at: Option<i64>,
}

/// A liked video that still needs its media downloaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDownload {
    pub platform: String,
    pub video_id: String,
    pub url: String,
    pub kind: String,
}

/// A finished download, as `record_download` stores it.
pub struct Downloaded<'a> {
    /// The video's folder, relative to the media root.
    pub dir: &'a str,
    pub files: &'a [String],
    pub bytes: u64,
    pub format: Option<&'a str>,
    pub info_json: Option<&'a str>,
    /// When the video was posted, if the download's metadata says.
    pub posted_at: Option<i64>,
}

/// A failed download is retried an hour later, then two hours, four, and so
/// on, but never more than this long after: a week, in seconds. It's never
/// given up on, because many failures aren't the video's fault (a yt-dlp
/// broken by a site change, an expired login, an outage) and can last days.
pub const MAX_RETRY_DELAY: i64 = 7 * 24 * 3600;

/// `~/.local/share/joyscroller/joyscroller.db`
pub fn default_path() -> Result<PathBuf> {
    Ok(dirs::data_dir()
        .context("no XDG data dir")?
        .join("joyscroller/joyscroller.db"))
}

pub fn open(path: &std::path::Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    // The syncs and the downloader each hold a connection. WAL lets one read
    // while the other writes, and the busy timeout makes a writer wait its
    // turn instead of failing.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(30))?;
    init(&mut conn)?;
    Ok(conn)
}

/// Create the tables if needed and bring them up to date.
fn init(conn: &mut Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    loop {
        // Take the write lock before reading the version, so two processes
        // opening the same old database can't both apply the same step.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let Some(step) = MIGRATIONS.get(usize::try_from(version)?) else {
            return Ok(());
        };
        tx.execute_batch(step)
            .with_context(|| format!("applying database migration {}", version + 1))?;
        tx.pragma_update(None, "user_version", version + 1)?;
        tx.commit()?;
    }
}

/// Unix seconds. A clock set before 1970 reads as 0.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed())
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
        params![now(), i64::try_from(added)?, sync_id],
    )?;
    Ok(())
}

/// Every video id already stored for `platform`.
pub fn video_ids(conn: &Connection, platform: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT video_id FROM videos WHERE platform = ?1")?;
    let ids = stmt
        .query_map([platform], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
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
             (platform, video_id, url, title, author_id, author_name, duration_seconds, available, raw_json, liked_at, kind, posted_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                v.duration_seconds.map(i64::try_from).transpose()?,
                v.available,
                v.raw_json,
                liked_at,
                v.kind,
                v.posted_at,
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

/// Up to `limit` videos to download next, most recently stored first, so new
/// likes don't wait behind the backlog. Skips videos that are unavailable,
/// already downloaded, or waiting out a retry delay.
pub fn pending_downloads(conn: &Connection, limit: usize) -> Result<Vec<PendingDownload>> {
    let mut stmt = conn.prepare(
        "SELECT v.platform, v.video_id, v.url, v.kind FROM videos v \
         LEFT JOIN downloads d ON d.platform = v.platform AND d.video_id = v.video_id \
         WHERE v.available = 1 AND d.downloaded_at IS NULL \
           AND (d.attempts IS NULL OR d.attempts = 0 \
                OR d.last_attempt_at + MIN(3600 << MIN(d.attempts - 1, 20), ?1) <= ?2) \
         ORDER BY v.rowid DESC LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            params![MAX_RETRY_DELAY, now(), i64::try_from(limit)?],
            |r| {
                Ok(PendingDownload {
                    platform: r.get(0)?,
                    video_id: r.get(1)?,
                    url: r.get(2)?,
                    kind: r.get(3)?,
                })
            },
        )?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// Count a failed download attempt and remember why it failed.
pub fn record_download_failure(
    conn: &Connection,
    platform: &str,
    video_id: &str,
    error: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO downloads (platform, video_id, attempts, last_attempt_at, last_error) \
         VALUES (?1, ?2, 1, ?3, ?4) \
         ON CONFLICT (platform, video_id) DO UPDATE SET \
           attempts = attempts + 1, last_attempt_at = excluded.last_attempt_at, \
           last_error = excluded.last_error",
        params![platform, video_id, now(), error],
    )?;
    Ok(())
}

/// Mark a video downloaded. Also fills in its `posted_at` if that wasn't
/// known yet; a value already there is never overwritten.
pub fn record_download(
    conn: &mut Connection,
    platform: &str,
    video_id: &str,
    done: &Downloaded,
) -> Result<()> {
    let at = now();
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO downloads \
           (platform, video_id, last_attempt_at, downloaded_at, dir, files, bytes, format, info_json) \
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT (platform, video_id) DO UPDATE SET \
           last_attempt_at = excluded.last_attempt_at, downloaded_at = excluded.downloaded_at, \
           last_error = NULL, dir = excluded.dir, files = excluded.files, bytes = excluded.bytes, \
           format = excluded.format, info_json = excluded.info_json",
        params![
            platform,
            video_id,
            at,
            done.dir,
            serde_json::to_string(done.files)?,
            i64::try_from(done.bytes)?,
            done.format,
            done.info_json,
        ],
    )?;
    tx.execute(
        "UPDATE videos SET posted_at = ?1 \
         WHERE platform = ?2 AND video_id = ?3 AND posted_at IS NULL",
        params![done.posted_at, platform, video_id],
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn video(id: &str, available: bool) -> NewVideo<'_> {
        NewVideo {
            video_id: id,
            url: "https://example.com",
            title: available.then_some("title"),
            author_id: None,
            author_name: None,
            duration_seconds: Some(60),
            available,
            raw_json: "{}",
            kind: "video",
            posted_at: None,
        }
    }

    fn memory_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        init(&mut conn).unwrap();
        conn
    }

    #[test]
    fn inserts_only_new_videos() {
        let mut conn = memory_db();
        assert_eq!(
            insert_new(&mut conn, "youtube", &[video("a", true)]).unwrap(),
            1
        );
        let added =
            insert_new(&mut conn, "youtube", &[video("a", true), video("b", true)]).unwrap();
        assert_eq!(added, 1);
        assert_eq!(
            video_ids(&conn, "youtube").unwrap(),
            HashSet::from(["a".into(), "b".into()])
        );
        assert!(video_ids(&conn, "tiktok").unwrap().is_empty());
    }

    #[test]
    fn refreshes_availability_but_keeps_title() {
        let mut conn = memory_db();
        insert_new(&mut conn, "youtube", &[video("a", true)]).unwrap();
        insert_new(&mut conn, "youtube", &[video("a", false)]).unwrap();
        let (title, available): (Option<String>, bool) = conn
            .query_row(
                "SELECT title, available FROM videos WHERE video_id = 'a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(title.as_deref(), Some("title"));
        assert!(!available);
    }

    #[test]
    fn records_sync_runs() {
        let conn = memory_db();
        let id = start_sync(&conn, "youtube").unwrap();
        finish_sync(&conn, id, 3).unwrap();
        let added: i64 = conn
            .query_row("SELECT added FROM syncs WHERE id = ?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(added, 3);
    }

    #[test]
    fn migrates_an_original_database() {
        // A database made before any migration, with one TikTok photo post.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO videos (platform, video_id, url, raw_json, liked_at) VALUES \
             ('tiktok', 'p', 'u', '{\"createTime\": 1700000000, \"imagePost\": {}}', 0), \
             ('youtube', 'y', 'u', '{}', 0)",
            [],
        )
        .unwrap();
        init(&mut conn).unwrap();
        // Running it again changes nothing.
        init(&mut conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, i64::try_from(MIGRATIONS.len()).unwrap());
        let rows: Vec<(String, Option<i64>)> = conn
            .prepare("SELECT kind, posted_at FROM videos ORDER BY platform")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                ("photo".into(), Some(1_700_000_000)),
                ("video".into(), None)
            ]
        );
    }

    fn pending_ids(conn: &Connection) -> Vec<String> {
        pending_downloads(conn, 10)
            .unwrap()
            .into_iter()
            .map(|p| p.video_id)
            .collect()
    }

    #[test]
    fn queues_newest_available_videos_first() {
        let mut conn = memory_db();
        insert_new(
            &mut conn,
            "youtube",
            &[video("old", true), video("gone", false), video("new", true)],
        )
        .unwrap();
        assert_eq!(pending_ids(&conn), ["new", "old"]);
    }

    #[test]
    fn failed_downloads_wait_before_retrying() {
        let mut conn = memory_db();
        insert_new(&mut conn, "youtube", &[video("a", true)]).unwrap();
        record_download_failure(&conn, "youtube", "a", "HTTP 429").unwrap();
        assert!(pending_ids(&conn).is_empty());

        // An hour later it's due again.
        conn.execute(
            "UPDATE downloads SET last_attempt_at = last_attempt_at - 3600",
            [],
        )
        .unwrap();
        assert_eq!(pending_ids(&conn), ["a"]);

        // However many times it has failed, it's due again within a week.
        conn.execute(
            "UPDATE downloads SET attempts = 500, last_attempt_at = ?1",
            [now() - MAX_RETRY_DELAY + 60],
        )
        .unwrap();
        assert!(pending_ids(&conn).is_empty());
        conn.execute(
            "UPDATE downloads SET last_attempt_at = last_attempt_at - 60",
            [],
        )
        .unwrap();
        assert_eq!(pending_ids(&conn), ["a"]);
    }

    #[test]
    fn downloaded_videos_leave_the_queue() {
        let mut conn = memory_db();
        insert_new(&mut conn, "youtube", &[video("a", true)]).unwrap();
        record_download_failure(&conn, "youtube", "a", "flaky").unwrap();
        let files = ["video.webm".to_string()];
        let done = |posted_at| Downloaded {
            dir: "youtube/a",
            files: &files,
            bytes: 10,
            format: Some("399+251"),
            info_json: None,
            posted_at,
        };
        record_download(&mut conn, "youtube", "a", &done(Some(5))).unwrap();
        assert!(pending_ids(&conn).is_empty());
        let (error, stored_files): (Option<String>, String) = conn
            .query_row("SELECT last_error, files FROM downloads", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(error, None);
        assert_eq!(stored_files, r#"["video.webm"]"#);

        // posted_at is filled in once, never overwritten.
        record_download(&mut conn, "youtube", "a", &done(Some(9))).unwrap();
        let posted_at: i64 = conn
            .query_row("SELECT posted_at FROM videos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(posted_at, 5);
    }
}
