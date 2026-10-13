//! The local archive: a SQLite database of every video the user has liked,
//! across platforms. A row's presence means "liked". Rows keep what the video
//! looked like when we first saw it; only `available` is refreshed on each
//! sync, so a video that goes private later still has its title on file.
//! `downloads` tracks which videos have their media saved to disk. Every query,
//! including the web UI's reads and its search (two FTS5 indexes over each
//! video's text, queried with what `query.rs` parses), lives here.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::query::{self, Atom, Index, Query};

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
    // 2: full-text search (see "Search" below) and indexes for the list's
    // column sorts. `video_text` is what's searched, one row per video under
    // the video's rowid: the title, the author (the stored name plus the
    // handle, display name or channel the platform also gives), and the
    // description, which only a download's metadata has (YouTube's listing
    // leaves it out; TikTok's and Instagram's repeat the caption, so a
    // description equal to the title is left out). Two FTS5 indexes read it
    // as external content: trigrams for substrings in any script, and
    // unicode61 words. The view says how a video's text is made, so the
    // backfill and the triggers can't disagree; a later step can replace it
    // and refill `video_text`. Video rows are insert-only and `available`
    // isn't searched, so only a download changes a video's text. The JSON is
    // always valid, but json_valid keeps one bad row from failing a sync.
    "CREATE VIEW video_text_source AS SELECT
         v.rowid AS id,
         v.title,
         concat_ws(' ', v.author_name, nullif(CASE WHEN json_valid(v.raw_json) THEN CASE v.platform
             WHEN 'youtube' THEN json_extract(v.raw_json, '$.uploader_id')
             WHEN 'tiktok' THEN json_extract(v.raw_json, '$.author.nickname')
             WHEN 'instagram' THEN json_extract(v.raw_json, '$.user.full_name')
           END END, v.author_name)) AS author,
         nullif(CASE WHEN json_valid(d.info_json) THEN json_extract(d.info_json, '$.description') END,
           v.title) AS description
       FROM videos v LEFT JOIN downloads d ON d.platform = v.platform AND d.video_id = v.video_id;
     CREATE TABLE video_text (
         id          INTEGER PRIMARY KEY,      -- the video's rowid
         title       TEXT,
         author      TEXT,
         description TEXT
     );
     CREATE VIRTUAL TABLE video_search USING fts5(
         title, author, description,
         content='video_text', content_rowid='id',
         tokenize='trigram remove_diacritics 2'
     );
     -- prefix='1 2' keeps a short word cheap to search while it's being typed.
     CREATE VIRTUAL TABLE video_words USING fts5(
         title, author, description,
         content='video_text', content_rowid='id',
         tokenize='unicode61 remove_diacritics 2',
         prefix='1 2'
     );
     INSERT INTO video_text (id, title, author, description)
       SELECT id, title, author, description FROM video_text_source;
     INSERT INTO video_search (video_search) VALUES ('rebuild');
     INSERT INTO video_words (video_words) VALUES ('rebuild');
     CREATE TRIGGER video_text_ai AFTER INSERT ON video_text BEGIN
       INSERT INTO video_search (rowid, title, author, description)
         VALUES (new.id, new.title, new.author, new.description);
       INSERT INTO video_words (rowid, title, author, description)
         VALUES (new.id, new.title, new.author, new.description);
     END;
     -- An external-content index forgets a row through its 'delete' command
     -- with the old values; inserting them would add the tokens again.
     CREATE TRIGGER video_text_ad AFTER DELETE ON video_text BEGIN
       INSERT INTO video_search (video_search, rowid, title, author, description)
         VALUES ('delete', old.id, old.title, old.author, old.description);
       INSERT INTO video_words (video_words, rowid, title, author, description)
         VALUES ('delete', old.id, old.title, old.author, old.description);
     END;
     CREATE TRIGGER video_text_au AFTER UPDATE ON video_text
     WHEN old.title IS NOT new.title OR old.author IS NOT new.author
       OR old.description IS NOT new.description
     BEGIN
       INSERT INTO video_search (video_search, rowid, title, author, description)
         VALUES ('delete', old.id, old.title, old.author, old.description);
       INSERT INTO video_words (video_words, rowid, title, author, description)
         VALUES ('delete', old.id, old.title, old.author, old.description);
       INSERT INTO video_search (rowid, title, author, description)
         VALUES (new.id, new.title, new.author, new.description);
       INSERT INTO video_words (rowid, title, author, description)
         VALUES (new.id, new.title, new.author, new.description);
     END;
     CREATE TRIGGER videos_text_ai AFTER INSERT ON videos BEGIN
       INSERT INTO video_text (id, title, author, description)
         SELECT id, title, author, description FROM video_text_source WHERE id = new.rowid;
     END;
     CREATE TRIGGER videos_text_ad AFTER DELETE ON videos BEGIN
       DELETE FROM video_text WHERE id = old.rowid;
     END;
     CREATE TRIGGER downloads_text_ai AFTER INSERT ON downloads BEGIN
       UPDATE video_text SET description =
           (SELECT description FROM video_text_source s WHERE s.id = video_text.id)
         WHERE id = (SELECT rowid FROM videos WHERE platform = new.platform AND video_id = new.video_id);
     END;
     CREATE TRIGGER downloads_text_au AFTER UPDATE OF info_json ON downloads BEGIN
       UPDATE video_text SET description =
           (SELECT description FROM video_text_source s WHERE s.id = video_text.id)
         WHERE id = (SELECT rowid FROM videos WHERE platform = new.platform AND video_id = new.video_id);
     END;
     -- The column sorts, two indexes each, matching `order_by` term for term:
     -- (x) read backwards is x DESC with empty values last, as SQLite sorts
     -- NULL lowest; (x IS NULL, x) read forwards is x ASC with them last.
     -- Every index ends in the rowid, which breaks ties the same way.
     CREATE INDEX videos_title_desc ON videos (title COLLATE NOCASE);
     CREATE INDEX videos_title_asc ON videos (title IS NULL, title COLLATE NOCASE);
     CREATE INDEX videos_author_desc ON videos (author_name COLLATE NOCASE);
     CREATE INDEX videos_author_asc ON videos (author_name IS NULL, author_name COLLATE NOCASE);
     CREATE INDEX videos_length_desc ON videos (duration_seconds);
     CREATE INDEX videos_length_asc ON videos (duration_seconds IS NULL, duration_seconds);
     CREATE INDEX videos_posted_desc ON videos (posted_at);
     CREATE INDEX videos_posted_asc ON videos (posted_at IS NULL, posted_at);",
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

/// A liked video that still needs its media downloaded, with what its
/// folder is named after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDownload {
    pub platform: String,
    pub video_id: String,
    pub url: String,
    pub kind: String,
    pub title: Option<String>,
    pub author_name: Option<String>,
    /// The local calendar date of `liked_at`, as `YYYY-MM-DD`.
    pub liked_on: String,
}

/// `PendingDownload`'s columns, for `pending_from_row`. SQLite turns
/// `liked_at` into a date in the system's time zone, the user's own.
const PENDING_COLUMNS: &str = "v.platform, v.video_id, v.url, v.kind, v.title, v.author_name, \
     date(v.liked_at, 'unixepoch', 'localtime')";

fn pending_from_row(r: &rusqlite::Row) -> rusqlite::Result<PendingDownload> {
    Ok(PendingDownload {
        platform: r.get(0)?,
        video_id: r.get(1)?,
        url: r.get(2)?,
        kind: r.get(3)?,
        title: r.get(4)?,
        author_name: r.get(5)?,
        liked_on: r.get(6)?,
    })
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
pub fn init(conn: &mut Connection) -> Result<()> {
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

/// Which videos are waiting to be downloaded: available, not downloaded, and
/// not waiting out a retry delay. `?1` is `MAX_RETRY_DELAY`, `?2` now, and
/// `?3` a platform or NULL for all.
const PENDING_WHERE: &str = "v.available = 1 AND d.downloaded_at IS NULL \
     AND (?3 IS NULL OR v.platform = ?3) \
     AND (d.attempts IS NULL OR d.attempts = 0 \
          OR d.last_attempt_at + MIN(3600 << MIN(d.attempts - 1, 20), ?1) <= ?2)";

/// Up to `limit` videos to download next, on `platform` or all of them, most
/// recently stored first, so new likes don't wait behind the backlog. Skips
/// videos that are unavailable, already downloaded, or waiting out a retry
/// delay.
pub fn pending_downloads(
    conn: &Connection,
    platform: Option<&str>,
    limit: usize,
) -> Result<Vec<PendingDownload>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PENDING_COLUMNS} FROM videos v \
         LEFT JOIN downloads d ON d.platform = v.platform AND d.video_id = v.video_id \
         WHERE {PENDING_WHERE} ORDER BY v.rowid DESC LIMIT ?4"
    ))?;
    let rows = stmt
        .query_map(
            params![MAX_RETRY_DELAY, now(), platform, i64::try_from(limit)?],
            pending_from_row,
        )?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

