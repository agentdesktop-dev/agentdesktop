ALTER TABLE enrollments DROP CONSTRAINT enrollments_status_check;
ALTER TABLE enrollments DROP CONSTRAINT enrollments_check;

ALTER TABLE enrollments
    ADD CONSTRAINT enrollments_status_check
    CHECK (status IN ('pending', 'issuing', 'approved', 'rejected'));

ALTER TABLE enrollments
    ADD CONSTRAINT enrollments_device_check
    CHECK ((status IN ('issuing', 'approved')) = (device_id IS NOT NULL));

ALTER TABLE certificates
    ADD COLUMN certificate_pem text NOT NULL;