-- Uploading to the PDS can be paused in Settings, and stays paused across
-- restarts until it's resumed.

ALTER TABLE pds_accounts ADD COLUMN paused_at INTEGER;  -- when the user paused uploading; NULL = uploading
