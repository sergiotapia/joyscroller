//! The local archive: a SQLite database of every video the user has liked,
//! across platforms. A row's presence means "liked". Rows keep what the video
//! looked like when we first saw it; only `available` is refreshed on each
//! sync, so a video that goes private later still has its title on file.
//! Every query lives in this module and the ones under it, one per concern:
//! `migrate` (the schema and its migrations), `videos`, `listings` (like
//! order, and listings under way), `syncs`, `downloads`,
//! `playback` (playback copies), `loudness` (how loud each download is),
//! `tv` (what /tv can air), `stats` (how far downloads have caught up),
//! `uploads` (the PDS account and what's been uploaded to it),
//! `list` (the UI's pages and sorts) and `search` (two FTS5 indexes over
//! each video's text, queried with what `query.rs` parses). Callers use what's re-exported here.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::Connection;

mod downloads;
mod list;
mod listings;
mod loudness;
mod migrate;
mod playback;
mod search;
mod stats;
mod syncs;
#[cfg(test)]
mod test_support;
mod tv;
mod uploads;
mod videos;

pub use downloads::{
    DownloadState, Downloaded, PendingDownload, download_item, download_state, pending_count,
    pending_downloads, record_download, record_download_failure,
};
pub use list::{Dir, Filter, Order, Sort, videos_count, videos_page};
pub use listings::{Listing, finish_listing, open_listing, store_page};
pub use loudness::{
    Loudness, PendingLoudness, pending_loudness, record_loudness, record_loudness_failure,
};
pub use migrate::init;
use migrate::{back_up_before_migrating, refuse_newer};
pub use playback::{
    MadePlayback, PendingPlayback, PlaybackState, forget_playback, pending_playback,
    pending_playback_count, playback_item, playback_state, record_playback,
    record_playback_failure,
};
pub use stats::{CatchUp, catch_up};
pub use syncs::{finish_sync, full_sync_done, full_sync_due, start_sync};
pub use tv::{Airable, airable};
pub use uploads::{
    PdsAccount, PendingUpload, UploadProgress, UploadState, UploadedPost, forget_pds_account,
    pds_account, pending_upload_count, pending_uploads, record_too_big, record_upload,
    record_upload_failure, set_pds_account, set_pds_paused, set_pds_platforms, upload_progress,
    upload_state,
};
pub use videos::{
    NewVideo, StoredVideo, VideoCount, insert_new, last_rowid, video, video_counts, video_ids,
    videos_after,
};

/// Open the database at `path`, creating it if needed, ready to use: brought
/// up to date, after a backup if it needed updating.
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
    refuse_newer(&conn)?;
    back_up_before_migrating(&conn, path)?;
    init(&mut conn)?;
    Ok(conn)
}

/// Unix seconds. A clock set before 1970 reads as 0.
pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed())
}
