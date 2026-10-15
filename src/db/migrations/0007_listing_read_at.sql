-- When each listing under way last stored a page. A listing cut short
-- carries on from its cursor unless that cursor is old enough to have gone
-- bad, and the cursor is as old as the last page read, not the listing:
-- one paused for hours at a time (Instagram's "Try Again Later" block)
-- could otherwise pass a day since it started and read its whole list
-- again from the top, re-asking for every page it already had.

ALTER TABLE listings ADD COLUMN read_at INTEGER NOT NULL DEFAULT 0;  -- unix seconds
UPDATE listings SET read_at = started_at;
