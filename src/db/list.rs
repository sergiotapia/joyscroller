//! The video list: a page of videos, filtered by platform and search, in a
//! column's order. Each column sort has two indexes (migration 6), and
//! `order_by` must match them term for term; a test checks the query plans.

use anyhow::Result;
use rusqlite::types::Value;
use rusqlite::{Connection, params_from_iter};

use super::search::search_matches;
use super::videos::{StoredVideo, VIDEO_COLUMNS, VIDEO_TABLES, video_from_row};
use crate::query::Query;

/// What the list can be ordered by. This is the whole list: no other text
/// ever reaches an `ORDER BY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sort {
    /// The search's best matches first. Without a search, newest first.
    Best,
    Title,
    Author,
    Length,
    Posted,
    /// Like order (`like_order`).
    Added,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Order {
    pub sort: Sort,
    pub dir: Dir,
}

impl Order {
    /// The list's usual order: most recently liked first.
    pub const NEWEST: Order = Order {
        sort: Sort::Added,
        dir: Dir::Desc,
    };
}

/// The `ORDER BY` terms for `order`, over `videos v` joined to `matches m`
/// when `searching`. Rows without the value (no caption, a YouTube video
/// whose post date isn't known until it's downloaded) go last either way.
/// Every order ends in like order and then the rowid, so equal values keep
/// one order and paging neither skips nor repeats a row. Each column order
/// is term for term one of the indexes migration 6 made (every index ends in
/// the rowid), so SQLite reads it from the index instead of sorting the table.
fn order_by(order: Order, searching: bool) -> String {
    // Names compare without regard to case, so "alice" doesn't sort after
    // "Zed". The collation goes on the sort term only: `title IS NULL` must
    // read exactly as the index has it.
    let (column, collate) = match order.sort {
        Sort::Best if searching => return "m.score, v.like_order DESC, v.rowid DESC".into(),
        Sort::Best => return "v.like_order DESC, v.rowid DESC".into(),
        Sort::Added => {
            return match order.dir {
                Dir::Asc => "v.like_order ASC, v.rowid ASC".into(),
                Dir::Desc => "v.like_order DESC, v.rowid DESC".into(),
            };
        }
        Sort::Title => ("v.title", " COLLATE NOCASE"),
        Sort::Author => ("v.author_name", " COLLATE NOCASE"),
        Sort::Length => ("v.duration_seconds", ""),
        Sort::Posted => ("v.posted_at", ""),
    };
    // SQLite sorts NULL lowest, so descending already puts it last.
    match order.dir {
        Dir::Asc => {
            format!("{column} IS NULL, {column}{collate} ASC, v.like_order ASC, v.rowid ASC")
        }
        Dir::Desc => format!("{column}{collate} DESC, v.like_order DESC, v.rowid DESC"),
    }
}

/// The values for `videos_page` and `videos_count`'s `?`s: the search's,
/// then the platform twice.
fn list_params(search: Vec<String>, platform: Option<&str>) -> Vec<Value> {
    let platform = platform.map_or(Value::Null, |name| Value::Text(name.into()));
    search
        .into_iter()
        .map(Value::Text)
        .chain([platform.clone(), platform])
        .collect()
}

