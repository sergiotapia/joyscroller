//! Uploads to the user's AT Protocol PDS (`pds.rs`): the connected account,
//! which downloaded posts still need uploading to it, and how each upload
//! went. Like a playback copy, an upload belongs to the download it was made
//! from: a new download is uploaded again, and an upload of an earlier
//! download isn't recorded. A post with a file over the PDS's limit is marked
//! too big and isn't tried again until the account is connected again.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use super::downloads::MAX_RETRY_DELAY;
use super::now;

/// The connected account. Its app password is in the keyring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PdsAccount {
    pub did: String,
    pub handle: String,
    /// Where it logs in, as typed.
    pub service_url: String,
    /// Where its repo lives.
    pub pds_url: String,
    /// The platforms whose posts upload.
    pub platforms: Vec<String>,
    pub blob_limit_bytes: Option<u64>,
    /// Paused in Settings: nothing uploads until it's resumed.
    pub paused: bool,
}

/// A downloaded post still to upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpload {
    pub platform: String,
    pub video_id: String,
    pub kind: String,
    pub url: String,
    pub title: Option<String>,
    pub author_name: Option<String>,
    /// Unix seconds.
    pub posted_at: Option<i64>,
    pub added_at: i64,
    pub like_order: i64,
    /// The download's folder, relative to the media root, and its files.
    pub folder: String,
    pub files: Vec<String>,
    /// When the download was recorded, which the record functions check is
    /// still the one that was uploaded.
    pub downloaded_at: i64,
    /// The download's size, all its files.
    pub size_bytes: u64,
}

/// What an upload left on the PDS.
pub struct UploadedPost<'a> {
    pub record_uri: &'a str,
    pub record_cid: &'a str,
    pub blobs_json: &'a str,
    pub size_bytes: u64,
}

/// Where a post's upload to the connected account stands, if tried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadState {
    pub failures: i64,
    pub last_error: Option<String>,
    pub uploaded_at: Option<i64>,
    pub too_big_at: Option<i64>,
    pub record_uri: Option<String>,
    pub size_bytes: Option<i64>,
    /// Whether it's about the post's current download; an older one's
    /// upload is replaced in its turn.
    pub current: bool,
}

