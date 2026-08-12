ALTER TABLE enrollments
    ADD COLUMN device_name text
    CHECK (device_name IS NULL OR (char_length(device_name) BETWEEN 1 AND 128));

ALTER TABLE devices
    ADD COLUMN device_name text
    CHECK (device_name IS NULL OR (char_length(device_name) BETWEEN 1 AND 128));