/// How many videos on `platform` `pending_downloads` would offer.
pub fn pending_count(conn: &Connection, platform: &str) -> Result<usize> {
    let count: i64 = conn.query_row(
        &format!(
            "SELECT count(*) FROM videos v \
             LEFT JOIN downloads d ON d.platform = v.platform AND d.video_id = v.video_id \
             WHERE {PENDING_WHERE}"
        ),
        params![MAX_RETRY_DELAY, now(), platform],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count)?)
}

/// One video, to download now whatever its state, as the UI's download
/// button asks.
pub fn download_item(
    conn: &Connection,
    platform: &str,
    video_id: &str,
) -> Result<Option<PendingDownload>> {
    let found = conn
        .query_row(
            &format!(
                "SELECT {PENDING_COLUMNS} FROM videos v WHERE v.platform = ?1 AND v.video_id = ?2"
            ),
            [platform, video_id],
            pending_from_row,
        )
        .optional()?;
    Ok(found)
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

/// A stored video, as the web UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVideo {
    /// Insertion order, which is like order. The UI sorts by it and streams
    /// rows past the last one a page has seen.
    pub rowid: i64,
    pub platform: String,
    pub video_id: String,
    pub url: String,
    pub title: Option<String>,
    pub author_id: Option<String>,
    pub author_name: Option<String>,
    pub duration_seconds: Option<i64>,
    pub available: bool,
    pub kind: String,
    pub posted_at: Option<i64>,
    pub liked_at: i64,
    /// Once its media is downloaded, the file in its folder to show as its
    /// picture: the thumbnail, or a photo post's first image.
    pub thumbnail: Option<String>,
}

/// `StoredVideo`'s columns, for `video_from_row`, selected from
/// `VIDEO_TABLES`. The file lists are sorted, so `MIN` is the first image.
const VIDEO_COLUMNS: &str = "v.rowid, v.platform, v.video_id, v.url, v.title, v.author_id, \
     v.author_name, v.duration_seconds, v.available, v.kind, v.posted_at, v.liked_at, \
     COALESCE(\
       (SELECT MIN(value) FROM json_each(d.files) WHERE value LIKE 'thumbnail.%'), \
       (SELECT MIN(value) FROM json_each(d.files) WHERE value LIKE 'image-%'))";

/// The videos, each with its download if that has finished.
const VIDEO_TABLES: &str = "videos v LEFT JOIN downloads d ON d.platform = v.platform \
     AND d.video_id = v.video_id AND d.downloaded_at IS NOT NULL";

fn video_from_row(r: &rusqlite::Row) -> rusqlite::Result<StoredVideo> {
    Ok(StoredVideo {
        rowid: r.get(0)?,
        platform: r.get(1)?,
        video_id: r.get(2)?,
        url: r.get(3)?,
        title: r.get(4)?,
        author_id: r.get(5)?,
        author_name: r.get(6)?,
        duration_seconds: r.get(7)?,
        available: r.get(8)?,
        kind: r.get(9)?,
        posted_at: r.get(10)?,
        liked_at: r.get(11)?,
        thumbnail: r.get(12)?,
    })
}

/// How many videos are stored per platform.
pub fn video_counts(conn: &Connection) -> Result<Vec<(String, usize)>> {
    let mut stmt = conn.prepare("SELECT platform, COUNT(*) FROM videos GROUP BY platform")?;
    let counts: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    counts
        .into_iter()
        .map(|(platform, count)| Ok((platform, usize::try_from(count)?)))
        .collect()
}

/// What the list can be ordered by. This is the whole list: no other text
/// ever reaches an `ORDER BY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// The search's best matches first. Without a search, newest first.
    Best,
    Title,
    Author,
    Length,
    Posted,
    /// Like order, which is rowid order.
    Added,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Order {
    pub sort: Sort,
    pub dir: Dir,
}

impl Order {
    /// The list's usual order: most recently liked first.
    pub const NEWEST: Order = Order {
        sort: Sort::Added,
        dir: Dir::Desc,
    };
}

/// The `ORDER BY` terms for `order`, over `videos v` joined to `matches m`
/// when `searching`. Rows without the value (no caption, a YouTube video
/// whose post date isn't known until it's downloaded) go last either way.
/// Every order ends in the rowid, so equal values keep one order and paging
/// neither skips nor repeats a row. Each column order is term for term one
/// of the indexes migration 2 made, so SQLite reads it from the index
/// instead of sorting the table.
fn order_by(order: Order, searching: bool) -> String {
    // Names compare without regard to case, so "alice" doesn't sort after
    // "Zed". The collation goes on the sort term only: `title IS NULL` must
    // read exactly as the index has it.
    let (column, collate) = match order.sort {
        Sort::Best if searching => return "m.score, v.rowid DESC".into(),
        Sort::Best => return "v.rowid DESC".into(),
        Sort::Added => {
            return match order.dir {
                Dir::Asc => "v.rowid ASC".into(),
                Dir::Desc => "v.rowid DESC".into(),
            };
        }
        Sort::Title => ("v.title", " COLLATE NOCASE"),
        Sort::Author => ("v.author_name", " COLLATE NOCASE"),
        Sort::Length => ("v.duration_seconds", ""),
        Sort::Posted => ("v.posted_at", ""),
    };
    // SQLite sorts NULL lowest, so descending already puts it last.
    match order.dir {
        Dir::Asc => format!("{column} IS NULL, {column}{collate} ASC, v.rowid ASC"),
        Dir::Desc => format!("{column}{collate} DESC, v.rowid DESC"),
    }
}

/// The values for `videos_page` and `videos_count`'s `?`s: the search's,
/// then the platform twice.
fn list_params(search: Vec<String>, platform: Option<&str>) -> Vec<Value> {
    let platform = platform.map_or(Value::Null, |name| Value::Text(name.into()));
    search
        .into_iter()
        .map(Value::Text)
        .chain([platform.clone(), platform])
        .collect()
}

/// One page of videos on one platform or all, those `search` matches (all
/// of them when it's empty), in `order`.
pub fn videos_page(
    conn: &Connection,
    platform: Option<&str>,
    search: &Query,
    order: Order,
    limit: usize,
    offset: usize,
) -> Result<Vec<StoredVideo>> {
    // bm25 is most of what a search costs, so it only runs to rank.
    let found = search_matches(search, order.sort == Sort::Best);
    let searching = found.is_some();
    let (with, join, search_params) = match found {
        Some((sql, params)) => (
            format!("WITH {sql}"),
            "JOIN matches m ON m.id = v.rowid",
            params,
        ),
        None => (String::new(), "", Vec::new()),
    };
    let mut params = list_params(search_params, platform);
    params.push(Value::Integer(i64::try_from(limit)?));
    params.push(Value::Integer(i64::try_from(offset)?));
    let mut stmt = conn.prepare(&format!(
        "{with} SELECT {VIDEO_COLUMNS} FROM {VIDEO_TABLES} {join} \
         WHERE ? IS NULL OR v.platform = ? ORDER BY {} LIMIT ? OFFSET ?",
        order_by(order, searching)
    ))?;
    let videos = stmt
        .query_map(params_from_iter(params), video_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(videos)
}

/// How many videos `videos_page` pages through.
pub fn videos_count(conn: &Connection, platform: Option<&str>, search: &Query) -> Result<usize> {
    let (with, join, search_params) = match search_matches(search, false) {
        Some((sql, params)) => (
            format!("WITH {sql}"),
            "JOIN matches m ON m.id = v.rowid",
            params,
        ),
        None => (String::new(), "", Vec::new()),
    };
    let count: i64 = conn.query_row(
        &format!("{with} SELECT count(*) FROM videos v {join} WHERE ? IS NULL OR v.platform = ?"),
        params_from_iter(list_params(search_params, platform)),
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count)?)
}

