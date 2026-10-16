//! What /tv can put on air (`tv.rs`): every downloaded post, with what the
//! schedule needs to know about it.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// A downloaded post, as the TV schedule sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct Airable {
    pub platform: String,
    pub video_id: String,
    /// `video` or `photo`.
    pub kind: String,
    pub author_name: Option<String>,
    /// The download's file names.
    pub files: Vec<String>,
    /// The media's length in seconds, when known: yt-dlp's, or the list's.
    pub duration_seconds: Option<f64>,
    /// The download's measured loudness, once measured.
    pub integrated_lufs: Option<f64>,
    pub true_peak_dbtp: Option<f64>,
}

/// Every downloaded post, in no particular order. The platform said some
/// are gone since, but their files are still here, so they air too.
pub fn airable(conn: &Connection) -> Result<Vec<Airable>> {
    let mut stmt = conn.prepare(
        "SELECT v.platform, v.video_id, v.kind, v.author_name, d.files_json, \
           COALESCE(json_extract(d.metadata_json, '$.duration'), v.duration_seconds), \
           l.integrated_lufs, l.true_peak_dbtp \
         FROM downloads d \
         JOIN videos v ON v.platform = d.platform AND v.video_id = d.video_id \
         LEFT JOIN loudness l ON l.platform = d.platform AND l.video_id = d.video_id \
           AND l.download_folder = d.folder \
         WHERE d.downloaded_at IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                Airable {
                    platform: r.get(0)?,
                    video_id: r.get(1)?,
                    kind: r.get(2)?,
                    author_name: r.get(3)?,
                    files: Vec::new(),
                    duration_seconds: r.get(5)?,
                    integrated_lufs: r.get(6)?,
                    true_peak_dbtp: r.get(7)?,
                },
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(airable, files)| {
            let files = serde_json::from_str(&files).with_context(|| {
                format!(
                    "reading the file list of {} video {}",
                    airable.platform, airable.video_id
                )
            })?;
            Ok(Airable { files, ..airable })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::*;
    use crate::db::*;

    #[test]
    fn only_downloads_air_with_their_loudness() {
        let mut conn = memory_db();
        insert_new(
            &mut conn,
            "youtube",
            &[new_video("a", true), new_video("b", true)],
        )
        .unwrap();
        download(&mut conn, "youtube", "a");
        conn.execute(
            "UPDATE downloads SET metadata_json = '{\"duration\": 61.5}'",
            [],
        )
        .unwrap();
        let mut found = airable(&conn).unwrap();
        assert_eq!(found.len(), 1);
        let a = found.remove(0);
        assert_eq!(a.video_id, "a");
        assert_eq!(a.files, ["thumbnail.jpg", "video.webm"]);
        assert_eq!(a.duration_seconds, Some(61.5));
        assert_eq!(a.integrated_lufs, None);

        let pending = pending_loudness(&conn, 1).unwrap().remove(0);
        let found = Loudness {
            integrated_lufs: Some(-20.0),
            true_peak_dbtp: Some(-3.0),
        };
        record_loudness(&conn, &pending, found).unwrap();
        // Without yt-dlp's length, the list's.
        conn.execute("UPDATE downloads SET metadata_json = NULL", [])
            .unwrap();
        let a = airable(&conn).unwrap().remove(0);
        assert_eq!(a.integrated_lufs, Some(-20.0));
        assert_eq!(a.true_peak_dbtp, Some(-3.0));
        assert_eq!(a.duration_seconds, Some(60.0));
    }
}
