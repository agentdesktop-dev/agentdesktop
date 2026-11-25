-- Append-only management audit log (SQLite). The triggers reject updates and
-- deletes. `seq` orders events for paging: as an INTEGER PRIMARY KEY it is the
-- rowid, which SQLite assigns on insert.
CREATE TABLE admin_audit_events (
    seq INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    time_ms BIGINT NOT NULL,
    event TEXT NOT NULL,
    actor_subject TEXT NOT NULL,
    action TEXT,
    target_id TEXT,
    outcome TEXT NOT NULL,
    event_json TEXT NOT NULL
);

CREATE INDEX admin_audit_events_time_idx ON admin_audit_events(time_ms);

CREATE TRIGGER admin_audit_events_no_update
BEFORE UPDATE ON admin_audit_events
BEGIN SELECT RAISE(ABORT, 'admin_audit_events is append-only'); END;

CREATE TRIGGER admin_audit_events_no_delete
BEFORE DELETE ON admin_audit_events
BEGIN SELECT RAISE(ABORT, 'admin_audit_events is append-only'); END;