// ---- Search ---------------------------------------------------------------
//
// A query (see `query.rs`) becomes common table expressions ending in
// `matches(id, score)`: the rowids of the matching videos, and a score where
// lower is better. Each token goes to the index that can answer it:
//
// * `video_search` uses the trigram tokenizer, so a token of three characters
//   or more is a substring match in any script. That's what makes a fragment
//   of a Japanese caption, which has no spaces to split on, findable.
// * `video_words` uses unicode61 and holds whole words. A trigram index can't
//   answer a token shorter than three characters, so "4" in "toy story 4"
//   goes here.
// * A short token in a script written without spaces (CJK, Thai) is neither a
//   word nor a trigram, so it's a plain substring test on the title.
//
// The operators are matched in two steps. The driver searches the words the
// query needs outright, including the longer words of quoted text and of a
// pattern: that finds a superset of the answer and ranks it with bm25. Each
// operator then narrows it with a condition of its own, chosen so that what
// it means survives how captions are written:
//
// * Quoted text is those exact characters, punctuation and spaces included,
//   which the trigram index answers as a substring, ignoring case and accents.
// * An unquoted exclusion is a whole word, or for `-spider-man` the words it
//   spells side by side across any separator, in `video_words`. It's never a
//   substring.
// * A pattern that ends a word with a star is a prefix query on
//   `video_words`. Any other is a GLOB on the lowercased text, which, unlike
//   the indexes, doesn't fold accents or non-ASCII case.
//
// Every token is quoted into MATCH, so FTS5's own query syntax (`NOT`,
// `col:`, parentheses) is just text. Nothing the user types reaches the SQL
// itself: it's all parameters.

/// SQL and the values for its `?`s, in order.
type Sql = (String, Vec<String>);

/// bm25's weights for the title, author and description: a word in the title
/// beats the same word in a channel name, which beats it buried in a
/// description.
const RANK: &str = "10.0, 5.0, 1.0";

/// `video_text`'s columns, which both indexes have too.
const TEXT_COLUMNS: [&str; 3] = ["title", "author", "description"];

/// The CTEs ending in `matches(id, score)`, or None when there's nothing to
/// search for. Without `rank` every score is 0.0 and bm25 never runs, for a
/// count or an order by a column.
fn search_matches(query: &Query, rank: bool) -> Option<Sql> {
    if query.is_empty() {
        return None;
    }
    let tokens = driver_tokens(query);
    let strict = strict(query);
    let driver = if tokens.is_empty() {
        alternatives(query)
    } else {
        Some(text_matches(&tokens, query.typing, rank))
    };
    Some(match driver {
        Some(driver) => narrowed(driver, strict),
        None => every_video(strict),
    })
}

/// Every video is a candidate, so the operators are tested as `video_text`
/// is read, rather than on a copy of all its ids joined back to it.
fn every_video(strict: Option<Sql>) -> Sql {
    let (condition, params) = strict.unwrap_or_else(|| ("TRUE".into(), Vec::new()));
    (
        format!(
            "matches(id, score) AS (SELECT t.id, 0.0 AS score FROM video_text t WHERE {condition})"
        ),
        params,
    )
}

fn narrowed((driver, mut params): Sql, strict: Option<Sql>) -> Sql {
    // The operators read the text only for a short substring or a pattern.
    // Every other one tests the id against an index, so the candidates, which
    // hold it, stand in for `video_text t`.
    let (from, u, condition) = match strict {
        None => ("candidates AS u", "u", String::new()),
        Some((sql, strict_params)) => {
            params.extend(strict_params);
            if TEXT_COLUMNS
                .iter()
                .any(|column| sql.contains(&format!("t.{column}")))
            {
                (
                    "candidates AS u JOIN video_text t ON t.id = u.id",
                    "u",
                    format!("WHERE {sql}"),
                )
            } else {
                ("candidates AS t", "t", format!("WHERE {sql}"))
            }
        }
    };
    // Materialized before the operators join it: flattened into that join,
    // bm25 loses the FTS5 query it scores and refuses to run.
    (
        format!(
            "candidates(id, score) AS MATERIALIZED ({driver}), \
             matches(id, score) AS (SELECT {u}.id, {u}.score FROM {from} {condition})"
        ),
        params,
    )
}

/// The tokens the query needs outright: every plain word, and the trigrams of
/// quoted text and patterns. A trigram is a substring, which takes in any
/// quoted text or pattern it's part of; a whole word wouldn't.
fn driver_tokens(query: &Query) -> Vec<(Index, &str)> {
    query
        .clauses
        .iter()
        .filter_map(|clause| match clause.as_slice() {
            [atom] => Some(atom_driver_tokens(atom)),
            _ => None,
        })
        .flatten()
        .collect()
}

fn atom_driver_tokens(atom: &Atom) -> Vec<(Index, &str)> {
    match atom {
        Atom::Words(text) => query::tokens(text),
        Atom::Phrase(text) | Atom::Wildcard(text) => query::tokens(text)
            .into_iter()
            .filter(|(index, _)| *index == Index::Trigram)
            .collect(),
    }
}

/// With no word it needs outright, a query starts from what any alternative
/// of one clause finds, when each has a token an index can find: everything
/// the clause allows is in there, and the operators then hold it to the rest.
/// A clause with an alternative only a scan can answer, such as `x*` or a
/// short unspaced word, and a query of nothing but exclusions, start from
/// every video (None).
fn alternatives(query: &Query) -> Option<Sql> {
    query.clauses.iter().find_map(|clause| {
        let atom_tokens: Vec<_> = clause.iter().map(atom_driver_tokens).collect();
        let indexed = atom_tokens
            .iter()
            .all(|tokens| tokens.iter().any(|(index, _)| *index != Index::Substring));
        indexed.then(|| {
            let parts: Vec<Sql> = atom_tokens
                .iter()
                .map(|tokens| text_matches(tokens, false, false))
                .collect();
            let sql = parts
                .iter()
                .map(|(sql, _)| sql.as_str())
                .collect::<Vec<_>>()
                .join(" UNION ");
            let params = parts.into_iter().flat_map(|(_, params)| params).collect();
            (format!("SELECT id, score FROM ({sql})"), params)
        })
    })
}

/// The videos with every token. One index drives the query and scores it;
/// every other kind of token narrows it. The trigram index drives when it
/// can, because its bm25 sees the longest and most specific tokens.
fn text_matches(tokens: &[(Index, &str)], typing: bool, rank: bool) -> Sql {
    let of = |wanted| -> Vec<&str> {
        tokens
            .iter()
            .filter(|(index, _)| *index == wanted)
            .map(|(_, token)| *token)
            .collect()
    };
    let (trigrams, words) = (of(Index::Trigram), of(Index::Word));
    // Lowercased to match `lower(title)`. SQLite's lower() folds only ASCII,
    // which is all a short token in an unspaced script can mix in ("a版").
    let mut substrings: Vec<String> = of(Index::Substring)
        .into_iter()
        .map(str::to_lowercase)
        .collect();
    // The token still being typed is a prefix, which only a word query
    // needs: a trigram phrase is already a substring match.
    let prefix = typing && matches!(tokens.last(), Some((Index::Word, _)));

    let (mut sql, mut params, words) = if !trigrams.is_empty() {
        (
            fts("video_search", rank),
            vec![phrases(&trigrams, false)],
            words,
        )
    } else if !words.is_empty() {
        (
            fts("video_words", rank),
            vec![phrases(&words, prefix)],
            Vec::new(),
        )
    } else {
        let first = substrings.remove(0);
        (
            format!(
                "SELECT id, 0.0 AS score FROM video_text WHERE instr({}, ?) > 0",
                folded("title", &first)
            ),
            vec![first],
            Vec::new(),
        )
    };
    // `+rowid`, not `rowid`: given a plain `rowid IN (…)`, SQLite hands each
    // id in the list to FTS5, which then runs its whole MATCH once per id.
    // The unary plus keeps the list out of FTS5, so its MATCH runs once.
    if !words.is_empty() {
        sql.push_str(" AND +rowid IN (SELECT rowid FROM video_words WHERE video_words MATCH ?)");
        params.push(phrases(&words, prefix));
    }
    let narrowing = substrings
        .iter()
        .map(|substring| {
            format!(
                " AND +rowid IN (SELECT id FROM video_text WHERE instr({}, ?) > 0)",
                folded("title", substring)
            )
        })
        .collect::<Vec<_>>()
        .concat();
    params.extend(substrings);
    (sql + &narrowing, params)
}

/// bm25's weights must be literals. The table is named in full because MATCH
/// doesn't accept an alias.
fn fts(table: &str, rank: bool) -> String {
    let score = if rank {
        format!("bm25({table}, {RANK})")
    } else {
        "0.0".into()
    };
    format!("SELECT rowid AS id, {score} AS score FROM {table} WHERE {table} MATCH ?")
}

fn phrases(tokens: &[&str], prefix: bool) -> String {
    let mut quoted: Vec<String> = tokens.iter().map(|token| quote_fts(token)).collect();
    if prefix && let Some(last) = quoted.last_mut() {
        last.push('*');
    }
    quoted.join(" ")
}

