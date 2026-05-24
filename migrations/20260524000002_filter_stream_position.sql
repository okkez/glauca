-- Add an explicit sort position to filter_streams so users can reorder them
-- within their parent query group.
ALTER TABLE filter_streams ADD COLUMN position INTEGER NOT NULL DEFAULT 0;

-- Initialise positions from the existing creation order within each parent.
UPDATE filter_streams
SET position = (
    SELECT COUNT(*)
    FROM filter_streams f2
    WHERE f2.parent_id = filter_streams.parent_id
      AND f2.rowid < filter_streams.rowid
);
