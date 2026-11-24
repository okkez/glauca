-- Filled only by a by-number re-fetch: the search query can't afford mergeStateStatus.
-- A search upsert leaves both columns as they were.
ALTER TABLE items ADD COLUMN merge_state_status TEXT;
ALTER TABLE items ADD COLUMN behind_by INTEGER;
