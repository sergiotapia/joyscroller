-- Full-text search (see `search.rs`) and indexes for the list's
-- column sorts. `video_text` is what's searched, one row per video under
-- the video's rowid: the title, the author (the stored name plus the
-- handle, display name or channel the platform also gives), and the
-- description, which only a download's metadata has (YouTube's listing
-- leaves it out; TikTok's and Instagram's repeat the caption, so a
-- description equal to the title is left out). Two FTS5 indexes read it
-- as external content: trigrams for substrings in any script, and
-- unicode61 words. The view says how a video's text is made, so the
-- backfill and the triggers can't disagree; a later step can replace it
-- and refill `video_text`. Video rows are insert-only and `available`
-- isn't searched, so only a download changes a video's text. The JSON is
-- always valid, but json_valid keeps one bad row from failing a sync.

CREATE VIEW video_text_source AS SELECT
    v.rowid AS id,
    v.title,
    concat_ws(' ', v.author_name, nullif(CASE WHEN json_valid(v.raw_json) THEN CASE v.platform
        WHEN 'youtube' THEN json_extract(v.raw_json, '$.uploader_id')
        WHEN 'tiktok' THEN json_extract(v.raw_json, '$.author.nickname')
        WHEN 'instagram' THEN json_extract(v.raw_json, '$.user.full_name')
      END END, v.author_name)) AS author,
    nullif(CASE WHEN json_valid(d.info_json) THEN json_extract(d.info_json, '$.description') END,
      v.title) AS description
  FROM videos v LEFT JOIN downloads d ON d.platform = v.platform AND d.video_id = v.video_id;
CREATE TABLE video_text (
    id          INTEGER PRIMARY KEY,      -- the video's rowid
    title       TEXT,
    author      TEXT,
    description TEXT
);
CREATE VIRTUAL TABLE video_search USING fts5(
    title, author, description,
    content='video_text', content_rowid='id',
    tokenize='trigram remove_diacritics 2'
);
-- prefix='1 2' keeps a short word cheap to search while it's being typed.
CREATE VIRTUAL TABLE video_words USING fts5(
    title, author, description,
    content='video_text', content_rowid='id',
    tokenize='unicode61 remove_diacritics 2',
    prefix='1 2'
);
INSERT INTO video_text (id, title, author, description)
  SELECT id, title, author, description FROM video_text_source;
INSERT INTO video_search (video_search) VALUES ('rebuild');
INSERT INTO video_words (video_words) VALUES ('rebuild');
CREATE TRIGGER video_text_ai AFTER INSERT ON video_text BEGIN
  INSERT INTO video_search (rowid, title, author, description)
    VALUES (new.id, new.title, new.author, new.description);
  INSERT INTO video_words (rowid, title, author, description)
    VALUES (new.id, new.title, new.author, new.description);
END;
-- An external-content index forgets a row through its 'delete' command
-- with the old values; inserting them would add the tokens again.
CREATE TRIGGER video_text_ad AFTER DELETE ON video_text BEGIN
  INSERT INTO video_search (video_search, rowid, title, author, description)
    VALUES ('delete', old.id, old.title, old.author, old.description);
  INSERT INTO video_words (video_words, rowid, title, author, description)
    VALUES ('delete', old.id, old.title, old.author, old.description);
END;
CREATE TRIGGER video_text_au AFTER UPDATE ON video_text
WHEN old.title IS NOT new.title OR old.author IS NOT new.author
  OR old.description IS NOT new.description
BEGIN
  INSERT INTO video_search (video_search, rowid, title, author, description)
    VALUES ('delete', old.id, old.title, old.author, old.description);
  INSERT INTO video_words (video_words, rowid, title, author, description)
    VALUES ('delete', old.id, old.title, old.author, old.description);
  INSERT INTO video_search (rowid, title, author, description)
    VALUES (new.id, new.title, new.author, new.description);
  INSERT INTO video_words (rowid, title, author, description)
    VALUES (new.id, new.title, new.author, new.description);
END;
CREATE TRIGGER videos_text_ai AFTER INSERT ON videos BEGIN
  INSERT INTO video_text (id, title, author, description)
    SELECT id, title, author, description FROM video_text_source WHERE id = new.rowid;
END;
CREATE TRIGGER videos_text_ad AFTER DELETE ON videos BEGIN
  DELETE FROM video_text WHERE id = old.rowid;
END;
CREATE TRIGGER downloads_text_ai AFTER INSERT ON downloads BEGIN
  UPDATE video_text SET description =
      (SELECT description FROM video_text_source s WHERE s.id = video_text.id)
    WHERE id = (SELECT rowid FROM videos WHERE platform = new.platform AND video_id = new.video_id);
END;
CREATE TRIGGER downloads_text_au AFTER UPDATE OF info_json ON downloads BEGIN
  UPDATE video_text SET description =
      (SELECT description FROM video_text_source s WHERE s.id = video_text.id)
    WHERE id = (SELECT rowid FROM videos WHERE platform = new.platform AND video_id = new.video_id);
END;
-- The column sorts, two indexes each, matching `order_by` term for term:
-- (x) read backwards is x DESC with empty values last, as SQLite sorts
-- NULL lowest; (x IS NULL, x) read forwards is x ASC with them last.
-- Every index ends in the rowid, which breaks ties the same way.
CREATE INDEX videos_title_desc ON videos (title COLLATE NOCASE);
CREATE INDEX videos_title_asc ON videos (title IS NULL, title COLLATE NOCASE);
CREATE INDEX videos_author_desc ON videos (author_name COLLATE NOCASE);
CREATE INDEX videos_author_asc ON videos (author_name IS NULL, author_name COLLATE NOCASE);
CREATE INDEX videos_length_desc ON videos (duration_seconds);
CREATE INDEX videos_length_asc ON videos (duration_seconds IS NULL, duration_seconds);
CREATE INDEX videos_posted_desc ON videos (posted_at);
CREATE INDEX videos_posted_asc ON videos (posted_at IS NULL, posted_at);
