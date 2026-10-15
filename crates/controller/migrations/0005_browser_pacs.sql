CREATE TABLE browser_pacs (
    device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    browser TEXT NOT NULL,
    packaging TEXT NOT NULL DEFAULT '',
    pac_url TEXT NOT NULL DEFAULT '',
    recommended BIGINT NOT NULL DEFAULT 0,
    locked BIGINT NOT NULL DEFAULT 0,
    source TEXT NOT NULL DEFAULT '',
    detail TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (device_id, browser)
);

CREATE INDEX browser_pacs_device_id_idx ON browser_pacs(device_id);

CREATE TABLE pac_fetches (
    device_id TEXT NOT NULL REFERENCES devices(id) ON DELETE CASCADE,
    url TEXT NOT NULL,
    sha256 TEXT NOT NULL DEFAULT '',
    size BIGINT NOT NULL DEFAULT 0,
    error TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (device_id, url)
);

CREATE INDEX pac_fetches_device_id_idx ON pac_fetches(device_id);
