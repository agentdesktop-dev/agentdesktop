ALTER TABLE certificates DROP COLUMN certificate_pem;

ALTER TABLE enrollments DROP CONSTRAINT enrollments_device_check;
ALTER TABLE enrollments DROP CONSTRAINT enrollments_status_check;

ALTER TABLE enrollments
    ADD CONSTRAINT enrollments_status_check
    CHECK (status IN ('pending', 'approved', 'rejected'));

ALTER TABLE enrollments
    ADD CONSTRAINT enrollments_check
    CHECK ((status = 'approved') = (device_id IS NOT NULL));