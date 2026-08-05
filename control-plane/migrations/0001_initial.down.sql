DROP TABLE audit_events;
DROP TABLE certificate_recovery_challenges;
DROP TABLE certificate_renewals;
ALTER TABLE devices DROP CONSTRAINT devices_current_certificate_fk;
DROP TABLE certificates;
DROP TABLE enrollments;
DROP TABLE devices;
DROP TABLE users;
DROP TABLE organizations;