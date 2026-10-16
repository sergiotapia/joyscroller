//! Loudness measurements (`loudness.rs`): which downloaded videos still need
//! measuring, and what each measurement found. Like a playback copy, a
//! measurement belongs to the download it was made from: a new download
//! deletes the row, and one of an earlier download isn't recorded.

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use super::downloads::MAX_RETRY_DELAY;
use super::now;

/// A downloaded video (or photo post) whose sound still needs measuring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLoudness {
    pub platform: String,
    pub video_id: String,
    /// The download's folder, relative to the media root.
    pub folder: String,
    /// The file names in `folder`.
    pub files: Vec<String>,
    /// When the download was recorded, which `record_loudness` checks is
    /// still the one that was measured.
    pub downloaded_at: i64,
}

/// What a measurement found: the sound's integrated loudness in LUFS and
/// its true peak in dBTP, or None for both when it has no sound.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    pub integrated_lufs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
}

/// Downloads not measured yet (or measured in a folder since replaced), not
/// waiting out a retry delay. `?1` is `MAX_RETRY_DELAY` and `?2` now;
/// failures back off as downloads do.
const LOUDNESS_WHERE: &str = "d.downloaded_at IS NOT NULL \
     AND (l.platform IS NULL OR l.download_folder != d.folder OR (l.measured_at IS NULL \
          AND (l.failures = 0 OR l.last_attempt_at + MIN(3600 << MIN(l.failures - 1, 20), ?1) <= ?2)))";

const LOUDNESS_TABLES: &str = "videos v \
     JOIN downloads d ON d.platform = v.platform AND d.video_id = v.video_id \
     LEFT JOIN loudness l ON l.platform = v.platform AND l.video_id = v.video_id";

/// Up to `limit` downloads to measure, most recently liked first, as the
/// downloads go.
pub fn pending_loudness(conn: &Connection, limit: usize) -> Result<Vec<PendingLoudness>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT v.platform, v.video_id, d.folder, d.files_json, d.downloaded_at \
         FROM {LOUDNESS_TABLES} WHERE {LOUDNESS_WHERE} \
         ORDER BY v.like_order DESC, v.rowid DESC LIMIT ?3"
    ))?;
    let rows = stmt
        .query_map(
            params![MAX_RETRY_DELAY, now(), i64::try_from(limit)?],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(platform, video_id, folder, files, downloaded_at)| {
            let files = serde_json::from_str(&files)
                .with_context(|| format!("reading the file list of {platform} video {video_id}"))?;
            Ok(PendingLoudness {
                platform,
                video_id,
                folder,
                files,
                downloaded_at,
            })
        })
        .collect()
}

/// Whether the download in `?3` recorded at `?4` is still video `?1`/`?2`'s.
const SAME_DOWNLOAD: &str = "EXISTS (SELECT 1 FROM downloads \
     WHERE platform = ?1 AND video_id = ?2 AND folder = ?3 AND downloaded_at = ?4)";

/// Record what measuring `pending` found, unless the video has been
/// downloaded again since. False when it was dropped for that reason.
pub fn record_loudness(
    conn: &Connection,
    pending: &PendingLoudness,
    found: Loudness,
) -> Result<bool> {
    // The WHERE also keeps SQLite from reading ON CONFLICT as a join.
    let stored = conn.execute(
        &format!(
            "INSERT INTO loudness \
               (platform, video_id, download_folder, last_attempt_at, measured_at, \
                integrated_lufs, true_peak_dbtp) \
             SELECT ?1, ?2, ?3, ?5, ?5, ?6, ?7 WHERE {SAME_DOWNLOAD} \
             ON CONFLICT (platform, video_id) DO UPDATE SET \
               download_folder = excluded.download_folder, failures = 0, \
               last_attempt_at = excluded.last_attempt_at, last_error = NULL, \
               measured_at = excluded.measured_at, integrated_lufs = excluded.integrated_lufs, \
               true_peak_dbtp = excluded.true_peak_dbtp"
        ),
        params![
            pending.platform,
            pending.video_id,
            pending.folder,
            pending.downloaded_at,
            now(),
            found.integrated_lufs,
            found.true_peak_dbtp,
        ],
    )?;
    Ok(stored > 0)
}

