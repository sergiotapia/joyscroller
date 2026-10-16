//! How far the archive has caught up with the likes, per platform: how many
//! are stored, downloaded and given a playback copy, for the stats page.

use anyhow::Result;
use rusqlite::Connection;

/// One platform's progress through its likes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CatchUp {
    pub platform: String,
    /// Every like stored.
    pub liked: usize,
    /// Not downloaded, and never will be: the platform says it's gone.
    pub gone: usize,
    pub downloaded: usize,
    /// Not downloaded yet, with a failed try behind it, so waiting to retry.
    pub failing: usize,
    /// Downloaded videos (photo posts have nothing to convert), each checked
    /// in turn for whether it needs a playback copy.
    pub videos: usize,
    /// Of `videos`, those checked, with a copy made if one was needed.
    pub checked: usize,
    /// Of `checked`, those that needed a copy.
    pub copies: usize,
    /// Of `videos`, those not checked yet, with a failed try behind them.
    pub copy_failing: usize,
}

impl CatchUp {
    /// The likes that can still be downloaded, downloaded or not.
    pub fn downloadable(&self) -> usize {
        self.liked - self.gone
    }
}

/// Every platform with likes stored, by name. A playback copy counts only
/// while it's of the video's current download, as the player serves it.
pub fn catch_up(conn: &Connection) -> Result<Vec<CatchUp>> {
    let mut stmt = conn.prepare(
        "SELECT v.platform, count(*), \
           count(*) FILTER (WHERE d.downloaded_at IS NULL \
             AND (v.available = 0 OR d.unavailable_at IS NOT NULL)), \
           count(*) FILTER (WHERE d.downloaded_at IS NOT NULL), \
           count(*) FILTER (WHERE d.downloaded_at IS NULL AND d.failures > 0 \
             AND v.available = 1 AND d.unavailable_at IS NULL), \
           count(*) FILTER (WHERE d.downloaded_at IS NOT NULL AND v.kind = 'video'), \
           count(*) FILTER (WHERE d.downloaded_at IS NOT NULL AND v.kind = 'video' \
             AND p.finished_at IS NOT NULL AND p.download_folder = d.folder), \
           count(*) FILTER (WHERE d.downloaded_at IS NOT NULL AND v.kind = 'video' \
             AND p.finished_at IS NOT NULL AND p.download_folder = d.folder \
             AND p.file_name IS NOT NULL), \
           count(*) FILTER (WHERE d.downloaded_at IS NOT NULL AND v.kind = 'video' \
             AND p.finished_at IS NULL AND p.failures > 0) \
         FROM videos v \
         LEFT JOIN downloads d ON d.platform = v.platform AND d.video_id = v.video_id \
         LEFT JOIN playback_copies p ON p.platform = v.platform AND p.video_id = v.video_id \
         GROUP BY v.platform ORDER BY v.platform",
    )?;
    let rows = stmt
        .query_map([], |r| {
            let count = |i| -> rusqlite::Result<usize> {
                let n: i64 = r.get(i)?;
                usize::try_from(n).map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        i,
                        rusqlite::types::Type::Integer,
                        e.into(),
                    )
                })
            };
            Ok(CatchUp {
                platform: r.get(0)?,
                liked: count(1)?,
                gone: count(2)?,
                downloaded: count(3)?,
                failing: count(4)?,
                videos: count(5)?,
                checked: count(6)?,
                copies: count(7)?,
                copy_failing: count(8)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::*;
    use crate::db::*;

    #[test]
    fn counts_each_step_per_platform() {
        let mut conn = memory_db();
        let photo = NewVideo {
            kind: "photo",
            ..new_video("photo", true)
        };
        insert_new(
            &mut conn,
            "tiktok",
            &[
                new_video("copied", true),
                new_video("plays", true),
                new_video("waiting", true),
                new_video("broken", true),
                photo,
                new_video("failing", true),
                new_video("gone", true),
                new_video("queued", true),
            ],
        )
        .unwrap();
        insert_new(
            &mut conn,
            "youtube",
            &[new_video("private", false), new_video("kept", false)],
        )
        .unwrap();
        for id in ["copied", "plays", "waiting", "broken", "photo"] {
            download(&mut conn, "tiktok", id);
        }
        // A video downloaded before it went private still counts as downloaded.
        download(&mut conn, "youtube", "kept");
        record_download_failure(&conn, "tiktok", "failing", "outage", false).unwrap();
        record_download_failure(&conn, "tiktok", "gone", "removed", true).unwrap();
        for (id, file_name) in [("copied", Some("playback.mp4")), ("plays", None)] {
            let state = download_state(&conn, "tiktok", id).unwrap().unwrap();
            let made = MadePlayback {
                download_folder: &format!("tiktok/{id}"),
                downloaded_at: state.downloaded_at.unwrap(),
                file_name,
                size_bytes: 1,
                method: if file_name.is_some() {
                    "encode"
                } else {
                    "none"
                },
                original_config_json: None,
            };
            assert!(record_playback(&conn, "tiktok", id, &made).unwrap());
        }
        let state = download_state(&conn, "tiktok", "broken").unwrap().unwrap();
        record_playback_failure(
            &conn,
            "tiktok",
            "broken",
            ("tiktok/broken", state.downloaded_at.unwrap()),
            "ffmpeg failed",
        )
        .unwrap();

        assert_eq!(
            catch_up(&conn).unwrap(),
            [
                CatchUp {
                    platform: "tiktok".into(),
                    liked: 8,
                    gone: 1,
                    downloaded: 5,
                    failing: 1,
                    videos: 4,
                    checked: 2,
                    copies: 1,
                    copy_failing: 1,
                },
                CatchUp {
                    platform: "youtube".into(),
                    liked: 2,
                    gone: 1,
                    downloaded: 1,
                    videos: 1,
                    ..CatchUp::default()
                },
            ]
        );
    }
}
