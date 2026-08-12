CREATE TABLE device_discovery_reports (
    device_id uuid PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    user_id uuid NOT NULL REFERENCES users(id),
    certificate_serial_number text NOT NULL REFERENCES certificates(serial_number),
    schema_version smallint NOT NULL,
    report jsonb NOT NULL,
    received_at timestamptz NOT NULL
);

CREATE INDEX device_discovery_reports_organization_idx
    ON device_discovery_reports (organization_id, received_at DESC);