//! The local archive: a SQLite database of every video the user has liked,
//! across platforms. A row's presence means "liked". Rows keep what the video
//! looked like when we first saw it; only `available` is refreshed on each
//! sync, so a video that goes private later still has its title on file.
//! Every query lives in this module and the ones under it, one per concern:
//! `migrate` (the schema and its migrations), `videos`, `syncs`, `downloads`,
//! `playback` (playback copies), `list` (the UI's pages and sorts) and
//! `search` (two FTS5 indexes over each video's text, queried with what
//! `query.rs` parses). Callers use what's re-exported here.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::Connection;

mod downloads;
mod list;
mod migrate;
mod playback;
mod search;
mod syncs;
#[cfg(test)]
mod test_support;
mod videos;

pub use downloads::{
    DownloadState, Downloaded, PendingDownload, download_item, download_state, pending_count,
    pending_downloads, record_download, record_download_failure,
};
pub use list::{Dir, Order, Sort, videos_count, videos_page};
pub use migrate::init;
use migrate::{back_up_before_migrating, refuse_newer};
pub use playback::{
    MadePlayback, PendingPlayback, PlaybackState, forget_playback, pending_playback,
    pending_playback_count, playback_item, playback_state, record_playback,
    record_playback_failure,
};
pub use syncs::{finish_sync, full_sync_done, full_sync_due, start_sync};
pub use videos::{
    NewVideo, StoredVideo, insert_new, last_rowid, video, video_counts, video_ids, videos_after,
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
