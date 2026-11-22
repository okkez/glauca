-- The merge precheck reads mergeStateStatus straight from GitHub; nothing read the cached copy.
ALTER TABLE items DROP COLUMN merge_state_status;
