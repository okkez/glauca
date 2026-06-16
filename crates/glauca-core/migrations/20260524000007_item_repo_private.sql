-- Repository visibility flag (0=public, 1=private), surfaced as a lock indicator
-- in the item list. Refreshed on every re-sync (see upsert_item's DO UPDATE).
ALTER TABLE items ADD COLUMN repo_private INTEGER NOT NULL DEFAULT 0;
