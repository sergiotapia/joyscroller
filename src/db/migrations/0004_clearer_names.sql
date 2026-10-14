-- Clearer names. `liked_at` is when a sync first found the like (the
-- platforms don't say when it was liked); `attempts` only ever counted
-- failures; `format` holds several format ids, or `photo`; the two JSON
-- blobs say where they come from; JSON columns end in `_json`. Renaming
-- a column keeps its data and rewrites the views and triggers that use
-- it, but the full-text indexes name their content table in their own
-- definition, so the search objects are dropped and made again under
-- their new names. They hold nothing of their own: `search_text` keeps
-- its rows through the rename, and the indexes are rebuilt from it.

DROP TRIGGER video_text_ai;
DROP TRIGGER video_text_ad;
DROP TRIGGER video_text_au;
DROP TRIGGER videos_text_ai;
DROP TRIGGER videos_text_ad;
DROP TRIGGER downloads_text_ai;
DROP TRIGGER downloads_text_au;
DROP VIEW video_text_source;
DROP TABLE video_search;
DROP TABLE video_words;
ALTER TABLE video_text RENAME TO search_text;
ALTER TABLE videos RENAME COLUMN liked_at TO added_at;
ALTER TABLE videos RENAME COLUMN raw_json TO listing_json;
ALTER TABLE syncs RENAME COLUMN added TO added_count;
ALTER TABLE downloads RENAME COLUMN attempts TO failures;
ALTER TABLE downloads RENAME COLUMN dir TO folder;
ALTER TABLE downloads RENAME COLUMN files TO files_json;
ALTER TABLE downloads RENAME COLUMN bytes TO size_bytes;
ALTER TABLE downloads RENAME COLUMN format TO formats;
ALTER TABLE downloads RENAME COLUMN info_json TO metadata_json;
CREATE VIEW search_text_source AS SELECT
    v.rowid AS id,
    v.title,
    concat_ws(' ', v.author_name, nullif(CASE WHEN json_valid(v.listing_json) THEN CASE v.platform
        WHEN 'youtube' THEN json_extract(v.listing_json, '$.uploader_id')
        WHEN 'tiktok' THEN json_extract(v.listing_json, '$.author.nickname')
        WHEN 'instagram' THEN json_extract(v.listing_json, '$.user.full_name')
      END END, v.author_name)) AS author,
    nullif(CASE WHEN json_valid(d.metadata_json) THEN json_extract(d.metadata_json, '$.description') END,
      v.title) AS description
  FROM videos v LEFT JOIN downloads d ON d.platform = v.platform AND d.video_id = v.video_id;
CREATE VIRTUAL TABLE search_trigrams USING fts5(
    title, author, description,
    content='search_text', content_rowid='id',
    tokenize='trigram remove_diacritics 2'
);
CREATE VIRTUAL TABLE search_words USING fts5(
    title, author, description,
    content='search_text', content_rowid='id',
    tokenize='unicode61 remove_diacritics 2',
    prefix='1 2'
);
INSERT INTO search_trigrams (search_trigrams) VALUES ('rebuild');
INSERT INTO search_words (search_words) VALUES ('rebuild');
CREATE TRIGGER search_text_ai AFTER INSERT ON search_text BEGIN
  INSERT INTO search_trigrams (rowid, title, author, description)
    VALUES (new.id, new.title, new.author, new.description);
  INSERT INTO search_words (rowid, title, author, description)
    VALUES (new.id, new.title, new.author, new.description);
END;
CREATE TRIGGER search_text_ad AFTER DELETE ON search_text BEGIN
  INSERT INTO search_trigrams (search_trigrams, rowid, title, author, description)
    VALUES ('delete', old.id, old.title, old.author, old.description);
  INSERT INTO search_words (search_words, rowid, title, author, description)
    VALUES ('delete', old.id, old.title, old.author, old.description);
END;
CREATE TRIGGER search_text_au AFTER UPDATE ON search_text
WHEN old.title IS NOT new.title OR old.author IS NOT new.author
  OR old.description IS NOT new.description
BEGIN
  INSERT INTO search_trigrams (search_trigrams, rowid, title, author, description)
    VALUES ('delete', old.id, old.title, old.author, old.description);
  INSERT INTO search_words (search_words, rowid, title, author, description)
    VALUES ('delete', old.id, old.title, old.author, old.description);
  INSERT INTO search_trigrams (rowid, title, author, description)
    VALUES (new.id, new.title, new.author, new.description);
  INSERT INTO search_words (rowid, title, author, description)
    VALUES (new.id, new.title, new.author, new.description);
END;
CREATE TRIGGER videos_search_ai AFTER INSERT ON videos BEGIN
  INSERT INTO search_text (id, title, author, description)
    SELECT id, title, author, description FROM search_text_source WHERE id = new.rowid;
END;
CREATE TRIGGER videos_search_ad AFTER DELETE ON videos BEGIN
  DELETE FROM search_text WHERE id = old.rowid;
END;
CREATE TRIGGER downloads_search_ai AFTER INSERT ON downloads BEGIN
  UPDATE search_text SET description =
      (SELECT description FROM search_text_source s WHERE s.id = search_text.id)
    WHERE id = (SELECT rowid FROM videos WHERE platform = new.platform AND video_id = new.video_id);
END;
CREATE TRIGGER downloads_search_au AFTER UPDATE OF metadata_json ON downloads BEGIN
  UPDATE search_text SET description =
      (SELECT description FROM search_text_source s WHERE s.id = search_text.id)
    WHERE id = (SELECT rowid FROM videos WHERE platform = new.platform AND video_id = new.video_id);
END;
