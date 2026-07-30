-- Drop `idx_items_query_id`: it is a strict prefix of the index SQLite builds for
-- `UNIQUE (query_id, repo_owner, repo_name, number)` (see the initial migration), so
-- every `WHERE query_id = ?` lookup — and the `ON DELETE CASCADE` from `queries`,
-- which needs an index on the child key to avoid a scan — is served by that index
-- whether this one exists or not.
--
-- What it did cost is writes. `upsert_items` commits once per page, per query, per
-- sync cycle, and `prune_missing_items` deletes in bulk; both paid an extra b-tree
-- update per row for an index nothing could ever prefer.
DROP INDEX IF EXISTS idx_items_query_id;
