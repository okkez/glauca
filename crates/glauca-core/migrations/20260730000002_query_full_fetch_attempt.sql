-- Split "a full walk finished" from "a full walk was attempted".
--
-- `last_full_fetch_at` carried both: `db::mark_fetched` stamped it on a completed
-- walk, and `db::mark_full_fetch_attempted` stamped it on a *failed* one so a query
-- that reliably errors mid-walk doesn't re-page on every single sync. The overload
-- leaks into the prune concurrency guard, which compares the value read before a walk
-- with the value re-read inside `db::prune_missing_items`: a *different* walk of the
-- same query failing in between moves the stamp, so a walk that did succeed skips its
-- prune and reports "a concurrent full fetch finished first". Safe, but false.
--
-- After this migration:
--   * `last_full_fetch_at`         — last *completed* full walk. Written only by
--     `mark_fetched(.., true)`. Sole input to the prune guard.
--   * `last_full_fetch_attempt_at` — last full walk *attempted*, success or failure.
--     Written by both writers, so it is always the later of the two. Sole input to
--     `is_full_fetch_due`.
-- which restores the `last_full_fetch_at <= last_fetched_at` invariant.
--
-- Backfilled from the old column so no existing cache is suddenly due for a full
-- walk. Where that old value came from a failure it now also sits in the completion
-- column, claiming a completion that never happened; the guard uses it only as a
-- change detector, and the first completed walk overwrites it.
ALTER TABLE queries ADD COLUMN last_full_fetch_attempt_at TEXT;

UPDATE queries
SET last_full_fetch_attempt_at = last_full_fetch_at
WHERE last_full_fetch_at IS NOT NULL;
