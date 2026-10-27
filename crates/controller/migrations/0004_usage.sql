-- LLM request usage exported by agentgateway as OTLP access logs.

-- Identifiers of stored records, kept for a few days so a re-sent record is
-- not counted twice.
CREATE TABLE usage_receipts (
    id TEXT PRIMARY KEY,
    received_at BIGINT NOT NULL
);

CREATE INDEX usage_receipts_received_at_idx ON usage_receipts(received_at);

-- Request measures summed per hour (bucket_seconds 3600) and, after 90 days,
-- per UTC day (86400). Rows are not tied to devices: usage outlives a
-- deleted device, and device_id is empty when the gateway did not say.
CREATE TABLE usage_rollups (
    bucket_start BIGINT NOT NULL,
    bucket_seconds BIGINT NOT NULL,
    subject TEXT NOT NULL,
    device_id TEXT NOT NULL,
    client_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    billing TEXT NOT NULL,
    requests BIGINT NOT NULL,
    errors BIGINT NOT NULL,
    input_tokens BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    cache_read_tokens BIGINT NOT NULL,
    cache_write_tokens BIGINT NOT NULL,
    cost_nanos BIGINT NOT NULL,
    unpriced_requests BIGINT NOT NULL,
    PRIMARY KEY (bucket_start, bucket_seconds, subject, device_id, client_id, provider, model, billing)
);

CREATE INDEX usage_rollups_bucket_start_idx ON usage_rollups(bucket_start);
CREATE INDEX usage_rollups_device_idx ON usage_rollups(device_id, bucket_start);
