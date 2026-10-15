//! The log of sync runs: when each started and finished, and how many new
//! likes it found. Also the lists whose next sync is due to read them in
//! full (`full_syncs_due`).

use anyhow::Result;
use rusqlite::{Connection, params};

use super::now;

/// Record the start of a sync; returns its id for `finish_sync`.
pub fn start_sync(conn: &Connection, platform: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO syncs (platform, started_at) VALUES (?1, ?2)",
        params![platform, now()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn finish_sync(conn: &Connection, sync_id: i64, added_count: usize) -> Result<()> {
    conn.execute(
        "UPDATE syncs SET finished_at = ?1, added_count = ?2 WHERE id = ?3",
        params![now(), i64::try_from(added_count)?, sync_id],
    )?;
    Ok(())
}

/// Whether the next sync should read `list` in full, past the posts already
/// stored. A new list, or one an update found posts in that it used to skip,
/// can't stop at stored posts until it has been read through once.
pub fn full_sync_due(conn: &Connection, list: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM full_syncs_due WHERE list = ?1)",
        [list],
        |r| r.get(0),
    )?)
}

/// Record that `list` has been read through in full.
pub fn full_sync_done(conn: &Connection, list: &str) -> Result<()> {
    conn.execute("DELETE FROM full_syncs_due WHERE list = ?1", [list])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::*;

    #[test]
    fn records_sync_runs() {
        let conn = memory_db();
        let id = start_sync(&conn, "youtube").unwrap();
        finish_sync(&conn, id, 3).unwrap();
        let added: i64 = conn
            .query_row("SELECT added_count FROM syncs WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(added, 3);
    }

    #[test]
    fn instagram_lists_are_read_in_full_once() {
        // A library with Instagram saves from before photos were kept reads
        // its saves again.
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::migrate::SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO videos (platform, video_id, url, raw_json, liked_at) \
             VALUES ('instagram', 'c', 'u', '{}', 0)",
            [],
        )
        .unwrap();
        crate::db::init(&mut conn).unwrap();
        assert!(full_sync_due(&conn, "instagram saved").unwrap());
        assert!(full_sync_due(&conn, "instagram liked").unwrap());
        full_sync_done(&conn, "instagram saved").unwrap();
        assert!(!full_sync_due(&conn, "instagram saved").unwrap());

        // A new library has no saves to read again, but every library reads
        // its likes in full the first time.
        let conn = memory_db();
        assert!(!full_sync_due(&conn, "instagram saved").unwrap());
        assert!(full_sync_due(&conn, "instagram liked").unwrap());
    }
}
