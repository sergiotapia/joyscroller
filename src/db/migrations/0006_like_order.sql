-- Like order gets a column of its own. Until now rowid order was like
-- order, which meant a sync had to hold a whole listing until it ended
-- and insert it oldest first. With `like_order` the rows can be written
-- page by page, newest first, as a listing comes in, and the list still
-- sorts the way the platform listed it: each listing takes a band of
-- numbers above everything stored (see `db/listings.rs`), its first post
-- the top of the band and each later one a step down. The rows stored so
-- far keep their order: their like_order is their rowid, below every band.
--
-- `listings` holds the listings still under way, so one cut short (a
-- restart, a failed page) carries on from its cursor in its own band,
-- with no gap and nothing out of order.

ALTER TABLE videos ADD COLUMN like_order INTEGER NOT NULL DEFAULT 0;
UPDATE videos SET like_order = rowid;
CREATE TABLE listings (
    list        TEXT PRIMARY KEY,         -- 'tiktok liked' | 'instagram saved' | 'instagram liked'
    top         INTEGER NOT NULL,         -- like_order of the listing's first post; the nth after it is top - n
    listed      INTEGER NOT NULL DEFAULT 0, -- posts listed and stored so far
    cursor      TEXT,                     -- where the next page starts; NULL = the start of the list
    full        INTEGER NOT NULL,         -- 1 = reads past stored posts to the end
    started_at  INTEGER NOT NULL
);
-- The column sorts break ties in like order now, so their indexes end in
-- it, and like order has an index of its own.
DROP INDEX videos_title_desc;
DROP INDEX videos_title_asc;
DROP INDEX videos_author_desc;
DROP INDEX videos_author_asc;
DROP INDEX videos_length_desc;
DROP INDEX videos_length_asc;
DROP INDEX videos_posted_desc;
DROP INDEX videos_posted_asc;
CREATE INDEX videos_like_order ON videos (like_order);
CREATE INDEX videos_title_desc ON videos (title COLLATE NOCASE, like_order);
CREATE INDEX videos_title_asc ON videos (title IS NULL, title COLLATE NOCASE, like_order);
CREATE INDEX videos_author_desc ON videos (author_name COLLATE NOCASE, like_order);
CREATE INDEX videos_author_asc ON videos (author_name IS NULL, author_name COLLATE NOCASE, like_order);
CREATE INDEX videos_length_desc ON videos (duration_seconds, like_order);
CREATE INDEX videos_length_asc ON videos (duration_seconds IS NULL, duration_seconds, like_order);
CREATE INDEX videos_posted_desc ON videos (posted_at, like_order);
CREATE INDEX videos_posted_asc ON videos (posted_at IS NULL, posted_at, like_order);