/// One page of videos on one platform or all, those `search` matches (all
/// of them when it's empty), in `order`.
pub fn videos_page(
    conn: &Connection,
    platform: Option<&str>,
    search: &Query,
    order: Order,
    limit: usize,
    offset: usize,
) -> Result<Vec<StoredVideo>> {
    // bm25 is most of what a search costs, so it only runs to rank.
    let found = search_matches(search, order.sort == Sort::Best);
    let searching = found.is_some();
    let (with, join, search_params) = match found {
        Some((sql, params)) => (
            format!("WITH {sql}"),
            "JOIN matches m ON m.id = v.rowid",
            params,
        ),
        None => (String::new(), "", Vec::new()),
    };
    let mut params = list_params(search_params, platform);
    params.push(Value::Integer(i64::try_from(limit)?));
    params.push(Value::Integer(i64::try_from(offset)?));
    let mut stmt = conn.prepare(&format!(
        "{with} SELECT {VIDEO_COLUMNS} FROM {VIDEO_TABLES} {join} \
         WHERE ? IS NULL OR v.platform = ? ORDER BY {} LIMIT ? OFFSET ?",
        order_by(order, searching)
    ))?;
    let videos = stmt
        .query_map(params_from_iter(params), video_from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(videos)
}

/// How many videos `videos_page` pages through.
pub fn videos_count(conn: &Connection, platform: Option<&str>, search: &Query) -> Result<usize> {
    let (with, join, search_params) = match search_matches(search, false) {
        Some((sql, params)) => (
            format!("WITH {sql}"),
            "JOIN matches m ON m.id = v.rowid",
            params,
        ),
        None => (String::new(), "", Vec::new()),
    };
    let count: i64 = conn.query_row(
        &format!("{with} SELECT count(*) FROM videos v {join} WHERE ? IS NULL OR v.platform = ?"),
        params_from_iter(list_params(search_params, platform)),
        |r| r.get(0),
    )?;
    Ok(usize::try_from(count)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::*;
    use crate::db::*;

    #[test]
    fn pages_newest_first_per_platform() {
        let mut conn = memory_db();
        insert_new(
            &mut conn,
            "youtube",
            // Listed newest first.
            &[new_video("y2", true), new_video("y1", true)],
        )
        .unwrap();
        insert_new(&mut conn, "tiktok", &[new_video("t1", true)]).unwrap();
        let ids = |platform, limit, offset| -> Vec<String> {
            videos_page(
                &conn,
                platform,
                &Query::default(),
                Order::NEWEST,
                limit,
                offset,
            )
            .unwrap()
            .into_iter()
            .map(|v| v.video_id)
            .collect()
        };
        assert_eq!(ids(None, 10, 0), ["t1", "y2", "y1"]);
        assert_eq!(ids(None, 1, 1), ["y2"]);
        assert_eq!(ids(Some("youtube"), 10, 0), ["y2", "y1"]);
        assert!(ids(Some("instagram"), 10, 0).is_empty());

        let mut counts = video_counts(&conn).unwrap();
        counts.sort();
        assert_eq!(counts, [("tiktok".into(), 1), ("youtube".into(), 2)]);
    }

    #[test]
    fn sorts_by_each_column_with_empty_values_last() {
        let mut conn = memory_db();
        insert_new(
            &mut conn,
            "youtube",
            &[
                NewVideo {
                    title: Some("cherry"),
                    author_name: Some("Bob"),
                    duration_seconds: Some(30),
                    posted_at: Some(300),
                    ..new_video("c", true)
                },
                NewVideo {
                    title: Some("Apple"),
                    author_name: Some("amy"),
                    duration_seconds: Some(90),
                    posted_at: Some(100),
                    ..new_video("a", true)
                },
                NewVideo {
                    title: None,
                    author_name: None,
                    duration_seconds: None,
                    posted_at: None,
                    ..new_video("none", true)
                },
                NewVideo {
                    title: Some("banana"),
                    author_name: Some("Zed"),
                    duration_seconds: Some(30),
                    posted_at: Some(200),
                    ..new_video("b", true)
                },
            ],
        )
        .unwrap();
        let page = |sort, dir| {
            ids(videos_page(&conn, None, &Query::default(), Order { sort, dir }, 10, 0).unwrap())
        };
        assert_eq!(page(Sort::Title, Dir::Asc), ["a", "b", "c", "none"]);
        assert_eq!(page(Sort::Title, Dir::Desc), ["c", "b", "a", "none"]);
        assert_eq!(page(Sort::Author, Dir::Asc), ["a", "c", "b", "none"]);
        // Equal lengths keep like order, reversed with the rest.
        assert_eq!(page(Sort::Length, Dir::Asc), ["b", "c", "a", "none"]);
        assert_eq!(page(Sort::Length, Dir::Desc), ["a", "c", "b", "none"]);
        assert_eq!(page(Sort::Posted, Dir::Desc), ["c", "b", "a", "none"]);
        assert_eq!(page(Sort::Posted, Dir::Asc), ["a", "b", "c", "none"]);
        assert_eq!(page(Sort::Added, Dir::Asc), ["b", "none", "a", "c"]);
        assert_eq!(page(Sort::Added, Dir::Desc), ["c", "a", "none", "b"]);
        assert_eq!(page(Sort::Best, Dir::Asc), ["c", "a", "none", "b"]);
    }

    #[test]
    fn column_sorts_read_their_index() {
        // SQLite plans an empty table differently from an archive.
        let mut conn = memory_db();
        let ids: Vec<String> = (0..2000).map(|n| format!("v{n}")).collect();
        let videos: Vec<NewVideo> = ids.iter().map(|id| new_video(id, true)).collect();
        insert_new(&mut conn, "youtube", &videos).unwrap();
        for sort in [
            Sort::Title,
            Sort::Author,
            Sort::Length,
            Sort::Posted,
            Sort::Added,
            Sort::Best,
        ] {
            for dir in [Dir::Asc, Dir::Desc] {
                let sql = format!(
                    "EXPLAIN QUERY PLAN SELECT v.rowid FROM videos v \
                     WHERE ?1 IS NULL OR v.platform = ?1 ORDER BY {} LIMIT 50",
                    order_by(Order { sort, dir }, false)
                );
                let plan: Vec<String> = conn
                    .prepare(&sql)
                    .unwrap()
                    .query_map([Option::<&str>::None], |r| r.get(3))
                    .unwrap()
                    .collect::<rusqlite::Result<_>>()
                    .unwrap();
                let plan = plan.join("\n");
                assert!(
                    plan.contains("USING INDEX videos_"),
                    "{sort:?} {dir:?}:\n{plan}"
                );
                assert!(!plan.contains("TEMP B-TREE"), "{sort:?} {dir:?}:\n{plan}");
            }
        }
    }
}
