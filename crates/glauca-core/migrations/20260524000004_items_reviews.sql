-- Store submitted reviews (JSON array: [{"login":"alice","state":"APPROVED"},...])
ALTER TABLE items ADD COLUMN reviews TEXT NOT NULL DEFAULT '[]';
