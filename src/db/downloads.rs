//! The download queue and what each download saved. A failed try is retried
//! after a delay that doubles each time, up to `MAX_RETRY_DELAY`.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use super::now;

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
    /// The local calendar date of `added_at`, as `YYYY-MM-DD`.
    pub added_on: String,
}

/// `PendingDownload`'s columns, for `pending_from_row`. SQLite turns
/// `added_at` into a date in the system's time zone, the user's own.
pub(super) const PENDING_COLUMNS: &str = "v.platform, v.video_id, v.url, v.kind, v.title, v.author_name, \
     date(v.added_at, 'unixepoch', 'localtime')";

pub(super) fn pending_from_row(r: &rusqlite::Row) -> rusqlite::Result<PendingDownload> {
    Ok(PendingDownload {
        platform: r.get(0)?,
        video_id: r.get(1)?,
        url: r.get(2)?,
        kind: r.get(3)?,
        title: r.get(4)?,
        author_name: r.get(5)?,
        added_on: r.get(6)?,
    })
}

/// A finished download, as `record_download` stores it.
pub struct Downloaded<'a> {
    /// The video's folder, relative to the media root.
    pub folder: &'a str,
    pub files: &'a [String],
    pub size_bytes: u64,
    pub formats: Option<&'a str>,
    pub metadata_json: Option<&'a str>,
    /// When the video was posted, if the download's metadata says.
    pub posted_at: Option<i64>,
}

/// A failed download is retried an hour later, then two hours, four, and so
/// on, but never more than this long after: a week, in seconds. It's never
/// given up on, because many failures aren't the video's fault (a yt-dlp
/// broken by a site change, an expired login, an outage) and can last days.
pub(super) const MAX_RETRY_DELAY: i64 = 7 * 24 * 3600;

/// Which videos are waiting to be downloaded: available, not downloaded, not
/// gone by the platform's word, and not waiting out a retry delay. `?1` is `MAX_RETRY_DELAY`, `?2` now, and
/// `?3` a platform or NULL for all.
const PENDING_WHERE: &str = "v.available = 1 AND d.downloaded_at IS NULL \
     AND d.unavailable_at IS NULL \
     AND (?3 IS NULL OR v.platform = ?3) \
     AND (d.failures IS NULL OR d.failures = 0 \
          OR d.last_attempt_at + MIN(3600 << MIN(d.failures - 1, 20), ?1) <= ?2)";

/// Up to `limit` videos to download next, on `platform` or all of them, most
/// recently liked first (by `like_order`), so new likes don't wait behind
/// the backlog. Skips videos that are unavailable, already downloaded, or
/// waiting out a retry delay.
pub fn pending_downloads(
    conn: &Connection,
    platform: Option<&str>,
    limit: usize,
) -> Result<Vec<PendingDownload>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PENDING_COLUMNS} FROM videos v \
         LEFT JOIN downloads d ON d.platform = v.platform AND d.video_id = v.video_id \
         WHERE {PENDING_WHERE} ORDER BY v.like_order DESC, v.rowid DESC LIMIT ?4"
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