/// The connected account, if any.
pub fn pds_account(conn: &Connection) -> Result<Option<PdsAccount>> {
    let found = conn
        .query_row(
            "SELECT did, handle, service_url, pds_url, platforms_json, blob_limit_bytes, \
               paused_at IS NOT NULL \
             FROM pds_accounts ORDER BY added_at DESC LIMIT 1",
            [],
            |r| {
                Ok((
                    PdsAccount {
                        did: r.get(0)?,
                        handle: r.get(1)?,
                        service_url: r.get(2)?,
                        pds_url: r.get(3)?,
                        platforms: Vec::new(),
                        blob_limit_bytes: r
                            .get::<_, Option<i64>>(5)?
                            .and_then(|b| u64::try_from(b).ok()),
                        paused: r.get(6)?,
                    },
                    r.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((mut account, platforms)) = found else {
        return Ok(None);
    };
    account.platforms =
        serde_json::from_str(&platforms).context("reading the connected account's platforms")?;
    Ok(Some(account))
}

/// Connect `account`, replacing any other. Its posts once too big are tried
/// again, since its limit may have changed.
pub fn set_pds_account(conn: &Connection, account: &PdsAccount) -> Result<()> {
    // Immediate: a deferred transaction that has to wait for another
    // connection's write fails instead of waiting.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    tx.execute("DELETE FROM pds_accounts", [])?;
    tx.execute(
        "INSERT INTO pds_accounts \
           (did, handle, service_url, pds_url, platforms_json, blob_limit_bytes, paused_at, \
            added_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, CASE WHEN ?7 THEN ?8 END, ?8)",
        params![
            account.did,
            account.handle,
            account.service_url,
            account.pds_url,
            serde_json::to_string(&account.platforms)?,
            account.blob_limit_bytes.map(i64::try_from).transpose()?,
            account.paused,
            now(),
        ],
    )?;
    tx.execute(
        "DELETE FROM pds_uploads WHERE did = ?1 AND too_big_at IS NOT NULL",
        [&account.did],
    )?;
    tx.commit()?;
    Ok(())
}

/// Change which platforms' posts upload.
pub fn set_pds_platforms(conn: &Connection, platforms: &[&str]) -> Result<()> {
    conn.execute(
        "UPDATE pds_accounts SET platforms_json = ?1",
        [serde_json::to_string(platforms)?],
    )?;
    Ok(())
}

/// Pause uploading, or resume it.
pub fn set_pds_paused(conn: &Connection, paused: bool) -> Result<()> {
    conn.execute(
        "UPDATE pds_accounts SET paused_at = CASE WHEN ?1 THEN ?2 END",
        params![paused, now()],
    )?;
    Ok(())
}

/// Disconnect the account. What was uploaded stays on record, for when
/// it's connected again.
pub fn forget_pds_account(conn: &Connection) -> Result<()> {
    conn.execute("DELETE FROM pds_accounts", [])?;
    Ok(())
}

/// Downloaded posts on the account's platforms not uploaded to it yet (or
/// uploaded from a folder since replaced), not too big and not waiting out a
/// retry delay. `?1` is `MAX_RETRY_DELAY`, `?2` now and `?3` a platform or
/// NULL for all; failures back off as downloads do.
const UPLOAD_WHERE: &str = "d.downloaded_at IS NOT NULL \
     AND EXISTS (SELECT 1 FROM json_each(a.platforms_json) WHERE value = v.platform) \
     AND (?3 IS NULL OR v.platform = ?3) \
     AND (u.platform IS NULL OR u.download_folder != d.folder \
          OR (u.uploaded_at IS NULL AND u.too_big_at IS NULL \
              AND (u.failures = 0 \
                   OR u.last_attempt_at + MIN(3600 << MIN(u.failures - 1, 20), ?1) <= ?2)))";

const UPLOAD_TABLES: &str = "videos v \
     JOIN downloads d ON d.platform = v.platform AND d.video_id = v.video_id \
     JOIN pds_accounts a \
     LEFT JOIN pds_uploads u \
       ON u.platform = v.platform AND u.video_id = v.video_id AND u.did = a.did";

/// Up to `limit` posts to upload to the connected account, on `platform` or
/// all of them, most recently liked first, as the downloads go.
pub fn pending_uploads(
    conn: &Connection,
    platform: Option<&str>,
    limit: usize,
) -> Result<Vec<PendingUpload>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT v.platform, v.video_id, v.kind, v.url, v.title, v.author_name, v.posted_at, \
           v.added_at, v.like_order, d.folder, d.files_json, d.downloaded_at, d.size_bytes \
         FROM {UPLOAD_TABLES} WHERE {UPLOAD_WHERE} \
         ORDER BY v.like_order DESC, v.rowid DESC LIMIT ?4"
    ))?;
    let rows = stmt
        .query_map(
            params![MAX_RETRY_DELAY, now(), platform, i64::try_from(limit)?],
            |r| {
                Ok((
                    PendingUpload {
                        platform: r.get(0)?,
                        video_id: r.get(1)?,
                        kind: r.get(2)?,
                        url: r.get(3)?,
                        title: r.get(4)?,
                        author_name: r.get(5)?,
                        posted_at: r.get(6)?,
                        added_at: r.get(7)?,
                        like_order: r.get(8)?,
                        folder: r.get(9)?,
                        files: Vec::new(),
                        downloaded_at: r.get(11)?,
                        size_bytes: r
                            .get::<_, Option<i64>>(12)?
                            .and_then(|b| u64::try_from(b).ok())
                            .unwrap_or(0),
                    },
                    r.get::<_, String>(10)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    rows.into_iter()
        .map(|(mut pending, files)| {
            pending.files = serde_json::from_str(&files).with_context(|| {
                format!(
                    "reading the file list of {} post {}",
                    pending.platform, pending.video_id
                )
            })?;
            Ok(pending)
        })
        .collect()
}

/// How many posts `pending_uploads` would offer.
pub fn pending_upload_count(conn: &Connection) -> Result<usize> {
    let count: i64 = conn.query_row(
        &format!("SELECT count(*) FROM {UPLOAD_TABLES} WHERE {UPLOAD_WHERE}"),
        params![MAX_RETRY_DELAY, now(), Option::<&str>::None],
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count)?)
}

/// How far the connected account's uploads have got, over the current
/// downloads of the platforms it uploads: what's uploaded, what's left
/// (waiting out a retry delay included), and what's too big for its server.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UploadProgress {
    pub uploaded_posts: usize,
    pub uploaded_files: usize,
    pub left_posts: usize,
    pub left_files: usize,
    pub too_big_posts: usize,
}

pub fn upload_progress(conn: &Connection) -> Result<UploadProgress> {
    let counts: [i64; 5] = conn.query_row(
        "WITH posts AS ( \
           SELECT json_array_length(d.files_json) AS files, \
             u.download_folder = d.folder AND u.uploaded_at IS NOT NULL AS uploaded, \
             u.download_folder = d.folder AND u.too_big_at IS NOT NULL AS too_big \
           FROM videos v \
           JOIN downloads d ON d.platform = v.platform AND d.video_id = v.video_id \
           JOIN pds_accounts a \
           LEFT JOIN pds_uploads u \
             ON u.platform = v.platform AND u.video_id = v.video_id AND u.did = a.did \
           WHERE d.downloaded_at IS NOT NULL \
             AND EXISTS (SELECT 1 FROM json_each(a.platforms_json) WHERE value = v.platform)) \
         SELECT count(*) FILTER (WHERE uploaded), \
           coalesce(sum(files) FILTER (WHERE uploaded), 0), \
           count(*) FILTER (WHERE NOT coalesce(uploaded OR too_big, 0)), \
           coalesce(sum(files) FILTER (WHERE NOT coalesce(uploaded OR too_big, 0)), 0), \
           count(*) FILTER (WHERE too_big) \
         FROM posts",
        [],
        |r| Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?]),
    )?;
    let [
        uploaded_posts,
        uploaded_files,
        left_posts,
        left_files,
        too_big_posts,
    ] = counts.map(|n| usize::try_from(n).unwrap_or(0));
    Ok(UploadProgress {
        uploaded_posts,
        uploaded_files,
        left_posts,
        left_files,
        too_big_posts,
    })
}

/// Whether the download in `?3` recorded at `?4` is still post `?1`/`?2`'s.
const SAME_DOWNLOAD: &str = "EXISTS (SELECT 1 FROM downloads \
     WHERE platform = ?1 AND video_id = ?2 AND folder = ?3 AND downloaded_at = ?4)";

/// Record that `pending` was uploaded to `did`, unless the post has been
/// downloaded again since. False when it was dropped for that reason.
pub fn record_upload(
    conn: &Connection,
    did: &str,
    pending: &PendingUpload,
    done: &UploadedPost,
) -> Result<bool> {
    // The WHERE also keeps SQLite from reading ON CONFLICT as a join.
    let stored = conn.execute(
        &format!(
            "INSERT INTO pds_uploads \
               (platform, video_id, download_folder, did, last_attempt_at, uploaded_at, \
                record_uri, record_cid, blobs_json, size_bytes) \
             SELECT ?1, ?2, ?3, ?5, ?6, ?6, ?7, ?8, ?9, ?10 WHERE {SAME_DOWNLOAD} \
             ON CONFLICT (platform, video_id, did) DO UPDATE SET \
               download_folder = excluded.download_folder, failures = 0, \
               last_attempt_at = excluded.last_attempt_at, last_error = NULL, \
               uploaded_at = excluded.uploaded_at, too_big_at = NULL, \
               record_uri = excluded.record_uri, record_cid = excluded.record_cid, \
               blobs_json = excluded.blobs_json, size_bytes = excluded.size_bytes"
        ),
        params![
            pending.platform,
            pending.video_id,
            pending.folder,
            pending.downloaded_at,
            did,
            now(),
            done.record_uri,
            done.record_cid,
            done.blobs_json,
            i64::try_from(done.size_bytes)?,
        ],
    )?;
    Ok(stored > 0)
}

/// Record that `pending` has a file too big for `did`'s PDS, so it isn't
/// tried again, unless the post has been downloaded again since.
pub fn record_too_big(
    conn: &Connection,
    did: &str,
    pending: &PendingUpload,
    error: &str,
) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO pds_uploads \
               (platform, video_id, download_folder, did, last_attempt_at, last_error, \
                too_big_at, size_bytes) \
             SELECT ?1, ?2, ?3, ?5, ?6, ?7, ?6, ?8 WHERE {SAME_DOWNLOAD} \
             ON CONFLICT (platform, video_id, did) DO UPDATE SET \
               download_folder = excluded.download_folder, failures = 0, \
               last_attempt_at = excluded.last_attempt_at, last_error = excluded.last_error, \
               uploaded_at = NULL, too_big_at = excluded.too_big_at, record_uri = NULL, \
               record_cid = NULL, blobs_json = NULL, size_bytes = excluded.size_bytes"
        ),
        params![
            pending.platform,
            pending.video_id,
            pending.folder,
            pending.downloaded_at,
            did,
            now(),
            error,
            i64::try_from(pending.size_bytes)?,
        ],
    )?;
    Ok(())
}

