-- X posts in search: the author's display name, as for the other
-- platforms, and the post a post quotes (its author and text), which the
-- video page shows under it. No stored row reads differently until an X
-- post is stored, so search_text needs no refill.

DROP VIEW search_text_source;
CREATE VIEW search_text_source AS SELECT
    v.rowid AS id,
    v.title,
    concat_ws(' ', v.author_name, nullif(CASE WHEN json_valid(v.listing_json) THEN CASE v.platform
        WHEN 'youtube' THEN json_extract(v.listing_json, '$.uploader_id')
        WHEN 'tiktok' THEN json_extract(v.listing_json, '$.author.nickname')
        WHEN 'instagram' THEN json_extract(v.listing_json, '$.user.full_name')
        WHEN 'x' THEN json_extract(v.listing_json, '$.core.user_results.result.core.name')
      END END, v.author_name)) AS author,
    CASE WHEN v.platform = 'x' THEN
      CASE WHEN json_valid(v.listing_json) THEN nullif(concat_ws(' ',
        json_extract(v.listing_json, '$.quoted_status_result.result.core.user_results.result.core.screen_name'),
        json_extract(v.listing_json, '$.quoted_status_result.result.core.user_results.result.core.name'),
        coalesce(
          json_extract(v.listing_json, '$.quoted_status_result.result.note_tweet.note_tweet_results.result.text'),
          json_extract(v.listing_json, '$.quoted_status_result.result.legacy.full_text'))), '') END
    ELSE nullif(CASE WHEN json_valid(d.metadata_json) THEN json_extract(d.metadata_json, '$.description') END,
      v.title) END AS description
  FROM videos v LEFT JOIN downloads d ON d.platform = v.platform AND d.video_id = v.video_id;
