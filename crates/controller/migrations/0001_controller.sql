CREATE TABLE devices (
    id TEXT PRIMARY KEY,
    hostname TEXT NOT NULL,
    os TEXT NOT NULL DEFAULT '',
    architecture TEXT NOT NULL DEFAULT '',
    agent_version TEXT NOT NULL DEFAULT '',
    created_at BIGINT NOT NULL,
    last_seen_at BIGINT,
    enrolled_by_issuer TEXT NOT NULL DEFAULT '',
    enrolled_by_subject TEXT NOT NULL DEFAULT '',
    idp_claims_json TEXT
);

CREATE TABLE device_credentials (
    device_id TEXT PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
    credential_hash TEXT NOT NULL UNIQUE
);

CREATE TABLE discoveries (
    device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    version TEXT NOT NULL,
    path TEXT NOT NULL,
    mcp_servers_json TEXT NOT NULL DEFAULT '[]',
    skills_json TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (device_id, kind, path)
);

CREATE TABLE device_config_status (
    device_id TEXT PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
    revision BIGINT NOT NULL,
    state BIGINT NOT NULL,
    error TEXT NOT NULL,
    updated_at BIGINT NOT NULL
);

CREATE INDEX discoveries_device_id_idx ON discoveries(device_id);