/// A string FTS5 reads as text, never as its query syntax.
fn quote_fts(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

/// Everything the driver can't say: quoted text, patterns, alternatives and
/// exclusions, as one condition on `video_text t`, or None when there's
/// nothing to hold the candidates to. Plain words need nothing here: the
/// driver already answered for them.
fn strict(query: &Query) -> Option<Sql> {
    let required = query
        .clauses
        .iter()
        .filter(|clause| !matches!(clause.as_slice(), [Atom::Words(_)]))
        .map(|clause| any(clause.iter().map(atom_condition).collect()));
    let excluded = query
        .excludes
        .iter()
        .map(|atom| negate(excluded_condition(atom)));
    let conditions: Vec<Sql> = required.chain(excluded).collect();
    (!conditions.is_empty()).then(|| all(conditions))
}

fn atom_condition(atom: &Atom) -> Sql {
    match atom {
        // Plain words as an alternative mean what they mean in the driver.
        Atom::Words(text) => all(query::tokens(text)
            .into_iter()
            .map(|(index, token)| match index {
                Index::Trigram => in_index("video_search", token),
                Index::Word => in_index("video_words", token),
                Index::Substring => contains(token),
            })
            .collect()),
        Atom::Phrase(text) => literal_condition(text),
        Atom::Wildcard(text) => wildcard_condition(text),
    }
}

/// An excluded word is a whole word, not a substring, or "-cat" would take
/// out every "catalog". One with punctuation in it, such as "-spider-man", is
/// the phrase it spells.
fn excluded_condition(atom: &Atom) -> Sql {
    match atom {
        Atom::Words(text) => phrase_condition(text),
        atom => atom_condition(atom),
    }
}

/// Quoted text is exactly those characters, punctuation and spaces and all.
/// The trigram index holds every character, so it answers that as a
/// substring, ignoring case and accents. Text too short for a trigram is
/// compared lowercased, which folds only ASCII.
fn literal_condition(text: &str) -> Sql {
    if text.chars().count() >= 3 {
        in_fts("video_search", quote_fts(text))
    } else {
        contains(text)
    }
}

/// The word index splits text the way `query::tokens` does, so a phrase
/// there is its words in order, across any separator. A script written
/// without spaces is one long word to that index, and its words are matched
/// as substrings instead, which loses their order.
fn phrase_condition(text: &str) -> Sql {
    let words: Vec<&str> = query::tokens(text).into_iter().map(|(_, w)| w).collect();
    if words.iter().any(|word| query::unspaced(word)) {
        all(words
            .into_iter()
            .map(|word| {
                if query::unspaced(word) {
                    substring_condition(word)
                } else {
                    in_index("video_words", word)
                }
            })
            .collect())
    } else {
        in_fts("video_words", quote_fts(&words.join(" ")))
    }
}

/// A pattern of one word that a star ends is the start of a word, which the
/// word index answers. Any other is a GLOB on the lowercased text, with
/// punctuation and stars alike matching any run of characters. Its pieces are
/// only letters, digits and marks, so nothing in it is GLOB syntax.
fn wildcard_condition(text: &str) -> Sql {
    let anchored = text
        .split('*')
        .next()
        .is_some_and(|head| !query::tokens(head).is_empty());
    let words: Vec<&str> = query::tokens(text).into_iter().map(|(_, w)| w).collect();
    let starts_word = anchored && words.first().is_some_and(|word| !query::unspaced(word));
    match words.as_slice() {
        [word] if starts_word => in_fts("video_words", format!("{}*", quote_fts(word))),
        [word] => substring_condition(word),
        _ => {
            let pieces: Vec<String> = words.iter().map(|word| word.to_lowercase()).collect();
            let pattern = format!("*{}*", pieces.join("*"));
            let glob = text_condition(|column| format!("{column} GLOB ?"), &pattern);
            match words.first() {
                Some(first) if starts_word => all(vec![
                    in_fts("video_words", format!("{}*", quote_fts(first))),
                    glob,
                ]),
                _ => glob,
            }
        }
    }
}

fn substring_condition(word: &str) -> Sql {
    if word.chars().count() >= 3 {
        in_index("video_search", word)
    } else {
        contains(word)
    }
}

fn in_index(table: &str, token: &str) -> Sql {
    in_fts(table, quote_fts(token))
}

fn in_fts(table: &str, expression: String) -> Sql {
    (
        format!("t.id IN (SELECT rowid FROM {table} WHERE {table} MATCH ?)"),
        vec![expression],
    )
}

fn contains(token: &str) -> Sql {
    text_condition(
        |column| format!("instr({column}, ?) > 0"),
        &token.to_lowercase(),
    )
}

/// SQLite's `lower()` folds only ASCII letters, so for text without one it
/// changes nothing the text could match, and is skipped.
fn folded(column: &str, text: &str) -> String {
    if text.chars().any(|c| c.is_ascii_alphabetic()) {
        format!("lower({column})")
    } else {
        column.into()
    }
}

/// `condition` over each text column of `video_text t`, any of which may
/// match. It turns a column into SQL with one `?`, which is `param`. A NULL
/// column reads as empty: NULL would make `NOT` unknown and drop the row from
/// an exclusion's results.
fn text_condition(condition: impl Fn(&str) -> String, param: &str) -> Sql {
    any(TEXT_COLUMNS
        .iter()
        .map(|column| {
            let column = format!("coalesce(t.{column}, '')");
            (condition(&folded(&column, param)), vec![param.into()])
        })
        .collect())
}

fn negate((sql, params): Sql) -> Sql {
    (format!("NOT {sql}"), params)
}

fn all(conditions: Vec<Sql>) -> Sql {
    join(conditions, " AND ")
}

fn any(conditions: Vec<Sql>) -> Sql {
    join(conditions, " OR ")
}

fn join(mut conditions: Vec<Sql>, operator: &str) -> Sql {
    if conditions.len() == 1 {
        return conditions.remove(0);
    }
    let sql = conditions
        .iter()
        .map(|(sql, _)| sql.as_str())
        .collect::<Vec<_>>()
        .join(operator);
    let params = conditions
        .into_iter()
        .flat_map(|(_, params)| params)
        .collect();
    (format!("({sql})"), params)
}

/// Every video stored after `rowid`, oldest first.
pub fn videos_after(conn: &Connection, rowid: i64) -> Result<Vec<StoredVideo>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {VIDEO_COLUMNS} FROM {VIDEO_TABLES} WHERE v.rowid > ?1 ORDER BY v.rowid"
    ))?;
    let videos = stmt
        .query_map([rowid], video_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(videos)
}

/// The rowid of the newest stored video, or 0 when there are none.
pub fn last_rowid(conn: &Connection) -> Result<i64> {
    Ok(
        conn.query_row("SELECT COALESCE(MAX(rowid), 0) FROM videos", [], |r| {
            r.get(0)
        })?,
    )
}

/// One video and its `raw_json`.
pub fn video(
    conn: &Connection,
    platform: &str,
    video_id: &str,
) -> Result<Option<(StoredVideo, String)>> {
    let found = conn
        .query_row(
            &format!(
                "SELECT {VIDEO_COLUMNS}, v.raw_json FROM {VIDEO_TABLES} \
                 WHERE v.platform = ?1 AND v.video_id = ?2"
            ),
            [platform, video_id],
            |r| Ok((video_from_row(r)?, r.get(13)?)),
        )
        .optional()?;
    Ok(found)
}

/// Where a video's download stands, if it has been tried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadState {
    pub attempts: i64,
    pub last_error: Option<String>,
    pub downloaded_at: Option<i64>,
    pub dir: Option<String>,
    /// The file names in `dir`, sorted; empty until downloaded.
    pub files: Vec<String>,
    pub bytes: Option<i64>,
}

