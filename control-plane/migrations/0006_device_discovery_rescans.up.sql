CREATE TABLE device_discovery_rescan_requests (
    device_id uuid PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    requested_by_subject text NOT NULL,
    requested_at timestamptz NOT NULL
);

CREATE INDEX device_discovery_rescan_requests_organization_idx
    ON device_discovery_rescan_requests (organization_id, requested_at DESC);