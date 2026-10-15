-- When the platform said a video is gone, so it's never tried again: TikTok
-- answers a video it no longer shows with status 10204 ("Video currently
-- unavailable" on its site), which yt-dlp reports as "Your IP address is
-- blocked from accessing this post", and keeps the video in the liked list.
-- The video's row stays, with its link and title, and the sync can't undo
-- this the way it refreshes `videos.available` from the list.

ALTER TABLE downloads ADD COLUMN unavailable_at INTEGER;  -- unix seconds; NULL = not known to be gone

UPDATE downloads SET unavailable_at = last_attempt_at
  WHERE downloaded_at IS NULL
    AND last_error LIKE '%[TikTok]%'
    AND (last_error LIKE '%Your IP address is blocked from accessing this post%'
         OR last_error LIKE '%Video not available, status code%');
