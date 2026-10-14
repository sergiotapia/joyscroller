//! Fixtures the database modules' tests share: an in-memory database with
//! the whole schema, sample rows, and a search helper.

use rusqlite::Connection;

use super::*;
use crate::query;

pub(super) fn new_video(id: &str, available: bool) -> NewVideo<'_> {
    NewVideo {
        video_id: id,
        url: "https://example.com",
        title: available.then_some("title"),
        author_id: None,
        author_name: None,
        duration_seconds: Some(60),
        available,
        listing_json: "{}",
        kind: "video",
        posted_at: None,
    }
}

pub(super) fn memory_db() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    init(&mut conn).unwrap();
    conn
}

pub(super) fn download(conn: &mut Connection, platform: &str, id: &str) {
    let files = ["thumbnail.jpg".to_string(), "video.webm".to_string()];
    let dir = format!("{platform}/{id}");
    let done = Downloaded {
        folder: &dir,
        files: &files,
        size_bytes: 10,
        formats: None,
        metadata_json: None,
        posted_at: None,
    };
    record_download(conn, platform, id, &done).unwrap();
}

pub(super) fn ids(videos: Vec<StoredVideo>) -> Vec<String> {
    videos.into_iter().map(|v| v.video_id).collect()
}

/// The ids `text` finds, sorted. Every search is also counted, read two
/// at a time and read in another order, so the count, the paging and the
/// list always agree.
pub(super) fn found(conn: &Connection, text: &str) -> Vec<String> {
    let query = query::parse(text);
    let best = Order {
        sort: Sort::Best,
        dir: Dir::Asc,
    };
    let all = ids(videos_page(conn, None, &query, best, 100, 0).unwrap());
    assert_eq!(
        videos_count(conn, None, &query).unwrap(),
        all.len(),
        "count disagrees with the list for {text:?}"
    );
    let mut paged = Vec::new();
    for offset in (0..).step_by(2) {
        let page = ids(videos_page(conn, None, &query, best, 2, offset).unwrap());
        if page.is_empty() {
            break;
        }
        paged.extend(page);
    }
    assert_eq!(paged, all, "paging disagrees with the list for {text:?}");
    let mut sorted = all;
    sorted.sort();
    let mut by_title = ids(videos_page(
        conn,
        None,
        &query,
        Order {
            sort: Sort::Title,
            dir: Dir::Asc,
        },
        100,
        0,
    )
    .unwrap());
    by_title.sort();
    assert_eq!(by_title, sorted, "sorting changed what {text:?} finds");
    sorted
}
