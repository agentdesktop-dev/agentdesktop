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
    status text NOT NULL CHECK (status IN ('pending', 'approved', 'rejected')),
    csr_der bytea NOT NULL,
    public_key_fingerprint text NOT NULL,
    created_at timestamptz NOT NULL,
    updated_at timestamptz NOT NULL,
    CHECK ((status = 'approved') = (device_id IS NOT NULL))
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
    not_before timestamptz NOT NULL,
    not_after timestamptz NOT NULL,
    revoked_at timestamptz,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE audit_events (
    id uuid PRIMARY KEY,
    organization_id uuid NOT NULL REFERENCES organizations(id),
    actor_subject text NOT NULL,
    action text NOT NULL,
    target_id uuid,
    occurred_at timestamptz NOT NULL DEFAULT now()
);