/// Count a failed download attempt and remember why it failed. `gone` means
/// the platform said the video is unavailable, so it's never tried again.
pub fn record_download_failure(
    conn: &Connection,
    platform: &str,
    video_id: &str,
    error: &str,
    gone: bool,
) -> Result<()> {
    let at = now();
    conn.execute(
        "INSERT INTO downloads (platform, video_id, failures, last_attempt_at, last_error, unavailable_at) \
         VALUES (?1, ?2, 1, ?3, ?4, ?5) \
         ON CONFLICT (platform, video_id) DO UPDATE SET \
           failures = failures + 1, last_attempt_at = excluded.last_attempt_at, \
           last_error = excluded.last_error, unavailable_at = excluded.unavailable_at",
        params![platform, video_id, at, error, gone.then_some(at)],
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
    // Immediate: a deferred transaction that has to wait for another
    // connection's write fails with "database is locked" instead of waiting.
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO downloads \
           (platform, video_id, last_attempt_at, downloaded_at, folder, files_json, size_bytes, formats, metadata_json) \
         VALUES (?1, ?2, ?3, ?3, ?4, ?5, ?6, ?7, ?8) \
         ON CONFLICT (platform, video_id) DO UPDATE SET \
           last_attempt_at = excluded.last_attempt_at, downloaded_at = excluded.downloaded_at, \
           last_error = NULL, folder = excluded.folder, files_json = excluded.files_json, \
           size_bytes = excluded.size_bytes, formats = excluded.formats, \
           metadata_json = excluded.metadata_json",
        params![
            platform,
            video_id,
            at,
            done.folder,
            serde_json::to_string(done.files)?,
            i64::try_from(done.size_bytes)?,
            done.formats,
            done.metadata_json,
        ],
    )?;
    tx.execute(
        "UPDATE videos SET posted_at = ?1 \
         WHERE platform = ?2 AND video_id = ?3 AND posted_at IS NULL",
        params![done.posted_at, platform, video_id],
    )?;
    // A playback copy and a loudness measurement belong to the files they
    // were made from.
    for table in ["playback_copies", "loudness"] {
        tx.execute(
            &format!("DELETE FROM {table} WHERE platform = ?1 AND video_id = ?2"),
            params![platform, video_id],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Where a video's download stands, if it has been tried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadState {
    pub failures: i64,
    pub last_error: Option<String>,
    /// When the platform said the video is gone; it's not tried again.
    pub unavailable_at: Option<i64>,
    pub downloaded_at: Option<i64>,
    pub folder: Option<String>,
    /// The file names in `folder`, sorted; empty until downloaded.
    pub files: Vec<String>,
    pub size_bytes: Option<i64>,
}

pub fn download_state(
    conn: &Connection,
    platform: &str,
    video_id: &str,
) -> Result<Option<DownloadState>> {
    let found = conn
        .query_row(
            "SELECT failures, last_error, downloaded_at, folder, size_bytes, files_json, unavailable_at \
             FROM downloads \
             WHERE platform = ?1 AND video_id = ?2",
            [platform, video_id],
            |r| {
                Ok((
                    DownloadState {
                        failures: r.get(0)?,
                        last_error: r.get(1)?,
                        unavailable_at: r.get(6)?,
                        downloaded_at: r.get(2)?,
                        folder: r.get(3)?,
                        files: Vec::new(),
                        size_bytes: r.get(4)?,
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
    use crate::db::test_support::*;
    use crate::db::*;

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
        insert_new(&mut conn, "youtube", &[new_video("a", true)]).unwrap();
        let item = download_item(&conn, "youtube", "a").unwrap().unwrap();
        assert_eq!(item.title.as_deref(), Some("title"));
        assert_eq!(item.added_on.len(), "2026-10-12".len());
        assert!(
            item.added_on
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
            // Listed newest first.
            &[
                new_video("new", true),
                new_video("gone", false),
                new_video("old", true),
            ],
        )
        .unwrap();
        assert_eq!(pending_ids(&conn), ["new", "old"]);
    }

    #[test]
    fn queues_per_platform() {
        let mut conn = memory_db();
        insert_new(&mut conn, "youtube", &[new_video("y", true)]).unwrap();
        insert_new(
            &mut conn,
            "tiktok",
            &[new_video("t2", true), new_video("t1", true)],
        )
        .unwrap();
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
        record_download_failure(&conn, "tiktok", "t2", "flaky", false).unwrap();
        assert_eq!(pending_count(&conn, "tiktok").unwrap(), 1);
        assert_eq!(pending_count(&conn, "instagram").unwrap(), 0);
    }

    #[test]
    fn a_video_the_platform_says_is_gone_is_never_tried_again() {
        let mut conn = memory_db();
        insert_new(&mut conn, "tiktok", &[new_video("a", true)]).unwrap();
        record_download_failure(&conn, "tiktok", "a", "unavailable", true).unwrap();
        // Long past any retry delay.
        conn.execute(
            "UPDATE downloads SET last_attempt_at = last_attempt_at - ?1",
            [2 * MAX_RETRY_DELAY],
        )
        .unwrap();
        assert!(pending_ids(&conn).is_empty());
        // The like stays, with its link and title, and says why.
        let state = download_state(&conn, "tiktok", "a").unwrap().unwrap();
        assert!(state.unavailable_at.is_some());
        assert_eq!(state.last_error.as_deref(), Some("unavailable"));
    }

    #[test]
    fn failed_downloads_wait_before_retrying() {
        let mut conn = memory_db();
        insert_new(&mut conn, "youtube", &[new_video("a", true)]).unwrap();
        record_download_failure(&conn, "youtube", "a", "HTTP 429", false).unwrap();
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
            "UPDATE downloads SET failures = 500, last_attempt_at = ?1",
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
        insert_new(&mut conn, "youtube", &[new_video("a", true)]).unwrap();
        record_download_failure(&conn, "youtube", "a", "flaky", false).unwrap();
        let files = ["video.webm".to_string()];
        let done = |posted_at| Downloaded {
            folder: "youtube/a",
            files: &files,
            size_bytes: 10,
            formats: Some("399+251"),
            metadata_json: None,
            posted_at,
        };
        record_download(&mut conn, "youtube", "a", &done(Some(5))).unwrap();
        assert!(pending_ids(&conn).is_empty());
        let (error, stored_files): (Option<String>, String) = conn
            .query_row("SELECT last_error, files_json FROM downloads", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(error, None);
        assert_eq!(stored_files, r#"["video.webm"]"#);
        let state = download_state(&conn, "youtube", "a").unwrap().unwrap();
        assert_eq!(state.files, ["video.webm"]);
        assert_eq!(state.folder.as_deref(), Some("youtube/a"));

        // posted_at is filled in once, never overwritten.
        record_download(&mut conn, "youtube", "a", &done(Some(9))).unwrap();
        let posted_at: i64 = conn
            .query_row("SELECT posted_at FROM videos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(posted_at, 5);
    }
}
