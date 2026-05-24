-- Extend cached items with additional metadata for the detail pane.
ALTER TABLE items ADD COLUMN body TEXT;
ALTER TABLE items ADD COLUMN assignees TEXT NOT NULL DEFAULT '[]';
ALTER TABLE items ADD COLUMN is_draft INTEGER NOT NULL DEFAULT 0;
ALTER TABLE items ADD COLUMN created_at_item TEXT;
ALTER TABLE items ADD COLUMN base_ref TEXT;
ALTER TABLE items ADD COLUMN head_ref TEXT;
ALTER TABLE items ADD COLUMN review_decision TEXT;
ALTER TABLE items ADD COLUMN milestone TEXT;
