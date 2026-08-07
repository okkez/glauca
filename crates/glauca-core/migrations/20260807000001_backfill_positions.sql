-- Give every existing query and filter stream a position of its own.
--
-- `position` arrived with a `DEFAULT 0` and, until now, was never assigned on insert:
-- the two migrations that added the column backfilled the rows that existed then, and
-- every row created since sits at 0. Reordering used to exchange two rows' positions,
-- which for a table of zeros wrote 0 back over itself and lost the reorder; it renumbers
-- now, so it no longer needs the stored values to be distinct. What still does is the
-- order the list comes back in: while positions tie, `ORDER BY position, created_at, id`
-- settles it by creation time — an order nobody chose.
--
-- Renumbering is in exactly the order the left pane already displays, so nothing the
-- user can see moves: rows that do carry distinct positions (an order someone chose
-- before this went unnoticed) keep it, and the ties behind them are broken the same way
-- the queries break them. Inserts now assign the next position and a reorder renumbers
-- what it touches, so this runs once and the invariant holds from here on.

WITH ranked AS (
    SELECT id, ROW_NUMBER() OVER (ORDER BY position, created_at, id) - 1 AS pos
    FROM queries
)
UPDATE queries
SET position = ranked.pos
FROM ranked
WHERE queries.id = ranked.id;

-- Filter streams are ordered within their parent query, so they are numbered per parent.
WITH ranked AS (
    SELECT
        id,
        ROW_NUMBER() OVER (PARTITION BY parent_id ORDER BY position, created_at, id) - 1 AS pos
    FROM filter_streams
)
UPDATE filter_streams
SET position = ranked.pos
FROM ranked
WHERE filter_streams.id = ranked.id;
