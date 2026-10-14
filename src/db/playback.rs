//! Playback copies (`transcode.rs`): which downloaded videos still need one,
//! and what each try made. A copy belongs to the download it was made from:
//! a new download deletes the row, and a copy of an earlier one isn't recorded.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use super::downloads::{MAX_RETRY_DELAY, PENDING_COLUMNS, PendingDownload, pending_from_row};
use super::now;

/// A downloaded video that needs its playback copy made (or checked for).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPlayback {
    pub item: PendingDownload,
    /// The download's folder, relative to the media root.
    pub folder: String,
    /// The file names in `folder`.
    pub files: Vec<String>,
    /// When the download was recorded, which `record_playback` checks is
    /// still the one the copy was made from.
    pub downloaded_at: i64,
}

/// Which downloaded videos still need a playback copy: downloaded videos
/// (photo posts have nothing to convert) with no copy made, not waiting out
/// a retry delay. `?1` is `MAX_RETRY_DELAY` and `?2` now; failures back off
/// as downloads do.
const PLAYBACK_TABLES: &str = "videos v \
     JOIN downloads d ON d.platform = v.platform AND d.video_id = v.video_id \
     LEFT JOIN playback_copies p ON p.platform = v.platform AND p.video_id = v.video_id";
const PLAYBACK_WHERE: &str = "d.downloaded_at IS NOT NULL AND v.kind = 'video' \
     AND p.finished_at IS NULL \
     AND (p.failures IS NULL OR p.failures = 0 \
          OR p.last_attempt_at + MIN(3600 << MIN(p.failures - 1, 20), ?1) <= ?2)";

/// `PendingPlayback`'s columns after `PENDING_COLUMNS`.
const PLAYBACK_COLUMNS: &str = "d.folder, d.files_json, d.downloaded_at";

fn pending_playback_from_row(
    r: &rusqlite::Row,
) -> rusqlite::Result<(PendingDownload, String, String, i64)> {
    Ok((pending_from_row(r)?, r.get(7)?, r.get(8)?, r.get(9)?))
}

fn pending_playback_from(
    (item, folder, files, downloaded_at): (PendingDownload, String, String, i64),
) -> Result<PendingPlayback> {
    let files = serde_json::from_str(&files).with_context(|| {
        format!(
            "reading the file list of {} video {}",
            item.platform, item.video_id
        )
    })?;
    Ok(PendingPlayback {
        item,
        folder,
        files,
        downloaded_at,
    })
}

/// Up to `limit` downloaded videos to make playback copies of, most recently
/// stored first, as the downloads go.
pub fn pending_playback(conn: &Connection, limit: usize) -> Result<Vec<PendingPlayback>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {PENDING_COLUMNS}, {PLAYBACK_COLUMNS} FROM {PLAYBACK_TABLES} \
         WHERE {PLAYBACK_WHERE} ORDER BY v.rowid DESC LIMIT ?3"
    ))?;
    let rows = stmt
        .query_map(
            params![MAX_RETRY_DELAY, now(), i64::try_from(limit)?],
            pending_playback_from_row,
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter().map(pending_playback_from).collect()
}

/// How many videos `pending_playback` would offer.
pub fn pending_playback_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row(
        &format!("SELECT count(*) FROM {PLAYBACK_TABLES} WHERE {PLAYBACK_WHERE}"),
        params![MAX_RETRY_DELAY, now()],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count)?)
}

/// One video `pending_playback` would offer, for a page that can't play its
/// original and asks for the copy now. A copy waiting out a retry delay
/// isn't offered: a page reloads once a try ends, and asking again on every
/// load would retry a failing copy over and over.
pub fn playback_item(
    conn: &Connection,
    platform: &str,
    video_id: &str,
) -> Result<Option<PendingPlayback>> {
    let found = conn
        .query_row(
            &format!(
                "SELECT {PENDING_COLUMNS}, {PLAYBACK_COLUMNS} FROM {PLAYBACK_TABLES} \
                 WHERE {PLAYBACK_WHERE} AND v.platform = ?3 AND v.video_id = ?4"
            ),
            params![MAX_RETRY_DELAY, now(), platform, video_id],
            pending_playback_from_row,
        )
        .optional()?;
    found.map(pending_playback_from).transpose()
}

/// Forget a video's playback copy, so it's made again: its file has gone.
pub fn forget_playback(conn: &Connection, platform: &str, video_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM playback_copies WHERE platform = ?1 AND video_id = ?2",
        params![platform, video_id],
    )?;
    Ok(())
}

