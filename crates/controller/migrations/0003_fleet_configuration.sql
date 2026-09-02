CREATE TABLE fleet_configuration (
    singleton BIGINT PRIMARY KEY CHECK (singleton = 1),
    yaml TEXT NOT NULL,
    revision BIGINT NOT NULL CHECK (revision > 0)
);