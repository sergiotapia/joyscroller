//! Like order, and the listings under way. A platform lists likes newest
//! first and gives no like times, so a post's place in its listing is all
//! that says when it was liked. Each listing takes a band of `like_order`
//! above everything stored: its first post gets the top of the band and
//! each later one a step down, so rows can be stored page by page as they
//! come and still sort as the platform listed them.
//!
//! A listing read page by page (TikTok's, Instagram's) keeps a row in
//! `listings` until it ends, with how far it got and the platform's cursor
//! for the next page, stored with each page. One cut short by a restart or
//! a failure carries on from there in its own band, so nothing is missed
//! and nothing lands out of order.

use anyhow::Result;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::now;
use super::videos::{NewVideo, insert_rows};

/// How much like order each listing takes: far more posts than any listing
/// holds (TikTok's runs out at about 5,000), with room left for half a
/// trillion listings.
const BAND: i64 = 1 << 24;

/// How long a listing cut short may wait to carry on from its cursor. The
/// platforms don't say how long their cursors stay good, so one older than
/// this reads its list again from the top, in its own band.
const STALE_AFTER: i64 = 24 * 60 * 60;

/// A listing under way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// Which list it reads, e.g. `instagram liked`.
    pub list: String,
    /// The `like_order` of its first post.
    top: i64,
    /// Posts listed and stored so far.
    listed: i64,
    /// Where its next page starts; None for the start of the list.
    pub cursor: Option<String>,
    /// Whether it reads past stored posts to the end of the list.
    pub full: bool,
}

/// The top of a new band: above every stored post, and above the bands of
/// listings still under way, which will store more posts in them.
pub(super) fn new_top(tx: &Transaction) -> Result<i64> {
    let highest: i64 = tx.query_row(
        "SELECT max(COALESCE((SELECT MAX(like_order) FROM videos), 0), \
                    COALESCE((SELECT MAX(top) FROM listings), 0))",
        [],
        |r| r.get(0),
    )?;
    Ok(highest + BAND)
}

/// The listing of `list` to read now: the one under way, carrying on from
/// its cursor, or a new one from the top of the list. `full` makes either
/// read to the end. A listing that has waited past `STALE_AFTER` starts its
/// list again from the top, in full: the posts it stored are passed over,
/// and the ones it never got to fill in the rest of its band.
pub fn open_listing(conn: &mut Connection, list: &str, full: bool) -> Result<Listing> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let found = tx
        .query_row(
            "SELECT top, listed, cursor, full, started_at FROM listings WHERE list = ?1",
            [list],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, bool>(3)?,
                    r.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let listing = match found {
        Some((top, _, _, _, started_at)) if now() - started_at > STALE_AFTER => {
            tx.execute(
                "UPDATE listings SET listed = 0, cursor = NULL, full = 1, started_at = ?2 \
                 WHERE list = ?1",
                params![list, now()],
            )?;
            Listing {
                list: list.to_string(),
                top,
                listed: 0,
                cursor: None,
                full: true,
            }
        }
        Some((top, listed, cursor, was_full, _)) => {
            if full && !was_full {
                tx.execute("UPDATE listings SET full = 1 WHERE list = ?1", [list])?;
            }
            Listing {
                list: list.to_string(),
                top,
                listed,
                cursor,
                full: full || was_full,
            }
        }
        None => {
            let top = new_top(&tx)?;
            tx.execute(
                "INSERT INTO listings (list, top, full, started_at) VALUES (?1, ?2, ?3, ?4)",
                params![list, top, full, now()],
            )?;
            Listing {
                list: list.to_string(),
                top,
                listed: 0,
                cursor: None,
                full,
            }
        }
    };
    tx.commit()?;
    Ok(listing)
}

/// Store one page of `listing` (`videos` as listed, newest first) and where
/// the next page starts, together, so a listing cut short carries on after
/// the last page it stored. Returns how many videos were new.
pub fn store_page(
    conn: &mut Connection,
    platform: &str,
    listing: &mut Listing,
    videos: &[NewVideo],
    next: Option<&str>,
) -> Result<usize> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let added = insert_rows(&tx, platform, videos, listing.top - listing.listed)?;
    let listed = listing.listed + i64::try_from(videos.len())?;
    tx.execute(
        "UPDATE listings SET listed = ?2, cursor = ?3 WHERE list = ?1",
        params![listing.list, listed, next],
    )?;
    tx.commit()?;
    listing.listed = listed;
    listing.cursor = next.map(str::to_string);
    Ok(added)
}

