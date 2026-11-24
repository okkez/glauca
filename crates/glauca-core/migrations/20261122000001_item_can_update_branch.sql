-- GitHub's `viewerCanUpdateBranch`: true when the web UI would offer "Update branch".
ALTER TABLE items ADD COLUMN can_update_branch INTEGER NOT NULL DEFAULT 0;
