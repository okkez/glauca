-- Filter streams: local child filters applied to a parent query's cached items.
-- No GitHub API calls are made for filter streams; they re-use the parent's cache.
CREATE TABLE IF NOT EXISTS filter_streams (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    parent_id   INTEGER NOT NULL REFERENCES queries(id) ON DELETE CASCADE,
    name        TEXT    NOT NULL,
    filter      TEXT    NOT NULL,
    created_at  TEXT    NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_filter_streams_parent ON filter_streams (parent_id);
