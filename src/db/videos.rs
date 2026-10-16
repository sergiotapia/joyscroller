//! Liked videos: storing what a sync finds (insert-only, each with its place
//! in like order, `like_order`), and reading them back for the UI.

use std::collections::HashSet;

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, params};

use super::now;

/// A liked video, in the platform-neutral shape the `videos` table stores.
pub struct NewVideo<'a> {
    pub video_id: &'a str,
    pub url: &'a str,
    pub title: Option<&'a str>,
    pub author_id: Option<&'a str>,
    pub author_name: Option<&'a str>,
    pub duration_seconds: Option<u64>,
    pub available: bool,
    pub listing_json: &'a str,
    /// `video`, or `photo` for a TikTok slideshow, an Instagram photo or
    /// carousel, or an X post with pictures or more than one video.
    pub kind: &'a str,
    pub posted_at: Option<i64>,
}

/// Every video id already stored for `platform`.
pub fn video_ids(conn: &Connection, platform: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare("SELECT video_id FROM videos WHERE platform = ?1")?;
    let ids = stmt
        .query_map([platform], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(ids)
}

/// Insert the videos we haven't seen before, a whole listing at once, as
/// listed: newest first. They take a band of like order of their own (see
/// `listings.rs`). For existing rows only `available` is refreshed.
/// Returns how many were new.
pub fn insert_new(conn: &mut Connection, platform: &str, videos: &[NewVideo]) -> Result<usize> {
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let top = super::listings::new_top(&tx)?;
    let added = insert_rows(&tx, platform, videos, top)?;
    tx.commit()?;
    Ok(added)
}

/// Insert `videos` (newest first) in `tx`, the first at `like_order`
/// `first` and each after it one lower. Returns how many were new.
pub(super) fn insert_rows(
    tx: &rusqlite::Transaction,
    platform: &str,
    videos: &[NewVideo],
    first: i64,
) -> Result<usize> {
    let added_at = now();
    let mut added = 0;
    {
        let mut insert = tx.prepare(
            "INSERT OR IGNORE INTO videos \
             (platform, video_id, url, title, author_id, author_name, duration_seconds, available, listing_json, added_at, kind, posted_at, like_order) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )?;
        let mut set_available =
            tx.prepare("UPDATE videos SET available = ?1 WHERE platform = ?2 AND video_id = ?3")?;
        for (v, like_order) in videos.iter().zip((0..).map(|n: i64| first - n)) {
            let inserted = insert.execute(params![
                platform,
                v.video_id,
                v.url,
                v.title,
                v.author_id,
                v.author_name,
                v.duration_seconds.map(i64::try_from).transpose()?,
                v.available,
                v.listing_json,
                added_at,
                v.kind,
                v.posted_at,
                like_order,
            ])?;
            if inserted == 0 {
                set_available.execute(params![v.available, platform, v.video_id])?;
            }
            added += inserted;
        }
    }
    Ok(added)
}

/// A stored video, as the web UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVideo {
    /// Insertion order. The UI streams rows past the last one a page has seen.
    pub rowid: i64,
    /// Its place in like order: higher is more recently liked.
    pub like_order: i64,
    pub platform: String,
    pub video_id: String,
    pub url: String,
    pub title: Option<String>,
    pub author_id: Option<String>,
    pub author_name: Option<String>,
    pub duration_seconds: Option<i64>,
    pub available: bool,
    pub kind: String,
    /// What sort of post it is, finer than `kind` (`POST_TYPE`): `video`,
    /// `slideshow` (TikTok), `photo` (Instagram, X) or `carousel` (Instagram).
    pub post_type: String,
    pub posted_at: Option<i64>,
    pub added_at: i64,
    /// Once its media is downloaded, the file in its folder to show as its
    /// picture: the thumbnail, or a photo post's first image.
    pub thumbnail: Option<String>,
}

/// What sort of post a row of `videos v` is, for the sidebar's types: a
/// `video`, or one of the kinds of `photo` post, which `kind` lumps
/// together because they play the same way: a TikTok `slideshow`, an
/// Instagram `photo` or `carousel` (its `media_type`, 1 or 8, from the list),
/// or an X post's pictures (or more than one picture or video). The sidebar
/// lists these names under each platform (`POST_TYPES` in `web.rs`), so a new
/// one goes there too.
macro_rules! post_type_sql {
    () => {
        "CASE WHEN v.kind = 'video' THEN 'video' \
           WHEN v.platform = 'instagram' AND json_extract(v.listing_json, '$.media_type') = 8 \
             THEN 'carousel' \
           WHEN v.platform IN ('instagram', 'x') THEN 'photo' \
           ELSE 'slideshow' END"
    };
}
pub(super) use post_type_sql;

/// `StoredVideo`'s columns, for `video_from_row`, selected from
/// `VIDEO_TABLES`. The file lists are sorted, so `MIN` is the first image.
pub(super) const VIDEO_COLUMNS: &str = concat!(
    "v.rowid, v.platform, v.video_id, v.url, v.title, v.author_id, \
     v.author_name, v.duration_seconds, v.available, v.kind, v.posted_at, v.added_at, \
     COALESCE(\
       (SELECT MIN(value) FROM json_each(d.files_json) WHERE value LIKE 'thumbnail.%'), \
       (SELECT MIN(value) FROM json_each(d.files_json) WHERE value LIKE 'image-%')), \
     v.like_order, ",
    post_type_sql!()
);

/// The videos, each with its download if that has finished.
pub(super) const VIDEO_TABLES: &str = "videos v LEFT JOIN downloads d ON d.platform = v.platform \
     AND d.video_id = v.video_id AND d.downloaded_at IS NOT NULL";

/// How many columns `VIDEO_COLUMNS` selects.
const VIDEO_COLUMN_COUNT: usize = 15;

pub(super) fn video_from_row(r: &rusqlite::Row) -> rusqlite::Result<StoredVideo> {
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
        added_at: r.get(11)?,
        thumbnail: r.get(12)?,
        like_order: r.get(13)?,
        post_type: r.get(VIDEO_COLUMN_COUNT - 1)?,
    })
}

