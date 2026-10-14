//! The log of sync runs: when each started and finished, and how many new
//! likes it found.

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
}
