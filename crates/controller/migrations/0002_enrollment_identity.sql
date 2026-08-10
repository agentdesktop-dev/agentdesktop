ALTER TABLE devices ADD COLUMN enrolled_by_issuer TEXT NOT NULL DEFAULT '';
ALTER TABLE devices ADD COLUMN enrolled_by_subject TEXT NOT NULL DEFAULT '';
