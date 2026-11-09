CREATE TABLE enrollment_receipts (
    device_id TEXT PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
    response_json TEXT NOT NULL
);
