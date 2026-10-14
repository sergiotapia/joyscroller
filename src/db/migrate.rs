//! Brings a database's tables up to date. The schema and every change since
//! are SQL files in `migrations/`, applied in order: `PRAGMA user_version`
//! counts the steps a database has had, and `init` applies the rest, each in
//! a transaction. Before changing an existing database, `back_up_before_migrating`
//! saves a copy of it; a database from a newer Joyscroller is refused.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;

/// The original schema. Never edit it, nor a step that has shipped: changes
/// are new steps in `MIGRATIONS`, so that new databases and old ones end up in
/// the same shape by the same steps.
pub(super) const SCHEMA: &str = include_str!("migrations/0000_schema.sql");

/// Schema changes since `SCHEMA`, in order. A database's `user_version` says
/// how many it has had. Append new steps; never edit or reorder applied ones.
pub(super) const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_kinds_and_downloads.sql"),
    include_str!("migrations/0002_search.sql"),
    include_str!("migrations/0003_playback_copies.sql"),
    include_str!("migrations/0004_clearer_names.sql"),
];

/// Before `init` changes an existing database, save a copy of it as it is,
/// in `backups/` beside it, e.g. `backups/joyscroller-2026-10-14-153000-v2.db`
/// for one at version 2. The migrations run in transactions and can't leave
/// a database half-changed, but one with a bug could still change it wrongly,
/// and the copy undoes that. A new database has nothing to keep.
pub(super) fn back_up_before_migrating(conn: &Connection, path: &std::path::Path) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let latest = i64::try_from(MIGRATIONS.len())?;
    let has_videos: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'videos')",
        [],
        |r| r.get(0),
    )?;
    if !has_videos || version >= latest {
        return Ok(());
    }
    let dir = path
        .parent()
        .context("the database has no folder")?
        .join("backups");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("joyscroller");
    let stamp: String = conn.query_row(
        "SELECT strftime('%Y-%m-%d-%H%M%S', 'now', 'localtime')",
        [],
        |r| r.get(0),
    )?;
    let backup = dir.join(format!("{stem}-{stamp}-v{version}.db"));
    let target = backup
        .to_str()
        .with_context(|| format!("{} isn't valid UTF-8", backup.display()))?;
    // A consistent copy, WAL included, compacted as it's written.
    conn.execute("VACUUM INTO ?1", [target])
        .with_context(|| format!("backing up the database to {}", backup.display()))?;
    eprintln!(
        "db: saved a copy of the database (version {version}) as {} before updating it to version {latest}",
        backup.display()
    );
    Ok(())
}

/// Refuse a database a newer Joyscroller has updated past what this one
/// knows. Its tables may have changed in ways this version's queries would
/// fail on, or worse, write wrongly to.
pub(super) fn refuse_newer(conn: &Connection) -> Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let latest = i64::try_from(MIGRATIONS.len())?;
    if version > latest {
        bail!(
            "this library was made by a newer version of Joyscroller (database version \
             {version}; this version knows up to {latest}). Update Joyscroller to open it."
        );
    }
    Ok(())
}

