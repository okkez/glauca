-- Redefine "unread" as update-driven (Jasper-style): an item is unread iff its
-- current `updated_at` is newer than the `updated_at` the user had seen when they
-- last read it. This replaces the previous "new since the query was last viewed"
-- (cached_at > last_viewed_at) plus sticky read-flag model.
ALTER TABLE items ADD COLUMN last_read_updated_at TEXT;

-- Backfill: previously-read items are treated as read at their current
-- `updated_at`, so they stay read until a later sync advances `updated_at`.
UPDATE items SET last_read_updated_at = updated_at WHERE read = 1;

-- Drop the columns of the old model. None are referenced by an index or the
-- UNIQUE constraint, so SQLite (>= 3.35) can drop them in place.
ALTER TABLE items DROP COLUMN read;
ALTER TABLE items DROP COLUMN cached_at;
ALTER TABLE queries DROP COLUMN last_viewed_at;
ALTER TABLE filter_streams DROP COLUMN last_viewed_at;
