CREATE TABLE daemon_config_revisions (
    revision BIGINT PRIMARY KEY CHECK (revision > 0),
    yaml TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    author_kind TEXT NOT NULL,
    author_issuer TEXT NOT NULL,
    author_subject TEXT NOT NULL,
    author_client_id TEXT,
    author_display TEXT,
    message TEXT NOT NULL DEFAULT ''
);
