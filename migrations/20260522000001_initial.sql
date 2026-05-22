-- Saved search queries and their cache metadata.
CREATE TABLE IF NOT EXISTS queries (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    query       TEXT    NOT NULL UNIQUE,
    kind        TEXT    NOT NULL CHECK (kind IN ('pull_request', 'issue')),
    last_fetched_at TEXT,
    created_at  TEXT    NOT NULL DEFAULT (datetime('now'))
);

-- Cached PR / issue results linked to the query that fetched them.
CREATE TABLE IF NOT EXISTS items (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    query_id      INTEGER NOT NULL REFERENCES queries(id) ON DELETE CASCADE,
    kind          TEXT    NOT NULL CHECK (kind IN ('pull_request', 'issue')),
    repo_owner    TEXT    NOT NULL,
    repo_name     TEXT    NOT NULL,
    number        INTEGER NOT NULL,
    title         TEXT    NOT NULL,
    url           TEXT    NOT NULL,
    author        TEXT,
    state         TEXT    NOT NULL,
    updated_at    TEXT    NOT NULL,
    labels        TEXT    NOT NULL DEFAULT '[]',
    comment_count INTEGER NOT NULL DEFAULT 0,
    cached_at     TEXT    NOT NULL DEFAULT (datetime('now')),
    UNIQUE (query_id, repo_owner, repo_name, number)
);

CREATE INDEX IF NOT EXISTS idx_items_query_id  ON items (query_id);
CREATE INDEX IF NOT EXISTS idx_items_state     ON items (state);
CREATE INDEX IF NOT EXISTS idx_items_updated   ON items (updated_at DESC);