/// A finished playback copy (or the finding that none is needed), as
/// `record_playback` stores it.
pub struct MadePlayback<'a> {
    /// The download's folder it was made in, and when that download was
    /// recorded.
    pub download_folder: &'a str,
    pub downloaded_at: i64,
    pub file_name: Option<&'a str>,
    pub size_bytes: u64,
    pub method: &'a str,
    pub original_config_json: Option<&'a str>,
}

/// Record a finished playback copy. Only if the download it was made from
/// is still the video's: a new download in between (which deletes the row)
/// gets its own copy. False when it was dropped for that reason.
pub fn record_playback(
    conn: &Connection,
    platform: &str,
    video_id: &str,
    made: &MadePlayback,
) -> Result<bool> {
    // The WHERE also keeps SQLite from reading ON CONFLICT as a join.
    let stored = conn.execute(
        &format!(
            "INSERT INTO playback_copies \
               (platform, video_id, download_folder, last_attempt_at, finished_at, file_name, \
                size_bytes, method, original_config_json) \
             SELECT ?1, ?2, ?3, ?4, ?4, ?5, ?6, ?7, ?8 WHERE {SAME_DOWNLOAD} \
             ON CONFLICT (platform, video_id) DO UPDATE SET \
               download_folder = excluded.download_folder, \
               last_attempt_at = excluded.last_attempt_at, finished_at = excluded.finished_at, \
               last_error = NULL, file_name = excluded.file_name, size_bytes = excluded.size_bytes, \
               method = excluded.method, original_config_json = excluded.original_config_json"
        ),
        params![
            platform,
            video_id,
            made.download_folder,
            now(),
            made.file_name,
            i64::try_from(made.size_bytes)?,
            made.method,
            made.original_config_json,
            made.downloaded_at,
        ],
    )?;
    Ok(stored > 0)
}

/// Whether the download in `?3` recorded at `?9` is still video `?1`/`?2`'s.
const SAME_DOWNLOAD: &str = "EXISTS (SELECT 1 FROM downloads \
     WHERE platform = ?1 AND video_id = ?2 AND folder = ?3 AND downloaded_at = ?9)";

/// Count a failed try at a playback copy and remember why it failed, unless
/// the video has been downloaded again since (see `record_playback`).
pub fn record_playback_failure(
    conn: &Connection,
    platform: &str,
    video_id: &str,
    from: (&str, i64),
    error: &str,
) -> Result<()> {
    let (folder, downloaded_at) = from;
    conn.execute(
        "INSERT INTO playback_copies \
           (platform, video_id, download_folder, failures, last_attempt_at, last_error) \
         SELECT ?1, ?2, ?3, 1, ?4, ?5 WHERE EXISTS (SELECT 1 FROM downloads \
           WHERE platform = ?1 AND video_id = ?2 AND folder = ?3 AND downloaded_at = ?6) \
         ON CONFLICT (platform, video_id) DO UPDATE SET \
           download_folder = excluded.download_folder, failures = failures + 1, \
           last_attempt_at = excluded.last_attempt_at, last_error = excluded.last_error",
        params![platform, video_id, folder, now(), error, downloaded_at],
    )?;
    Ok(())
}

/// Where a video's playback copy stands, once one has been tried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackState {
    pub download_folder: String,
    pub failures: i64,
    pub last_error: Option<String>,
    pub finished_at: Option<i64>,
    /// The copy's file name; None when none was needed.
    pub file_name: Option<String>,
    pub size_bytes: Option<i64>,
    pub method: Option<String>,
    /// The original's `MediaCapabilities` video configuration, JSON.
    pub original_config_json: Option<String>,
}

