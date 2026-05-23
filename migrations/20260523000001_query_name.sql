-- Add a display name column to root queries.
-- When NULL the query string itself is used as the display label.
ALTER TABLE queries ADD COLUMN name TEXT;