/// Count a failed measurement and remember why, unless the video has been
/// downloaded again since.
pub fn record_loudness_failure(
    conn: &Connection,
    pending: &PendingLoudness,
    error: &str,
) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO loudness \
               (platform, video_id, download_folder, failures, last_attempt_at, last_error) \
             SELECT ?1, ?2, ?3, 1, ?5, ?6 WHERE {SAME_DOWNLOAD} \
             ON CONFLICT (platform, video_id) DO UPDATE SET \
               failures = CASE WHEN download_folder = excluded.download_folder \
                 THEN failures + 1 ELSE 1 END, \
               download_folder = excluded.download_folder, \
               last_attempt_at = excluded.last_attempt_at, last_error = excluded.last_error, \
               measured_at = NULL, integrated_lufs = NULL, true_peak_dbtp = NULL"
        ),
        params![
            pending.platform,
            pending.video_id,
            pending.folder,
            pending.downloaded_at,
            now(),
            error,
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::*;
    use crate::db::*;

    fn pending_ids(conn: &Connection) -> Vec<String> {
        pending_loudness(conn, 10)
            .unwrap()
            .into_iter()
            .map(|p| p.video_id)
            .collect()
    }

    const LOUD: Loudness = Loudness {
        integrated_lufs: Some(-14.0),
        true_peak_dbtp: Some(-1.0),
    };

    #[test]
    fn downloads_are_measured_newest_first_once() {
        let mut conn = memory_db();
        let photo = NewVideo {
            kind: "photo",
            ..new_video("photo", true)
        };
        insert_new(
            &mut conn,
            "tiktok",
            &[
                new_video("not yet", true),
                photo,
                new_video("new", true),
                new_video("old", true),
            ],
        )
        .unwrap();
        for id in ["old", "new", "photo"] {
            download(&mut conn, "tiktok", id);
        }
        // Photo posts have sound too; videos not downloaded have nothing.
        assert_eq!(pending_ids(&conn), ["photo", "new", "old"]);

        let first = pending_loudness(&conn, 1).unwrap().remove(0);
        assert_eq!(first.folder, "tiktok/photo");
        assert!(record_loudness(&conn, &first, LOUD).unwrap());
        assert_eq!(pending_ids(&conn), ["new", "old"]);

        // A post with no sound is measured too, and not offered again.
        let silent = pending_loudness(&conn, 1).unwrap().remove(0);
        let nothing = Loudness {
            integrated_lufs: None,
            true_peak_dbtp: None,
        };
        assert!(record_loudness(&conn, &silent, nothing).unwrap());
        assert_eq!(pending_ids(&conn), ["old"]);
    }

    #[test]
    fn a_new_download_is_measured_again() {
        let mut conn = memory_db();
        insert_new(&mut conn, "youtube", &[new_video("a", true)]).unwrap();
        download(&mut conn, "youtube", "a");
        conn.execute(
            "UPDATE downloads SET downloaded_at = downloaded_at - 10",
            [],
        )
        .unwrap();
        let measured = pending_loudness(&conn, 1).unwrap().remove(0);
        // Downloaded again while it was measured: the old files' loudness is
        // dropped, and the new download waits its turn.
        download(&mut conn, "youtube", "a");
        assert!(!record_loudness(&conn, &measured, LOUD).unwrap());
        assert_eq!(pending_ids(&conn), ["a"]);

        let measured = pending_loudness(&conn, 1).unwrap().remove(0);
        assert!(record_loudness(&conn, &measured, LOUD).unwrap());
        assert!(pending_ids(&conn).is_empty());
        conn.execute(
            "UPDATE downloads SET downloaded_at = downloaded_at - 10",
            [],
        )
        .unwrap();
        download(&mut conn, "youtube", "a");
        assert_eq!(pending_ids(&conn), ["a"]);
    }

    #[test]
    fn failed_measurements_wait_before_retrying() {
        let mut conn = memory_db();
        insert_new(&mut conn, "youtube", &[new_video("a", true)]).unwrap();
        download(&mut conn, "youtube", "a");
        let pending = pending_loudness(&conn, 1).unwrap().remove(0);
        record_loudness_failure(&conn, &pending, "ffmpeg broke").unwrap();
        assert!(pending_ids(&conn).is_empty());
        conn.execute(
            "UPDATE loudness SET last_attempt_at = last_attempt_at - 3601",
            [],
        )
        .unwrap();
        assert_eq!(pending_ids(&conn), ["a"]);
        record_loudness_failure(&conn, &pending, "again").unwrap();
        let failures: i64 = conn
            .query_row("SELECT failures FROM loudness", [], |r| r.get(0))
            .unwrap();
        assert_eq!(failures, 2);
    }
}
