-- Name of the enrollment token a device enrolled with; NULL for devices
-- enrolled by a user through OIDC.
ALTER TABLE devices ADD COLUMN enrollment_token TEXT;
