-- Give every existing query and filter stream a position of its own.
--
-- `position` was never assigned on insert until now, so every row created after the two
-- migrations that added the column sits at the `DEFAULT 0` — an order that is only implied by
-- `ORDER BY position, created_at, id`, never recorded. Renumbering is in exactly that order, so
-- this records what the left pane already shows without moving anything.
--
-- A window function rather than the correlated `COUNT(*)` those two migrations used: they ranked
-- by `rowid`, which the update does not touch, while this ranks by the column it rewrites.

WITH ranked AS (
    SELECT id, ROW_NUMBER() OVER (ORDER BY position, created_at, id) - 1 AS new_position
    FROM queries
)
UPDATE queries
SET position = ranked.new_position
FROM ranked
WHERE queries.id = ranked.id;

-- Filter streams are ordered within their parent query, so they are numbered per parent.
WITH ranked AS (
    SELECT
        id,
        ROW_NUMBER() OVER (PARTITION BY parent_id ORDER BY position, created_at, id) - 1
            AS new_position
    FROM filter_streams
)
UPDATE filter_streams
SET position = ranked.new_position
FROM ranked
WHERE filter_streams.id = ranked.id;