/// Create the tables if needed and bring them up to date.
pub fn init(conn: &mut Connection) -> Result<()> {
    refuse_newer(conn)?;
    conn.execute_batch(SCHEMA)?;
    loop {
        // Take the write lock before reading the version, so two processes
        // opening the same old database can't both apply the same step.
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let Some(step) = MIGRATIONS.get(usize::try_from(version)?) else {
            return Ok(());
        };
        tx.execute_batch(step)
            .with_context(|| format!("applying database migration {}", version + 1))?;
        tx.pragma_update(None, "user_version", version + 1)?;
        tx.commit()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::*;
    use crate::db::*;

    #[test]
    fn migrates_an_original_database() {
        // A database made before any migration, with one TikTok photo post.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO videos (platform, video_id, url, raw_json, liked_at) VALUES \
             ('tiktok', 'p', 'u', '{\"createTime\": 1700000000, \"imagePost\": {}}', 0), \
             ('youtube', 'y', 'u', '{}', 0)",
            [],
        )
        .unwrap();
        init(&mut conn).unwrap();
        // Running it again changes nothing.
        init(&mut conn).unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, i64::try_from(MIGRATIONS.len()).unwrap());
        let rows: Vec<(String, Option<i64>)> = conn
            .prepare("SELECT kind, posted_at FROM videos ORDER BY platform")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            rows,
            [
                ("photo".into(), Some(1_700_000_000)),
                ("video".into(), None)
            ]
        );
    }

    /// A database at version 3, before the renames, with a video of each
    /// kind, a download, a failed download, a sync and a playback copy.
    fn version_3_database() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        for step in &MIGRATIONS[..3] {
            conn.execute_batch(step).unwrap();
        }
        conn.pragma_update(None, "user_version", 3).unwrap();
        conn.execute_batch(
            "INSERT INTO videos (platform, video_id, url, title, author_name, raw_json, liked_at, \
               kind, posted_at, available) VALUES \
             ('youtube', 'y', 'https://y', 'Dog walk', 'Doggo', '{\"uploader_id\": \"@doggo\"}', \
               100, 'video', 50, 1), \
             ('tiktok', 'p', 'https://p', 'Cat slides', 'kitty', '{}', 200, 'photo', 60, 0), \
             ('instagram', 'i', 'https://i', NULL, 'reels', '{}', 300, 'video', NULL, 1);
             INSERT INTO downloads (platform, video_id, attempts, last_attempt_at, last_error, \
               downloaded_at, dir, files, bytes, format, info_json) VALUES \
             ('youtube', 'y', 2, 400, NULL, 400, 'youtube/y', '[\"video.webm\"]', 1234, \
               '399+251', '{\"description\": \"a long walk\"}'), \
             ('tiktok', 'p', 3, 500, 'HTTP 429', NULL, NULL, NULL, NULL, NULL, NULL);
             INSERT INTO syncs (platform, started_at, finished_at, added) VALUES ('youtube', 1, 2, 7);
             INSERT INTO playback_copies (platform, video_id, download_folder, finished_at, \
               file_name, size_bytes, method, original_config_json) VALUES \
             ('youtube', 'y', 'youtube/y', 450, 'playback.mp4', 999, 'encode', '{}');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn renaming_keeps_every_row_and_value() {
        let mut conn = version_3_database();
        let rowids = |conn: &Connection| -> Vec<(i64, String)> {
            conn.prepare("SELECT rowid, video_id FROM videos ORDER BY rowid")
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap()
        };
        let before = rowids(&conn);
        init(&mut conn).unwrap();
        assert_eq!(rowids(&conn), before);

        let (added_at, listing_json): (i64, String) = conn
            .query_row(
                "SELECT added_at, listing_json FROM videos WHERE video_id = 'y'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (added_at, listing_json.as_str()),
            (100, r#"{"uploader_id": "@doggo"}"#)
        );
        let stored = crate::db::video(&conn, "tiktok", "p").unwrap().unwrap().0;
        assert_eq!((stored.added_at, stored.kind.as_str()), (200, "photo"));
        assert!(!stored.available);

        let download = download_state(&conn, "youtube", "y").unwrap().unwrap();
        assert_eq!(download.failures, 2);
        assert_eq!(download.folder.as_deref(), Some("youtube/y"));
        assert_eq!(download.files, ["video.webm"]);
        assert_eq!(download.size_bytes, Some(1234));
        let (formats, metadata): (String, String) = conn
            .query_row(
                "SELECT formats, metadata_json FROM downloads WHERE video_id = 'y'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(formats, "399+251");
        assert!(metadata.contains("a long walk"));
        let failed = download_state(&conn, "tiktok", "p").unwrap().unwrap();
        assert_eq!(
            (failed.failures, failed.last_error.as_deref()),
            (3, Some("HTTP 429"))
        );
        let added_count: i64 = conn
            .query_row("SELECT added_count FROM syncs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(added_count, 7);
        let copy = playback_state(&conn, "youtube", "y").unwrap().unwrap();
        assert_eq!(copy.file_name.as_deref(), Some("playback.mp4"));

        // Search still finds what it found, and keeps up with new rows.
        assert_eq!(found(&conn, "dog walk"), ["y"]);
        assert_eq!(found(&conn, "doggo"), ["y"]);
        assert_eq!(found(&conn, "long walk"), ["y"]);
        assert_eq!(found(&conn, "kitty"), ["p"]);
        insert_new(
            &mut conn,
            "youtube",
            &[NewVideo {
                title: Some("Bird song"),
                ..new_video("b", true)
            }],
        )
        .unwrap();
        assert_eq!(found(&conn, "bird"), ["b"]);
        for index in ["search_trigrams", "search_words"] {
            conn.execute(
                &format!("INSERT INTO {index} ({index}) VALUES ('integrity-check')"),
                [],
            )
            .unwrap();
        }
        // Nothing of the old names is left.
        let leftovers: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name LIKE 'video_text%' \
                   OR name IN ('video_search', 'video_words', 'playback') \
                   OR sql LIKE '%raw_json%' OR sql LIKE '%liked_at%' OR sql LIKE '%info_json%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn refuses_a_database_from_a_newer_version() {
        let mut conn = memory_db();
        let newer = i64::try_from(MIGRATIONS.len()).unwrap() + 1;
        conn.pragma_update(None, "user_version", newer).unwrap();
        let error = init(&mut conn).unwrap_err().to_string();
        assert!(error.contains("newer version of Joyscroller"), "{error}");
    }

    #[test]
    fn backs_up_a_database_before_migrating_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("joyscroller.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(SCHEMA).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute(
                "INSERT INTO videos (platform, video_id, url, raw_json, liked_at) \
                 VALUES ('youtube', 'y', 'u', '{}', 0)",
                [],
            )
            .unwrap();
        }
        open(&path).unwrap();
        let backups: Vec<_> = std::fs::read_dir(dir.path().join("backups"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(backups.len(), 1);
        let name = backups[0].file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("joyscroller-") && name.ends_with("-v1.db"),
            "{name}"
        );
        // The copy is the database as it was: version 1, old names and all.
        let copy = Connection::open(&backups[0]).unwrap();
        let version: i64 = copy
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
        let liked_at: i64 = copy
            .query_row("SELECT liked_at FROM videos", [], |r| r.get(0))
            .unwrap();
        assert_eq!(liked_at, 0);

        // Up to date now, so opening again copies nothing; nor does a new one.
        open(&path).unwrap();
        open(&dir.path().join("fresh.db")).unwrap();
        assert_eq!(
            std::fs::read_dir(dir.path().join("backups"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn migrating_indexes_the_videos_already_stored() {
        // A database from before search, as v0.1.0 left it.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute(
            "INSERT INTO videos (platform, video_id, url, title, author_name, raw_json, liked_at) VALUES \
             ('tiktok', 't', 'u', 'Cat piano', 'kitty', '{\"author\": {\"nickname\": \"Kitty Kat\"}}', 0), \
             ('youtube', 'y', 'u', 'Dog walk', 'Doggo', 'not json', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO downloads (platform, video_id, downloaded_at, info_json) \
             VALUES ('youtube', 'y', 1, '{\"description\": \"a long walk\"}')",
            [],
        )
        .unwrap();
        init(&mut conn).unwrap();
        assert_eq!(found(&conn, "piano"), ["t"]);
        assert_eq!(found(&conn, "kat"), ["t"]);
        assert_eq!(found(&conn, "doggo"), ["y"]);
        assert_eq!(found(&conn, "long walk"), ["y"]);
        // Both indexes agree with their content.
        for index in ["search_trigrams", "search_words"] {
            conn.execute(
                &format!("INSERT INTO {index} ({index}) VALUES ('integrity-check')"),
                [],
            )
            .unwrap();
        }
        // Videos stored afterwards are indexed as they arrive, and a later
        // download's description replaces nothing else.
        insert_new(
            &mut conn,
            "youtube",
            &[NewVideo {
                title: Some("Bird song"),
                ..new_video("b", true)
            }],
        )
        .unwrap();
        assert_eq!(found(&conn, "bird"), ["b"]);
        let info = r#"{"description": "recorded at dawn"}"#;
        let done = Downloaded {
            folder: "youtube/b",
            files: &[],
            size_bytes: 0,
            formats: None,
            metadata_json: Some(info),
            posted_at: None,
        };
        record_download_failure(&conn, "youtube", "b", "HTTP 429").unwrap();
        record_download(&mut conn, "youtube", "b", &done).unwrap();
        assert_eq!(found(&conn, "dawn"), ["b"]);
        assert_eq!(found(&conn, "bird"), ["b"]);
        for index in ["search_trigrams", "search_words"] {
            conn.execute(
                &format!("INSERT INTO {index} ({index}) VALUES ('integrity-check')"),
                [],
            )
            .unwrap();
        }
    }
}
