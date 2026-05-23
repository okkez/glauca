-- Add requested_reviewers column to items table.
-- Stored as a JSON array of login strings, e.g. '["alice","bob"]'.
ALTER TABLE items ADD COLUMN requested_reviewers TEXT NOT NULL DEFAULT '[]';
