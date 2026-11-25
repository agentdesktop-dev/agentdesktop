-- Append-only management audit log (PostgreSQL). The triggers reject updates,
-- deletes and truncation. `seq` orders events for paging; the identity column
-- assigns it on insert.
CREATE TABLE admin_audit_events (
    seq BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
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

CREATE FUNCTION admin_audit_events_append_only() RETURNS trigger
    LANGUAGE plpgsql AS $$
    BEGIN RAISE EXCEPTION 'admin_audit_events is append-only'; END
    $$;

CREATE TRIGGER admin_audit_events_no_change
    BEFORE UPDATE OR DELETE ON admin_audit_events
    FOR EACH ROW EXECUTE FUNCTION admin_audit_events_append_only();

CREATE TRIGGER admin_audit_events_no_truncate
    BEFORE TRUNCATE ON admin_audit_events
    FOR EACH STATEMENT EXECUTE FUNCTION admin_audit_events_append_only();
