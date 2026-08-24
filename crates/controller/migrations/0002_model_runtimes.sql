CREATE TABLE model_runtimes (
    device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    models_json TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (device_id, kind)
);

CREATE INDEX model_runtimes_device_id_idx ON model_runtimes(device_id);