/// How many videos are stored, by platform and post type.
pub fn video_counts(conn: &Connection) -> Result<Vec<VideoCount>> {
    let mut stmt = conn.prepare(concat!(
        "SELECT v.platform, ",
        post_type_sql!(),
        ", COUNT(*) FROM videos v GROUP BY 1, 2"
    ))?;
    let counts: Vec<(String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    counts
        .into_iter()
        .map(|(platform, post_type, count)| {
            Ok(VideoCount {
                platform,
                post_type,
                count: usize::try_from(count)?,
            })
        })
        .collect()
}

/// How many videos there are of one post type on one platform.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct VideoCount {
    pub platform: String,
    pub post_type: String,
    pub count: usize,
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

/// One video and its `listing_json`.
pub fn video(
    conn: &Connection,
    platform: &str,
    video_id: &str,
) -> Result<Option<(StoredVideo, String)>> {
    let found = conn
        .query_row(
            &format!(
                "SELECT {VIDEO_COLUMNS}, v.listing_json FROM {VIDEO_TABLES} \
                 WHERE v.platform = ?1 AND v.video_id = ?2"
            ),
            [platform, video_id],
            // `listing_json` comes after `VIDEO_COLUMNS`.
            |r| Ok((video_from_row(r)?, r.get(VIDEO_COLUMN_COUNT)?)),
        )
        .optional()?;
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::*;
    use crate::db::*;
    use crate::query::Query;

    #[test]
    fn inserts_only_new_videos() {
        let mut conn = memory_db();
        assert_eq!(
            insert_new(&mut conn, "youtube", &[new_video("a", true)]).unwrap(),
            1
        );
        let added = insert_new(
            &mut conn,
            "youtube",
            &[new_video("a", true), new_video("b", true)],
        )
        .unwrap();
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
        insert_new(&mut conn, "youtube", &[new_video("a", true)]).unwrap();
        insert_new(&mut conn, "youtube", &[new_video("a", false)]).unwrap();
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
    fn streams_videos_stored_after_a_rowid() {
        let mut conn = memory_db();
        assert_eq!(last_rowid(&conn).unwrap(), 0);
        insert_new(&mut conn, "youtube", &[new_video("a", true)]).unwrap();
        let seen = last_rowid(&conn).unwrap();
        insert_new(
            &mut conn,
            "youtube",
            &[new_video("b", true), new_video("c", true)],
        )
        .unwrap();
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
        insert_new(&mut conn, "youtube", &[new_video("a", true)]).unwrap();
        let (found, raw) = super::video(&conn, "youtube", "a").unwrap().unwrap();
        assert_eq!(found.title.as_deref(), Some("title"));
        assert_eq!(raw, "{}");
        assert!(super::video(&conn, "tiktok", "a").unwrap().is_none());

        assert!(download_state(&conn, "youtube", "a").unwrap().is_none());
        record_download_failure(&conn, "youtube", "a", "HTTP 429", false).unwrap();
        let state = download_state(&conn, "youtube", "a").unwrap().unwrap();
        assert_eq!(state.failures, 1);
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
            &[
                new_video("f", true),
                new_video("p", true),
                new_video("v", true),
            ],
        )
        .unwrap();
        let save = |conn: &mut Connection, id, files: &[String]| {
            let done = Downloaded {
                folder: "tiktok/x",
                files,
                size_bytes: 10,
                formats: None,
                metadata_json: None,
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
        record_download_failure(&conn, "tiktok", "f", "HTTP 403", false).unwrap();

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
}
