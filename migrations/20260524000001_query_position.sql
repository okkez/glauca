-- Add an explicit sort position to queries so users can reorder them.
ALTER TABLE queries ADD COLUMN position INTEGER NOT NULL DEFAULT 0;

-- Initialise positions from the existing creation order so existing data is stable.
UPDATE queries
SET position = (
    SELECT COUNT(*)
    FROM queries q2
    WHERE q2.rowid < queries.rowid
);