pub fn download_state(
    conn: &Connection,
    platform: &str,
    video_id: &str,
) -> Result<Option<DownloadState>> {
    let found = conn
        .query_row(
            "SELECT attempts, last_error, downloaded_at, dir, bytes, files FROM downloads \
             WHERE platform = ?1 AND video_id = ?2",
            [platform, video_id],
            |r| {
                Ok((
                    DownloadState {
                        attempts: r.get(0)?,
                        last_error: r.get(1)?,
                        downloaded_at: r.get(2)?,
                        dir: r.get(3)?,
                        files: Vec::new(),
                        bytes: r.get(4)?,
                    },
                    r.get::<_, Option<String>>(5)?,
                ))
            },
        )
        .optional()?;
    let Some((mut state, files)) = found else {
        return Ok(None);
    };
    if let Some(files) = files {
        state.files = serde_json::from_str(&files)
            .with_context(|| format!("reading the file list of {platform} video {video_id}"))?;
    }
    Ok(Some(state))
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
        pending_downloads(conn, None, 10)
            .unwrap()
            .into_iter()
            .map(|p| p.video_id)
            .collect()
    }

    #[test]
    fn download_item_has_what_the_folder_is_named_after() {
        let mut conn = memory_db();
        insert_new(&mut conn, "youtube", &[video("a", true)]).unwrap();
        let item = download_item(&conn, "youtube", "a").unwrap().unwrap();
        assert_eq!(item.title.as_deref(), Some("title"));
        assert_eq!(item.liked_on.len(), "2026-10-12".len());
        assert!(
            item.liked_on
                .chars()
                .all(|c| c.is_ascii_digit() || c == '-')
        );
        assert_eq!(pending_downloads(&conn, None, 1).unwrap(), [item]);
        assert!(download_item(&conn, "youtube", "b").unwrap().is_none());
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
    fn queues_per_platform() {
        let mut conn = memory_db();
        insert_new(&mut conn, "youtube", &[video("y", true)]).unwrap();
        insert_new(&mut conn, "tiktok", &[video("t1", true), video("t2", true)]).unwrap();
        let ids = |platform| -> Vec<String> {
            pending_downloads(&conn, platform, 10)
                .unwrap()
                .into_iter()
                .map(|p| p.video_id)
                .collect()
        };
        assert_eq!(ids(Some("youtube")), ["y"]);
        assert_eq!(ids(Some("tiktok")), ["t2", "t1"]);
        assert_eq!(ids(None), ["t2", "t1", "y"]);
        assert_eq!(pending_count(&conn, "tiktok").unwrap(), 2);
        record_download_failure(&conn, "tiktok", "t2", "flaky").unwrap();
        assert_eq!(pending_count(&conn, "tiktok").unwrap(), 1);
        assert_eq!(pending_count(&conn, "instagram").unwrap(), 0);
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
        let state = download_state(&conn, "youtube", "a").unwrap().unwrap();
        assert_eq!(state.files, ["video.webm"]);
        assert_eq!(state.dir.as_deref(), Some("youtube/a"));

        // posted_at is filled in once, never overwritten.
        record_download(&mut conn, "youtube", "a", &done(Some(9))).unwrap();
        let posted_at: i64 = conn
            .query_row("SELECT posted_at FROM videos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(posted_at, 5);
    }

    #[test]
    fn pages_newest_first_per_platform() {
        let mut conn = memory_db();
        insert_new(
            &mut conn,
            "youtube",
            &[video("y1", true), video("y2", true)],
        )
        .unwrap();
        insert_new(&mut conn, "tiktok", &[video("t1", true)]).unwrap();
        let ids = |platform, limit, offset| -> Vec<String> {
            videos_page(
                &conn,
                platform,
                &Query::default(),
                Order::NEWEST,
                limit,
                offset,
            )
            .unwrap()
            .into_iter()
            .map(|v| v.video_id)
            .collect()
        };
        assert_eq!(ids(None, 10, 0), ["t1", "y2", "y1"]);
        assert_eq!(ids(None, 1, 1), ["y2"]);
        assert_eq!(ids(Some("youtube"), 10, 0), ["y2", "y1"]);
        assert!(ids(Some("instagram"), 10, 0).is_empty());

        let mut counts = video_counts(&conn).unwrap();
        counts.sort();
        assert_eq!(counts, [("tiktok".into(), 1), ("youtube".into(), 2)]);
    }

    #[test]
    fn streams_videos_stored_after_a_rowid() {
        let mut conn = memory_db();
        assert_eq!(last_rowid(&conn).unwrap(), 0);
        insert_new(&mut conn, "youtube", &[video("a", true)]).unwrap();
        let seen = last_rowid(&conn).unwrap();
        insert_new(&mut conn, "youtube", &[video("b", true), video("c", true)]).unwrap();
        let after: Vec<String> = videos_after(&conn, seen)
            .unwrap()
            .into_iter()
            .map(|v| v.video_id)
            .collect();
        assert_eq!(after, ["b", "c"]);
    }

    #[test]
    fn looks_up_one_video_and_its_download() {
        let mut conn = memory_db();
        insert_new(&mut conn, "youtube", &[video("a", true)]).unwrap();
        let (found, raw) = super::video(&conn, "youtube", "a").unwrap().unwrap();
        assert_eq!(found.title.as_deref(), Some("title"));
        assert_eq!(raw, "{}");
        assert!(super::video(&conn, "tiktok", "a").unwrap().is_none());

        assert!(download_state(&conn, "youtube", "a").unwrap().is_none());
        record_download_failure(&conn, "youtube", "a", "HTTP 429").unwrap();
        let state = download_state(&conn, "youtube", "a").unwrap().unwrap();
        assert_eq!(state.attempts, 1);
        assert_eq!(state.last_error.as_deref(), Some("HTTP 429"));
        assert_eq!(state.downloaded_at, None);
        assert!(state.files.is_empty());
    }

    #[test]
    fn videos_carry_their_downloaded_picture() {
        let mut conn = memory_db();
        insert_new(
            &mut conn,
            "tiktok",
            &[video("v", true), video("p", true), video("f", true)],
        )
        .unwrap();
        let save = |conn: &mut Connection, id, files: &[String]| {
            let done = Downloaded {
                dir: "tiktok/x",
                files,
                bytes: 10,
                format: None,
                info_json: None,
                posted_at: None,
            };
            record_download(conn, "tiktok", id, &done).unwrap();
        };
        let names = |names: &[&str]| -> Vec<String> { names.iter().map(|&n| n.into()).collect() };
        save(&mut conn, "v", &names(&["thumbnail.jpg", "video.mp4"]));
        save(
            &mut conn,
            "p",
            &names(&["image-01.jpg", "image-02.jpg", "sound.mp3"]),
        );
        // A failed try has no files, so no picture.
        record_download_failure(&conn, "tiktok", "f", "HTTP 403").unwrap();

        let pictures: Vec<(String, Option<String>)> =
            videos_page(&conn, None, &Query::default(), Order::NEWEST, 10, 0)
                .unwrap()
                .into_iter()
                .map(|v| (v.video_id, v.thumbnail))
                .collect();
        assert_eq!(
            pictures,
            [
                ("f".into(), None),
                ("p".into(), Some("image-01.jpg".into())),
                ("v".into(), Some("thumbnail.jpg".into())),
            ]
        );
        let (found, _) = super::video(&conn, "tiktok", "v").unwrap().unwrap();
        assert_eq!(found.thumbnail.as_deref(), Some("thumbnail.jpg"));
    }

    // ---- Search ----

    /// Titles as they really come, borrowed from magnetissimo's search tests:
    /// dotted names, a run of spaces and a dash in front, CJK with no spaces,
    /// an accent, quotes and a bar inside a title.
    const CORPUS: &[(&str, &str)] = &[
        (
            "dune_uhd",
            "Dune Part Two 2024 2160p UHD BluRay x265-MeGusta",
        ),
        (
            "dune_web",
            "Dune.Part.Two.2024.1080p.WEB-DL.DDP5.1.H.264-FLUX",
        ),
        ("dune_hdcam", "Dune 2021 HDCAM x264-GAP"),
        ("dune_cam", "Dune Part Two 2024 CAM 720p x264"),
        (
            "prophecy3",
            "Dune Prophecy S01E03 Sisterhood Above All 1080p MAX WEB-DL DDP5 1 Atmos H 264-FLUX",
        ),
        ("prophecy4", "Dune Prophecy S01E04 Twice Born 2160p WEB-DL"),
        ("dunes", "Dunes of Arrakis Soundtrack FLAC"),
        (
            "seinfeld_s05e11",
            "-    Seinfeld S05E11 The Conversion 1080p HEVC x265-MeGusta",
        ),
        (
            "seinfeld_s05e16",
            "-    Seinfeld S05E16 The Stand-In 1080p PLAY WEB-DL AAC2 0 H 264-FLUX",
        ),
        (
            "seinfeld_s03e14",
            "Seinfeld s03e14 the pez dispenser dvdrip x264 digg mkv",
        ),
        ("seinfeld_disc", "Seinfeld season 5 disc 4 dvdr"),
        ("bb_s05e13", "Breaking-Bad-S05E13-HDTVx264EVOLVE[ettv]"),
        ("bb_s05e14", "Breaking Bad S05E14 Ozymandias 720p"),
        (
            "el_camino",
            "El Camino - A Breaking Bad Movie 2019 (720p - WEB-DL) [DUBLADO]",
        ),
        (
            "cabin_woods",
            "The.Cabin.In.The.Woods.2011.DVDRip.XviD-Ac3.Feel-Free",
        ),
        ("cabin", "The Cabin (2018)"),
        ("shingeki", "進撃の巨人 The Final Season 2160p"),
        (
            "rilakkuma",
            "[ANi] 拉拉熊 - 27 [1080P][Baha][WEB-DL][AAC AVC][CHT] mp4",
        ),
        ("pikachu", "Pokémon Detective Pikachu 2019 1080p"),
        ("toy4", "Toy.Story.4.2019.1080p"),
        ("toy3", "Toy Story 3 (2010)"),
        ("f9", "F9 2021 720p HDCAM x265-GAP"),
        ("camera", "Camera Obscura 2017 1080p WEB"),
        (
            "weird_al",
            r#""Weird Al" Yankovic - Mandatory Fun (2014) FLAC"#,
        ),
        ("cyberpunk", "Cyberpunk | Edgerunners S01 1080p"),
        ("not_okay", "Not Okay (2022)"),
        ("up", "Up (2009) 4K HDR"),
    ];

    const DUNE: &[&str] = &[
        "dune_uhd",
        "dune_web",
        "dune_hdcam",
        "dune_cam",
        "prophecy3",
        "prophecy4",
        "dunes",
    ];
    const SEINFELD: &[&str] = &[
        "seinfeld_s05e11",
        "seinfeld_s05e16",
        "seinfeld_s03e14",
        "seinfeld_disc",
    ];

    /// The corpus, plus one video by a TikTok creator whose display name
    /// differs from their handle. Only "cabin" has a description, from its
    /// download, so a match on "couple" is in the description.
    fn search_db() -> Connection {
        let mut conn = memory_db();
        let mut videos: Vec<NewVideo> = CORPUS
            .iter()
            .map(|&(id, title)| NewVideo {
                title: Some(title),
                ..video(id, true)
            })
            .collect();
        videos.push(NewVideo {
            title: None,
            author_name: Some("zicktoonz"),
            raw_json: r#"{"author": {"nickname": "Zick Toonz"}}"#,
            ..video("creator", true)
        });
        insert_new(&mut conn, "tiktok", &videos).unwrap();
        let info = r#"{"description": "A couple escape to a cabin in the woods."}"#;
        record_download(
            &mut conn,
            "tiktok",
            "cabin",
            &Downloaded {
                dir: "tiktok/cabin",
                files: &[],
                bytes: 0,
                format: None,
                info_json: Some(info),
                posted_at: None,
            },
        )
        .unwrap();
        conn
    }

    fn ids(videos: Vec<StoredVideo>) -> Vec<String> {
        videos.into_iter().map(|v| v.video_id).collect()
    }

    /// The ids `text` finds, sorted. Every search is also counted, read two
    /// at a time and read in another order, so the count, the paging and the
    /// list always agree.
    fn found(conn: &Connection, text: &str) -> Vec<String> {
        let query = query::parse(text);
        let best = Order {
            sort: Sort::Best,
            dir: Dir::Asc,
        };
        let all = ids(videos_page(conn, None, &query, best, 100, 0).unwrap());
        assert_eq!(
            videos_count(conn, None, &query).unwrap(),
            all.len(),
            "count disagrees with the list for {text:?}"
        );
        let mut paged = Vec::new();
        for offset in (0..).step_by(2) {
            let page = ids(videos_page(conn, None, &query, best, 2, offset).unwrap());
            if page.is_empty() {
                break;
            }
            paged.extend(page);
        }
        assert_eq!(paged, all, "paging disagrees with the list for {text:?}");
        let mut sorted = all;
        sorted.sort();
        let mut by_title = ids(videos_page(
            conn,
            None,
            &query,
            Order {
                sort: Sort::Title,
                dir: Dir::Asc,
            },
            100,
            0,
        )
        .unwrap());
        by_title.sort();
        assert_eq!(by_title, sorted, "sorting changed what {text:?} finds");
        sorted
    }

    fn sorted(ids: &[&str]) -> Vec<String> {
        let mut ids: Vec<String> = ids.iter().map(|&id| id.into()).collect();
        ids.sort();
        ids
    }

    fn all_but(except: &[&str]) -> Vec<String> {
        let mut ids: Vec<&str> = CORPUS.iter().map(|(id, _)| *id).collect();
        ids.push("creator");
        ids.retain(|id| !except.contains(id));
        sorted(&ids)
    }

    fn without(ids: &[&str], except: &[&str]) -> Vec<String> {
        sorted(
            &ids.iter()
                .copied()
                .filter(|id| !except.contains(id))
                .collect::<Vec<_>>(),
        )
    }

    /// Asserts each case's query finds what it expects in `search_db`.
    fn assert_finds(cases: &[(&str, &str, Vec<String>)]) {
        let conn = search_db();
        for (label, text, expected) in cases {
            assert_eq!(&found(&conn, text), expected, "{label}: {text:?}");
        }
    }

    #[test]
    fn search_quotes() {
        assert_finds(&[
            // Quotes match exactly what's between them, punctuation and
            // spaces and all, ignoring only case and accents.
            ("quotes keep brackets", r#""[ani]""#, sorted(&["rilakkuma"])),
            ("quotes keep a dot", r#""h.264""#, sorted(&["dune_web"])),
            (
                "quotes keep a space",
                r#""h 264""#,
                sorted(&["prophecy3", "seinfeld_s05e16"]),
            ),
            (
                "quotes keep a hyphen",
                r#""web-dl""#,
                sorted(&[
                    "dune_web",
                    "prophecy3",
                    "prophecy4",
                    "seinfeld_s05e16",
                    "el_camino",
                    "rilakkuma",
                ]),
            ),
            ("a hyphen is not a dot", r#""web.dl""#, sorted(&[])),
            (
                "a space is not a dot",
                r#""part two""#,
                sorted(&["dune_uhd", "dune_cam"]),
            ),
            (
                "a dot is not a space",
                r#""part.two""#,
                sorted(&["dune_web"]),
            ),
            ("quoted text keeps its order", r#""two part""#, sorted(&[])),
            (
                "quoted text keeps its words together",
                r#""dune two""#,
                sorted(&[]),
            ),
            (
                "quoted text matches inside a word",
                r#""s05e1""#,
                sorted(&[
                    "seinfeld_s05e11",
                    "seinfeld_s05e16",
                    "bb_s05e13",
                    "bb_s05e14",
                ]),
            ),
            (
                "a quoted word is a substring too",
                r#""dune""#,
                sorted(DUNE),
            ),
            (
                "quoted text across a dash",
                r#""x265-megusta""#,
                sorted(&["dune_uhd", "seinfeld_s05e11"]),
            ),
            (
                "quoted text with parentheses",
                r#""(2018)""#,
                sorted(&["cabin"]),
            ),
        ]);
    }

    #[test]
    fn search_quotes_in_any_column_and_script() {
        assert_finds(&[
            (
                "quoted text in a description",
                r#""cabin in the woods""#,
                sorted(&["cabin"]),
            ),
            (
                "quotes in a title are characters",
                r#""weird al""#,
                sorted(&["weird_al"]),
            ),
            (
                "quoted text ignores case and accents",
                r#""POKEMON""#,
                sorted(&["pikachu"]),
            ),
            (
                "quoted text in an unspaced script",
                r#""進撃の巨人""#,
                sorted(&["shingeki"]),
            ),
            (
                "short quoted text in an unspaced script",
                r#""巨人""#,
                sorted(&["shingeki"]),
            ),
            ("short quoted text", r#""拉拉""#, sorted(&["rilakkuma"])),
            ("a short quoted number", r#"up "4""#, sorted(&["up"])),
            (
                "dots in a title are not spaces",
                r#""toy story 4""#,
                sorted(&[]),
            ),
            (
                "an unclosed quote",
                r#"dune "part two"#,
                sorted(&["dune_uhd", "dune_cam"]),
            ),
            (
                "curly quotes",
                "“part two”",
                sorted(&["dune_uhd", "dune_cam"]),
            ),
        ]);
    }

    #[test]
    fn search_exclusions() {
        assert_finds(&[
            // Exclusions remove whole words, so "-cam" keeps El Camino and HDCAM.
            (
                "an exclusion is a whole word",
                "dune -cam",
                without(DUNE, &["dune_cam"]),
            ),
            (
                "two exclusions",
                "dune -cam -hdcam",
                without(DUNE, &["dune_cam", "dune_hdcam"]),
            ),
            (
                "a pattern excludes inside words",
                "dune -*cam",
                without(DUNE, &["dune_cam", "dune_hdcam"]),
            ),
            (
                "only an exclusion is everything else",
                "-cam",
                all_but(&["dune_cam"]),
            ),
            (
                "an exclusion of a longer word",
                "breaking bad -camino",
                sorted(&["bb_s05e13", "bb_s05e14"]),
            ),
            (
                "an exclusion doesn't cut into words",
                "breaking bad -cam",
                sorted(&["bb_s05e13", "bb_s05e14", "el_camino"]),
            ),
            (
                "an exclusion after a dash",
                "seinfeld -x265",
                without(SEINFELD, &["seinfeld_s05e11"]),
            ),
            (
                "an excluded word with a hyphen is a phrase",
                "seinfeld -web-dl",
                without(SEINFELD, &["seinfeld_s05e16"]),
            ),
            (
                "an excluded quote",
                r#"seinfeld -"web-dl""#,
                without(SEINFELD, &["seinfeld_s05e16"]),
            ),
            (
                "an excluded quote is exact too",
                r#"seinfeld -"web dl""#,
                sorted(SEINFELD),
            ),
            (
                "a lone dash is not an exclusion",
                "seinfeld - x265",
                sorted(&["seinfeld_s05e11"]),
            ),
            (
                "two dashes are one",
                "seinfeld --x265",
                without(SEINFELD, &["seinfeld_s05e11"]),
            ),
            (
                "a substring survives its whole word's exclusion",
                "dune -dune",
                sorted(&["dunes"]),
            ),
            (
                "an exclusion ignores accents",
                "pikachu -pokemon",
                sorted(&[]),
            ),
            (
                "an exclusion in an unspaced script",
                "2160p -巨人",
                sorted(&["dune_uhd", "prophecy4"]),
            ),
            (
                "an exclusion reads descriptions",
                "cabin -couple",
                sorted(&["cabin_woods"]),
            ),
            ("exclusions of nothing", "-xyzzy", all_but(&[])),
        ]);
    }

    #[test]
    fn search_alternatives() {
        assert_finds(&[
            // A bar is OR, and binds tighter than the space between words.
            (
                "alternatives",
                "dune 2160p|1080p",
                sorted(&["dune_uhd", "dune_web", "prophecy3", "prophecy4"]),
            ),
            (
                "a doubled bar",
                "dune 2160p||1080p",
                sorted(&["dune_uhd", "dune_web", "prophecy3", "prophecy4"]),
            ),
            (
                "short alternatives are whole words",
                "toy story 3|4",
                sorted(&["toy3", "toy4"]),
            ),
            ("a short alternative", "up 4k|8k", sorted(&["up"])),
            ("no alternative matches", "up 8k|16k", sorted(&[])),
            (
                "alternatives as the whole query",
                "dune|seinfeld",
                sorted(&[DUNE, SEINFELD].concat()),
            ),
            (
                "a bar binds tighter than a space",
                "dune|seinfeld 1080p",
                sorted(&[
                    "dune_web",
                    "prophecy3",
                    "seinfeld_s05e11",
                    "seinfeld_s05e16",
                ]),
            ),
            (
                "a quote as an alternative",
                r#""part two"|prophecy"#,
                sorted(&["dune_uhd", "dune_cam", "prophecy3", "prophecy4"]),
            ),
            (
                "a bar next to an exclusion is ignored",
                "dune 2160p|-cam",
                sorted(&["dune_uhd", "prophecy4"]),
            ),
            ("stray bars", "| dune |", sorted(DUNE)),
            (
                "episodes as alternatives",
                "s05e13|s05e14",
                sorted(&["bb_s05e13", "bb_s05e14"]),
            ),
            (
                "alternatives of different shapes",
                "breaking bad s05e13|camino",
                sorted(&["bb_s05e13", "el_camino"]),
            ),
            (
                "unspaced alternatives",
                "進撃|拉拉熊",
                sorted(&["shingeki", "rilakkuma"]),
            ),
            (
                "short unspaced alternatives",
                "進撃|拉拉",
                sorted(&["shingeki", "rilakkuma"]),
            ),
            (
                "an alternative matching nothing",
                "dune xyzzy|2160p",
                sorted(&["dune_uhd", "prophecy4"]),
            ),
            (
                "a bar inside a title",
                "cyberpunk|edgerunners",
                sorted(&["cyberpunk"]),
            ),
        ]);
    }

    #[test]
    fn search_patterns() {
        assert_finds(&[
            // A star is any run of characters. A word it ends is a prefix of
            // a word; one it starts can begin anywhere.
            (
                "a trailing star is a word prefix",
                "s05e1*",
                sorted(&[
                    "seinfeld_s05e11",
                    "seinfeld_s05e16",
                    "bb_s05e13",
                    "bb_s05e14",
                ]),
            ),
            (
                "a prefix with other words",
                "seinfeld s05e1*",
                sorted(&["seinfeld_s05e11", "seinfeld_s05e16"]),
            ),
            (
                "a prefix starts a word",
                "cam*",
                sorted(&["dune_cam", "el_camino", "camera"]),
            ),
            (
                "a leading star starts anywhere",
                "*cam",
                sorted(&["dune_hdcam", "dune_cam", "el_camino", "camera", "f9"]),
            ),
            (
                "a star in the middle",
                "s0*e14",
                sorted(&["seinfeld_s03e14", "bb_s05e14"]),
            ),
            (
                "a star across words",
                "breaking*bad",
                sorted(&["bb_s05e13", "bb_s05e14", "el_camino"]),
            ),
            ("a short prefix", "toy story 4*", sorted(&["toy4"])),
            (
                "a prefix of three characters",
                "x26*",
                sorted(&[
                    "dune_uhd",
                    "dune_hdcam",
                    "dune_cam",
                    "seinfeld_s05e11",
                    "seinfeld_s03e14",
                    "f9",
                ]),
            ),
            (
                "prefixes as alternatives",
                "s05e1*|s03e*",
                sorted(&[
                    "seinfeld_s05e11",
                    "seinfeld_s05e16",
                    "seinfeld_s03e14",
                    "bb_s05e13",
                    "bb_s05e14",
                ]),
            ),
            (
                "an excluded prefix",
                "dune -s01e*",
                without(DUNE, &["prophecy3", "prophecy4"]),
            ),
            (
                "a prefix in an unspaced script",
                "進撃*",
                sorted(&["shingeki"]),
            ),
            (
                "stars around an unspaced word",
                "*巨人*",
                sorted(&["shingeki"]),
            ),
            ("a prefix ignores accents", "poke*", sorted(&["pikachu"])),
            (
                "a prefix reads descriptions",
                "cabin wood*",
                sorted(&["cabin_woods", "cabin"]),
            ),
            ("a lone star next to a word", "dune *", sorted(DUNE)),
        ]);
    }

    #[test]
    fn search_plain_words_and_combinations() {
        assert_finds(&[
            // No operator, no change.
            ("plain words are substrings", "dune", sorted(DUNE)),
            ("FTS5 operators are words", "NOT", sorted(&["not_okay"])),
            (
                "a trailing hyphen is punctuation",
                "x265-",
                sorted(&["dune_uhd", "seinfeld_s05e11", "f9"]),
            ),
            (
                "a short last word is a prefix while typed",
                "up 4",
                sorted(&["up"]),
            ),
            (
                "a bar inside a title isn't needed to find it",
                "cyberpunk edgerunners",
                sorted(&["cyberpunk"]),
            ),
            (
                "a title with a dash in front",
                "seinfeld conversion",
                sorted(&["seinfeld_s05e11"]),
            ),
            ("descriptions", "couple", sorted(&["cabin"])),
            ("handles", "zicktoonz", sorted(&["creator"])),
            ("display names", "zick toonz", sorted(&["creator"])),
            // Everything at once.
            (
                "quote, alternative, exclusion and prefix",
                r#""part.two" 2160p|1080p -cam -x26*"#,
                sorted(&["dune_web"]),
            ),
            (
                "alternatives of exclusions don't exist",
                "dune -cam|-hdcam",
                without(DUNE, &["dune_cam", "dune_hdcam"]),
            ),
        ]);
    }

    #[test]
    fn titles_rank_above_authors_and_descriptions() {
        let mut conn = memory_db();
        insert_new(
            &mut conn,
            "youtube",
            &[
                NewVideo {
                    title: Some("Something else"),
                    author_name: Some("Piano Guy"),
                    ..video("author", true)
                },
                NewVideo {
                    title: Some("Piano solo"),
                    ..video("title", true)
                },
                NewVideo {
                    title: Some("Quiet"),
                    ..video("description", true)
                },
            ],
        )
        .unwrap();
        let info = r#"{"description": "a piano piece"}"#;
        let done = Downloaded {
            dir: "youtube/description",
            files: &[],
            bytes: 0,
            format: None,
            info_json: Some(info),
            posted_at: None,
        };
        record_download(&mut conn, "youtube", "description", &done).unwrap();
        let best = Order {
            sort: Sort::Best,
            dir: Dir::Asc,
        };
        let ranked = ids(videos_page(&conn, None, &query::parse("piano"), best, 10, 0).unwrap());
        assert_eq!(ranked, ["title", "author", "description"]);
        // A platform narrows a search, and its count.
        let query = query::parse("piano");
        assert!(
            videos_page(&conn, Some("tiktok"), &query, best, 10, 0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(videos_count(&conn, Some("youtube"), &query).unwrap(), 3);
        assert_eq!(videos_count(&conn, Some("tiktok"), &query).unwrap(), 0);
    }

    #[test]
    fn input_that_could_break_the_sql_or_fts5_is_searched_safely() {
        let conn = search_db();
        let long_words: Vec<String> = (1..=500).map(|n| format!("w{n}")).collect();
        let excluded: Vec<String> = (1..=500).map(|n| format!("-w{n}")).collect();
        let phrases: Vec<String> = (1..=200).map(|n| format!("\"w{n} x\"")).collect();
        let patterns: Vec<String> = (1..=200).map(|n| format!("w{n}*z")).collect();
        let hostile = [
            r#"""#.to_string(),
            r#"""""#.into(),
            r#"-""#.into(),
            r#"|"|"#.into(),
            r#"*"*"#.into(),
            "\\".into(),
            "'".into(),
            "' OR 1=1 --".into(),
            "NEAR(a b)".into(),
            "{title}: dune".into(),
            "title:dune".into(),
            "description : dune".into(),
            "dune^".into(),
            "(".into(),
            ")".into(),
            r#""a" OR "b""#.into(),
            "-(".into(),
            "*-*".into(),
            "||||".into(),
            "-|-".into(),
            "dune\u{0}cam".into(),
            "🔥 dune 🔥".into(),
            "é".into(),
            "دون".into(),
            "a".repeat(10_000),
            "-".repeat(1000),
            long_words.join("|"),
            excluded.join(" "),
            phrases.join(" "),
            patterns.join(" "),
            "%".into(),
            "_".into(),
            "[a-z]*".into(),
            "?".into(),
            "dune [".into(),
            "dune ]*".into(),
            "s0?e1*".into(),
            "**a**b**".into(),
        ];
        for text in &hostile {
            found(&conn, text);
        }
    }

    #[test]
    fn glob_characters_in_a_pattern_are_punctuation() {
        let mut conn = memory_db();
        insert_new(
            &mut conn,
            "youtube",
            &[NewVideo {
                title: Some("Question? Mark [Remastered]"),
                ..video("q", true)
            }],
        )
        .unwrap();
        assert_eq!(found(&conn, "q*ion"), ["q"]);
        assert_eq!(found(&conn, "ques*[remastered]"), ["q"]);
        // As a GLOB class, "[a-z]" would match the "q" of "Question".
        assert!(found(&conn, "[a-z]uestion*").is_empty());
    }

    #[test]
    fn migrating_indexes_the_videos_already_stored() {
        // A database from before search, as v0.1.0 left it.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO videos (platform, video_id, url, title, author_name, raw_json, liked_at) VALUES \
             ('tiktok', 't', 'u', 'Cat piano', 'kitty', '{\"author\": {\"nickname\": \"Kitty Kat\"}}', 0), \
             ('youtube', 'y', 'u', 'Dog walk', 'Doggo', 'not json', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO downloads (platform, video_id, downloaded_at, info_json) \
             VALUES ('youtube', 'y', 1, '{\"description\": \"a long walk\"}')",
            [],
        )
        .unwrap();
        init(&mut conn).unwrap();
        assert_eq!(found(&conn, "piano"), ["t"]);
        assert_eq!(found(&conn, "kat"), ["t"]);
        assert_eq!(found(&conn, "doggo"), ["y"]);
        assert_eq!(found(&conn, "long walk"), ["y"]);
        // Both indexes agree with their content.
        for index in ["video_search", "video_words"] {
            conn.execute(
                &format!("INSERT INTO {index} ({index}) VALUES ('integrity-check')"),
                [],
            )
            .unwrap();
        }
        // Videos stored afterwards are indexed as they arrive, and a later
        // download's description replaces nothing else.
        insert_new(
            &mut conn,
            "youtube",
            &[NewVideo {
                title: Some("Bird song"),
                ..video("b", true)
            }],
        )
        .unwrap();
        assert_eq!(found(&conn, "bird"), ["b"]);
        let info = r#"{"description": "recorded at dawn"}"#;
        let done = Downloaded {
            dir: "youtube/b",
            files: &[],
            bytes: 0,
            format: None,
            info_json: Some(info),
            posted_at: None,
        };
        record_download_failure(&conn, "youtube", "b", "HTTP 429").unwrap();
        record_download(&mut conn, "youtube", "b", &done).unwrap();
        assert_eq!(found(&conn, "dawn"), ["b"]);
        assert_eq!(found(&conn, "bird"), ["b"]);
        for index in ["video_search", "video_words"] {
            conn.execute(
                &format!("INSERT INTO {index} ({index}) VALUES ('integrity-check')"),
                [],
            )
            .unwrap();
        }
    }

    #[test]
    fn sorts_by_each_column_with_empty_values_last() {
        let mut conn = memory_db();
        insert_new(
            &mut conn,
            "youtube",
            &[
                NewVideo {
                    title: Some("banana"),
                    author_name: Some("Zed"),
                    duration_seconds: Some(30),
                    posted_at: Some(200),
                    ..video("b", true)
                },
                NewVideo {
                    title: None,
                    author_name: None,
                    duration_seconds: None,
                    posted_at: None,
                    ..video("none", true)
                },
                NewVideo {
                    title: Some("Apple"),
                    author_name: Some("amy"),
                    duration_seconds: Some(90),
                    posted_at: Some(100),
                    ..video("a", true)
                },
                NewVideo {
                    title: Some("cherry"),
                    author_name: Some("Bob"),
                    duration_seconds: Some(30),
                    posted_at: Some(300),
                    ..video("c", true)
                },
            ],
        )
        .unwrap();
        let page = |sort, dir| {
            ids(videos_page(&conn, None, &Query::default(), Order { sort, dir }, 10, 0).unwrap())
        };
        assert_eq!(page(Sort::Title, Dir::Asc), ["a", "b", "c", "none"]);
        assert_eq!(page(Sort::Title, Dir::Desc), ["c", "b", "a", "none"]);
        assert_eq!(page(Sort::Author, Dir::Asc), ["a", "c", "b", "none"]);
        // Equal lengths keep like order, reversed with the rest.
        assert_eq!(page(Sort::Length, Dir::Asc), ["b", "c", "a", "none"]);
        assert_eq!(page(Sort::Length, Dir::Desc), ["a", "c", "b", "none"]);
        assert_eq!(page(Sort::Posted, Dir::Desc), ["c", "b", "a", "none"]);
        assert_eq!(page(Sort::Posted, Dir::Asc), ["a", "b", "c", "none"]);
        assert_eq!(page(Sort::Added, Dir::Asc), ["b", "none", "a", "c"]);
        assert_eq!(page(Sort::Added, Dir::Desc), ["c", "a", "none", "b"]);
        assert_eq!(page(Sort::Best, Dir::Asc), ["c", "a", "none", "b"]);
    }

    #[test]
    fn column_sorts_read_their_index() {
        // SQLite plans an empty table differently from an archive.
        let mut conn = memory_db();
        let ids: Vec<String> = (0..2000).map(|n| format!("v{n}")).collect();
        let videos: Vec<NewVideo> = ids.iter().map(|id| video(id, true)).collect();
        insert_new(&mut conn, "youtube", &videos).unwrap();
        for sort in [Sort::Title, Sort::Author, Sort::Length, Sort::Posted] {
            for dir in [Dir::Asc, Dir::Desc] {
                let sql = format!(
                    "EXPLAIN QUERY PLAN SELECT v.rowid FROM videos v \
                     WHERE ?1 IS NULL OR v.platform = ?1 ORDER BY {} LIMIT 50",
                    order_by(Order { sort, dir }, false)
                );
                let plan: Vec<String> = conn
                    .prepare(&sql)
                    .unwrap()
                    .query_map([Option::<&str>::None], |r| r.get(3))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                let plan = plan.join("\n");
                assert!(
                    plan.contains("USING INDEX videos_"),
                    "{sort:?} {dir:?}:\n{plan}"
                );
                assert!(!plan.contains("TEMP B-TREE"), "{sort:?} {dir:?}:\n{plan}");
            }
        }
    }
}