/// Count a failed upload and remember why, unless the post has been
/// downloaded again since. A post uploaded from an earlier download keeps
/// that record on file until this one replaces it.
pub fn record_upload_failure(
    conn: &Connection,
    did: &str,
    pending: &PendingUpload,
    error: &str,
) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO pds_uploads \
               (platform, video_id, download_folder, did, failures, last_attempt_at, last_error) \
             SELECT ?1, ?2, ?3, ?5, 1, ?6, ?7 WHERE {SAME_DOWNLOAD} \
             ON CONFLICT (platform, video_id, did) DO UPDATE SET \
               failures = CASE WHEN download_folder = excluded.download_folder \
                 THEN failures + 1 ELSE 1 END, \
               download_folder = excluded.download_folder, \
               last_attempt_at = excluded.last_attempt_at, last_error = excluded.last_error, \
               uploaded_at = NULL, too_big_at = NULL"
        ),
        params![
            pending.platform,
            pending.video_id,
            pending.folder,
            pending.downloaded_at,
            did,
            now(),
            error,
        ],
    )?;
    Ok(())
}

/// Where `platform`/`video_id`'s upload to the connected account stands.
pub fn upload_state(
    conn: &Connection,
    platform: &str,
    video_id: &str,
) -> Result<Option<UploadState>> {
    let found = conn
        .query_row(
            "SELECT u.failures, u.last_error, u.uploaded_at, u.too_big_at, u.record_uri, \
               u.size_bytes, d.folder IS NOT NULL AND d.folder = u.download_folder \
             FROM pds_uploads u \
             JOIN pds_accounts a ON a.did = u.did \
             LEFT JOIN downloads d ON d.platform = u.platform AND d.video_id = u.video_id \
             WHERE u.platform = ?1 AND u.video_id = ?2",
            [platform, video_id],
            |r| {
                Ok(UploadState {
                    failures: r.get(0)?,
                    last_error: r.get(1)?,
                    uploaded_at: r.get(2)?,
                    too_big_at: r.get(3)?,
                    record_uri: r.get(4)?,
                    size_bytes: r.get(5)?,
                    current: r.get(6)?,
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

    fn account(platforms: &[&str]) -> PdsAccount {
        PdsAccount {
            did: "did:plc:me".into(),
            handle: "me.test".into(),
            service_url: "https://pds.test".into(),
            pds_url: "https://pds.test".into(),
            platforms: platforms.iter().map(ToString::to_string).collect(),
            blob_limit_bytes: Some(100),
            paused: false,
        }
    }

    fn pending_ids(conn: &Connection) -> Vec<String> {
        pending_uploads(conn, None, 10)
            .unwrap()
            .into_iter()
            .map(|p| p.video_id)
            .collect()
    }

    const DONE: UploadedPost = UploadedPost {
        record_uri: "at://did:plc:me/c/r",
        record_cid: "bafy",
        blobs_json: "[]",
        size_bytes: 10,
    };

    #[test]
    fn nothing_uploads_without_an_account() {
        let mut conn = memory_db();
        insert_new(&mut conn, "tiktok", &[new_video("a", true)]).unwrap();
        download(&mut conn, "tiktok", "a");
        assert_eq!(pending_ids(&conn), Vec::<String>::new());
        assert_eq!(pds_account(&conn).unwrap(), None);
    }

    #[test]
    fn downloads_on_the_chosen_platforms_upload_newest_first_once() {
        let mut conn = memory_db();
        insert_new(
            &mut conn,
            "tiktok",
            &[
                new_video("not yet", true),
                new_video("new", true),
                new_video("old", true),
            ],
        )
        .unwrap();
        insert_new(&mut conn, "youtube", &[new_video("yt", true)]).unwrap();
        for (platform, id) in [("tiktok", "old"), ("tiktok", "new"), ("youtube", "yt")] {
            download(&mut conn, platform, id);
        }
        set_pds_account(&conn, &account(&["tiktok"])).unwrap();
        assert_eq!(pds_account(&conn).unwrap(), Some(account(&["tiktok"])));
        assert_eq!(pending_ids(&conn), ["new", "old"]);
        assert_eq!(pending_upload_count(&conn).unwrap(), 2);

        let first = pending_uploads(&conn, None, 1).unwrap().remove(0);
        assert_eq!(first.folder, "tiktok/new");
        assert_eq!(first.files, ["thumbnail.jpg", "video.webm"]);
        assert!(record_upload(&conn, "did:plc:me", &first, &DONE).unwrap());
        assert_eq!(pending_ids(&conn), ["old"]);
        // Each download has a thumbnail and a video.
        assert_eq!(
            upload_progress(&conn).unwrap(),
            UploadProgress {
                uploaded_posts: 1,
                uploaded_files: 2,
                left_posts: 1,
                left_files: 2,
                too_big_posts: 0,
            }
        );
        let state = upload_state(&conn, "tiktok", "new").unwrap().unwrap();
        assert_eq!(state.record_uri.as_deref(), Some(DONE.record_uri));
        assert!(state.current);

        set_pds_platforms(&conn, &["tiktok", "youtube"]).unwrap();
        assert_eq!(pending_ids(&conn), ["yt", "old"]);
        assert_eq!(pending_uploads(&conn, Some("youtube"), 5).unwrap().len(), 1);
    }

    #[test]
    fn a_new_download_uploads_again() {
        let mut conn = memory_db();
        insert_new(&mut conn, "youtube", &[new_video("a", true)]).unwrap();
        download(&mut conn, "youtube", "a");
        set_pds_account(&conn, &account(&["youtube"])).unwrap();
        conn.execute(
            "UPDATE downloads SET downloaded_at = downloaded_at - 10",
            [],
        )
        .unwrap();
        let uploading = pending_uploads(&conn, None, 1).unwrap().remove(0);
        // Downloaded again while it uploaded: the old files' upload isn't
        // recorded, and the new download waits its turn.
        download(&mut conn, "youtube", "a");
        assert!(!record_upload(&conn, "did:plc:me", &uploading, &DONE).unwrap());
        assert_eq!(pending_ids(&conn), ["a"]);
        let uploading = pending_uploads(&conn, None, 1).unwrap().remove(0);
        assert!(record_upload(&conn, "did:plc:me", &uploading, &DONE).unwrap());
        assert_eq!(pending_ids(&conn), Vec::<String>::new());
    }

    #[test]
    fn failed_uploads_wait_and_too_big_ones_wait_for_a_new_limit() {
        let mut conn = memory_db();
        insert_new(
            &mut conn,
            "tiktok",
            &[new_video("big", true), new_video("a", true)],
        )
        .unwrap();
        download(&mut conn, "tiktok", "a");
        download(&mut conn, "tiktok", "big");
        set_pds_account(&conn, &account(&["tiktok"])).unwrap();
        let [big, a] =
            <[PendingUpload; 2]>::try_from(pending_uploads(&conn, None, 2).unwrap()).unwrap();

        record_upload_failure(&conn, "did:plc:me", &a, "offline").unwrap();
        record_too_big(&conn, "did:plc:me", &big, "video.webm is too big").unwrap();
        assert_eq!(pending_ids(&conn), Vec::<String>::new());
        // Waiting out a retry delay still counts as left; too big doesn't.
        let progress = upload_progress(&conn).unwrap();
        assert_eq!((progress.left_posts, progress.too_big_posts), (1, 1));
        conn.execute(
            "UPDATE pds_uploads SET last_attempt_at = last_attempt_at - 3601",
            [],
        )
        .unwrap();
        assert_eq!(pending_ids(&conn), ["a"]);
        record_upload_failure(&conn, "did:plc:me", &a, "again").unwrap();
        let state = upload_state(&conn, "tiktok", "a").unwrap().unwrap();
        assert_eq!(
            (state.failures, state.last_error.as_deref()),
            (2, Some("again"))
        );

        // Connecting again (with perhaps a new limit) tries it again.
        set_pds_account(&conn, &account(&["tiktok"])).unwrap();
        assert_eq!(pending_ids(&conn), ["big"]);
    }

    #[test]
    fn pausing_is_kept_on_the_account() {
        let conn = memory_db();
        set_pds_account(&conn, &account(&["tiktok"])).unwrap();
        set_pds_paused(&conn, true).unwrap();
        assert!(pds_account(&conn).unwrap().unwrap().paused);
        set_pds_paused(&conn, false).unwrap();
        assert!(!pds_account(&conn).unwrap().unwrap().paused);
    }

    #[test]
    fn disconnecting_keeps_what_was_uploaded() {
        let mut conn = memory_db();
        insert_new(&mut conn, "tiktok", &[new_video("a", true)]).unwrap();
        download(&mut conn, "tiktok", "a");
        set_pds_account(&conn, &account(&["tiktok"])).unwrap();
        let a = pending_uploads(&conn, None, 1).unwrap().remove(0);
        record_upload(&conn, "did:plc:me", &a, &DONE).unwrap();
        forget_pds_account(&conn).unwrap();
        assert_eq!(upload_state(&conn, "tiktok", "a").unwrap(), None);
        set_pds_account(&conn, &account(&["tiktok"])).unwrap();
        assert_eq!(pending_ids(&conn), Vec::<String>::new());
        assert!(upload_state(&conn, "tiktok", "a").unwrap().is_some());
    }
}
