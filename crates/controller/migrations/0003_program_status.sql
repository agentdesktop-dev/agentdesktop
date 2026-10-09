CREATE TABLE device_program_status (
    device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    program TEXT NOT NULL,
    position BIGINT NOT NULL,
    revision BIGINT NOT NULL,
    state BIGINT NOT NULL,
    detail TEXT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (device_id, program)
);

CREATE INDEX device_program_status_device_id_idx ON device_program_status(device_id);

ALTER TABLE device_config_status ADD COLUMN programs_reported BIGINT NOT NULL DEFAULT 0;
