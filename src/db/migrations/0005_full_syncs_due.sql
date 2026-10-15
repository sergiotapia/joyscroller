-- Lists whose next sync reads them in full instead of stopping at posts
-- already stored, removed once a read has gone all the way through.
-- Earlier versions kept only the videos a user saved on Instagram and
-- skipped saved photos and carousels, and an ordinary sync stops long
-- before the older ones, so a library that already has Instagram saves
-- reads them all once more. Instagram's liked list is new, and its first
-- read can't stop at stored posts either: the saves are stored, and the
-- newest likes are often the same posts.

CREATE TABLE full_syncs_due (
    list TEXT PRIMARY KEY                -- 'instagram saved' | 'instagram liked'
);
INSERT INTO full_syncs_due (list)
  SELECT 'instagram saved' WHERE EXISTS (SELECT 1 FROM videos WHERE platform = 'instagram');
INSERT INTO full_syncs_due (list) VALUES ('instagram liked');