pub fn playback_state(
    conn: &Connection,
    platform: &str,
    video_id: &str,
) -> Result<Option<PlaybackState>> {
    let found = conn
        .query_row(
            "SELECT download_folder, failures, last_error, finished_at, file_name, size_bytes, \
               method, original_config_json \
             FROM playback_copies WHERE platform = ?1 AND video_id = ?2",
            [platform, video_id],
            |r| {
                Ok(PlaybackState {
                    download_folder: r.get(0)?,
                    failures: r.get(1)?,
                    last_error: r.get(2)?,
                    finished_at: r.get(3)?,
                    file_name: r.get(4)?,
                    size_bytes: r.get(5)?,
                    method: r.get(6)?,
                    original_config_json: r.get(7)?,
                })
            },
        )
        .optional()?;
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::*;
    use crate::db::*;

    fn playback_ids(conn: &Connection) -> Vec<String> {
        pending_playback(conn, 10)
            .unwrap()
            .into_iter()
            .map(|p| p.item.video_id)
            .collect()
    }

    #[test]
    fn downloaded_videos_queue_for_playback_newest_first() {
        let mut conn = memory_db();
        let photo = NewVideo {
            kind: "photo",
            ..new_video("photo", true)
        };
        insert_new(
            &mut conn,
            "tiktok",
            &[
                new_video("old", true),
                new_video("new", true),
                photo,
                new_video("not yet", true),
            ],
        )
        .unwrap();
        for id in ["old", "new", "photo"] {
            download(&mut conn, "tiktok", id);
        }
        // As if downloaded a while ago, so downloading again is told apart.
        conn.execute(
            "UPDATE downloads SET downloaded_at = downloaded_at - 10",
            [],
        )
        .unwrap();
        // Photo posts and videos not downloaded have nothing to convert.
        assert_eq!(playback_ids(&conn), ["new", "old"]);
        assert_eq!(pending_playback_count(&conn).unwrap(), 2);
        let next = &pending_playback(&conn, 1).unwrap()[0];
        assert_eq!(next.folder, "tiktok/new");
        assert_eq!(next.files, ["thumbnail.jpg", "video.webm"]);

        let downloaded_at = |id: &str| -> i64 {
            conn.query_row(
                "SELECT downloaded_at FROM downloads WHERE video_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        let made = MadePlayback {
            download_folder: "tiktok/new",
            downloaded_at: downloaded_at("new"),
            file_name: Some("playback.mp4"),
            size_bytes: 20,
            method: "encode",
            original_config_json: Some("{}"),
        };
        assert!(record_playback(&conn, "tiktok", "new", &made).unwrap());
        let none_needed = MadePlayback {
            file_name: None,
            method: "none",
            original_config_json: None,
            size_bytes: 0,
            download_folder: "tiktok/old",
            downloaded_at: downloaded_at("old"),
        };
        assert!(record_playback(&conn, "tiktok", "old", &none_needed).unwrap());
        assert!(playback_ids(&conn).is_empty());
        let state = playback_state(&conn, "tiktok", "new").unwrap().unwrap();
        assert_eq!(state.file_name.as_deref(), Some("playback.mp4"));
        assert_eq!(state.method.as_deref(), Some("encode"));
        assert!(state.finished_at.is_some());
        assert_eq!(
            playback_state(&conn, "tiktok", "old")
                .unwrap()
                .unwrap()
                .file_name,
            None
        );
        assert!(playback_item(&conn, "tiktok", "new").unwrap().is_none());

        // Downloading the video again starts its copy over, and a copy of
        // the earlier download that finishes afterwards isn't recorded.
        download(&mut conn, "tiktok", "new");
        assert!(playback_state(&conn, "tiktok", "new").unwrap().is_none());
        assert_eq!(playback_ids(&conn), ["new"]);
        assert!(!record_playback(&conn, "tiktok", "new", &made).unwrap());
        record_playback_failure(
            &conn,
            "tiktok",
            "new",
            ("tiktok/new", made.downloaded_at),
            "late",
        )
        .unwrap();
        assert!(playback_state(&conn, "tiktok", "new").unwrap().is_none());

        forget_playback(&conn, "tiktok", "old").unwrap();
        assert_eq!(playback_ids(&conn), ["new", "old"]);
    }

    #[test]
    fn failed_playback_copies_wait_before_retrying() {
        let mut conn = memory_db();
        insert_new(&mut conn, "youtube", &[new_video("a", true)]).unwrap();
        download(&mut conn, "youtube", "a");
        let next = &pending_playback(&conn, 1).unwrap()[0];
        record_playback_failure(
            &conn,
            "youtube",
            "a",
            (&next.folder, next.downloaded_at),
            "ffmpeg failed",
        )
        .unwrap();
        assert!(playback_ids(&conn).is_empty());
        let state = playback_state(&conn, "youtube", "a").unwrap().unwrap();
        assert_eq!(state.failures, 1);
        assert_eq!(state.last_error.as_deref(), Some("ffmpeg failed"));
        // Asking for it directly waits too.
        assert!(playback_item(&conn, "youtube", "a").unwrap().is_none());
        assert!(playback_item(&conn, "youtube", "b").unwrap().is_none());

        conn.execute(
            "UPDATE playback_copies SET last_attempt_at = last_attempt_at - 3600",
            [],
        )
        .unwrap();
        assert_eq!(playback_ids(&conn), ["a"]);
        assert!(playback_item(&conn, "youtube", "a").unwrap().is_some());
    }
}