/// Record that `listing` has reached its end, or stopped at posts already
/// stored. The list's next listing starts from the top, in a new band.
pub fn finish_listing(conn: &Connection, listing: &Listing) -> Result<()> {
    conn.execute("DELETE FROM listings WHERE list = ?1", [&listing.list])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_support::*;
    use crate::db::*;

    /// Video ids on the first page of the list, newest first.
    fn newest(conn: &Connection) -> Vec<String> {
        videos_page(
            conn,
            None,
            &crate::query::Query::default(),
            Order::NEWEST,
            100,
            0,
        )
        .unwrap()
        .into_iter()
        .map(|v| v.video_id)
        .collect()
    }

    #[test]
    fn pages_stored_as_they_come_sort_as_listed() {
        let mut conn = memory_db();
        insert_new(&mut conn, "youtube", &[new_video("old", true)]).unwrap();

        let mut listing = open_listing(&mut conn, "tiktok liked", false).unwrap();
        assert_eq!(listing.cursor, None);
        let page = [new_video("t1", true), new_video("t2", true)];
        assert_eq!(
            store_page(&mut conn, "tiktok", &mut listing, &page, Some("c1")).unwrap(),
            2
        );
        let page = [new_video("t3", true)];
        store_page(&mut conn, "tiktok", &mut listing, &page, None).unwrap();
        finish_listing(&conn, &listing).unwrap();

        assert_eq!(newest(&conn), ["t1", "t2", "t3", "old"]);
        // The next listing goes above it.
        let mut next = open_listing(&mut conn, "tiktok liked", false).unwrap();
        store_page(
            &mut conn,
            "tiktok",
            &mut next,
            &[new_video("t0", true)],
            None,
        )
        .unwrap();
        assert_eq!(newest(&conn), ["t0", "t1", "t2", "t3", "old"]);
    }

    #[test]
    fn a_listing_cut_short_carries_on_in_its_band() {
        let mut conn = memory_db();
        let mut listing = open_listing(&mut conn, "instagram liked", true).unwrap();
        store_page(
            &mut conn,
            "instagram",
            &mut listing,
            &[new_video("a", true)],
            Some("c1"),
        )
        .unwrap();

        // Meanwhile another platform's listing stores newer posts.
        insert_new(&mut conn, "youtube", &[new_video("y", true)]).unwrap();

        // A restart: the listing carries on from where it got to.
        let mut resumed = open_listing(&mut conn, "instagram liked", false).unwrap();
        assert_eq!(resumed.cursor.as_deref(), Some("c1"));
        assert!(resumed.full);
        store_page(
            &mut conn,
            "instagram",
            &mut resumed,
            &[new_video("b", true)],
            None,
        )
        .unwrap();
        finish_listing(&conn, &resumed).unwrap();
        assert_eq!(newest(&conn), ["y", "a", "b"]);
    }

    #[test]
    fn listings_under_way_keep_their_bands_apart() {
        let mut conn = memory_db();
        let mut tiktok = open_listing(&mut conn, "tiktok liked", false).unwrap();
        let mut instagram = open_listing(&mut conn, "instagram saved", false).unwrap();
        // Neither has stored anything when the other starts; the second
        // still goes above the first.
        store_page(
            &mut conn,
            "tiktok",
            &mut tiktok,
            &[new_video("t", true)],
            None,
        )
        .unwrap();
        store_page(
            &mut conn,
            "instagram",
            &mut instagram,
            &[new_video("i", true)],
            None,
        )
        .unwrap();
        assert_eq!(newest(&conn), ["i", "t"]);
    }

    #[test]
    fn a_stale_listing_reads_its_list_again_in_full() {
        let mut conn = memory_db();
        let mut listing = open_listing(&mut conn, "tiktok liked", false).unwrap();
        store_page(
            &mut conn,
            "tiktok",
            &mut listing,
            &[new_video("a", true)],
            Some("c1"),
        )
        .unwrap();
        conn.execute(
            "UPDATE listings SET started_at = started_at - 2 * 86400",
            [],
        )
        .unwrap();

        let again = open_listing(&mut conn, "tiktok liked", false).unwrap();
        assert_eq!((again.cursor.as_deref(), again.full), (None, true));
        assert_eq!(again.top, listing.top);
    }

    #[test]
    fn a_migrated_library_keeps_its_order() {
        // Rows stored before like_order, in rowid order, oldest first.
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::migrate::SCHEMA).unwrap();
        conn.execute(
            "INSERT INTO videos (platform, video_id, url, raw_json, liked_at) VALUES \
             ('youtube', 'first', 'u', '{}', 0), ('tiktok', 'second', 'u', '{}', 0), \
             ('youtube', 'third', 'u', '{}', 0)",
            [],
        )
        .unwrap();
        crate::db::init(&mut conn).unwrap();
        assert_eq!(newest(&conn), ["third", "second", "first"]);
        // New likes go above them.
        insert_new(&mut conn, "youtube", &[new_video("new", true)]).unwrap();
        assert_eq!(newest(&conn), ["new", "third", "second", "first"]);
    }

    #[test]
    fn stored_posts_keep_their_place() {
        // A re-liked post comes back at the top of a listing, but stays
        // where it was first stored.
        let mut conn = memory_db();
        insert_new(
            &mut conn,
            "youtube",
            &[new_video("a", true), new_video("b", true)],
        )
        .unwrap();
        insert_new(
            &mut conn,
            "youtube",
            &[new_video("b", true), new_video("c", true)],
        )
        .unwrap();
        assert_eq!(newest(&conn), ["c", "a", "b"]);
    }
}
