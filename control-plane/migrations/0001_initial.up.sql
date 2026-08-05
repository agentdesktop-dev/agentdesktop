CREATE TABLE organizations (
    id uuid PRIMARY KEY,
    issuer text NOT NULL UNIQUE,
    display_name text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE users (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    subject text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (organization_id, subject)
);

CREATE TABLE devices (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    status text NOT NULL CHECK (status IN ('active', 'revoked')),
    created_at timestamptz NOT NULL DEFAULT now(),
    revoked_at timestamptz
);

CREATE TABLE enrollments (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    user_id uuid NOT NULL REFERENCES users(id),
    device_id uuid REFERENCES devices(id),
    status text NOT NULL CHECK (status IN ('pending', 'issuing', 'approved', 'rejected')),
    csr_der bytea NOT NULL,
    public_key_fingerprint text NOT NULL,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    CHECK ((status IN ('issuing', 'approved')) = (device_id IS NOT NULL))
);

CREATE INDEX enrollments_user_created_idx ON enrollments (user_id, created_at DESC);
CREATE INDEX enrollments_key_idx ON enrollments (organization_id, public_key_fingerprint);
CREATE UNIQUE INDEX enrollments_pending_key_idx
    ON enrollments (organization_id, user_id, public_key_fingerprint)
    WHERE status = 'pending';

CREATE TABLE certificates (
    serial_number text PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    device_id uuid NOT NULL REFERENCES devices(id),
    public_key_fingerprint text NOT NULL,
    certificate_pem text NOT NULL,
    not_before timestamptz NOT NULL,
    not_after timestamptz NOT NULL,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE certificate_renewals (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    user_id uuid NOT NULL REFERENCES users(id),
    device_id uuid NOT NULL REFERENCES devices(id),
    presented_serial_number text NOT NULL REFERENCES certificates(serial_number),
    certificate_serial_number text REFERENCES certificates(serial_number),
    status text NOT NULL CHECK (status IN ('issuing', 'approved')),
    csr_der bytea NOT NULL,
    public_key_fingerprint text NOT NULL,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    UNIQUE (device_id, public_key_fingerprint),
    CHECK ((status = 'approved') = (certificate_serial_number IS NOT NULL))
);

CREATE INDEX certificate_renewals_issuing_idx
    ON certificate_renewals (updated_at, id) WHERE status = 'issuing';

CREATE TABLE audit_events (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    actor_subject text NOT NULL,
    action text NOT NULL,
    target_id uuid,
    occurred_at timestamptz NOT NULL DEFAULT now()
);