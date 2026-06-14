-- Per-item read flag. Drives the unread badge together with the "new since"
-- check (unread = new AND not read). New items default to unread (0); the flag is
-- preserved across re-syncs (see upsert_item, which omits `read` from DO UPDATE).
ALTER TABLE items ADD COLUMN read INTEGER NOT NULL DEFAULT 0;
