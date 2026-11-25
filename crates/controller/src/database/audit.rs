//! Append-only storage for management audit events.
//!
//! The controller has no code that updates or deletes audit rows, and triggers
//! from the `0007_admin_audit_*` migrations reject `UPDATE` and `DELETE` (and
//! `TRUNCATE` on PostgreSQL), so a bug, a manual query or an SQL injection
//! elsewhere cannot edit the history. The table owner can still drop the
//! triggers; the copy streamed to the SIEM is the tamper-resistant record.
//!
//! `seq` orders events for the Audit page and its API. It is unique and
//! increasing but may have gaps (PostgreSQL identity values are not reused after
//! a failed insert), so it says nothing about missing events. Gap detection is
//! the SIEM's job, using the stream counter in `crate::audit`.

use anyhow::Context;

use super::Database;

/// Indexed columns stored next to the event JSON.
pub struct AuditColumns<'a> {
    pub id: &'a str,
    pub time_ms: i64,
    pub event: &'a str,
    pub actor_subject: &'a str,
    pub action: Option<&'a str>,
    pub target_id: Option<&'a str>,
    pub outcome: &'a str,
}

/// A stored event; it serializes as the event with its `seq` added, as the
/// audit API serves it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct AuditRow {
    pub seq: i64,
    #[serde(flatten)]
    pub event: serde_json::Map<String, serde_json::Value>,
}

impl Database {
    /// Appends one event and returns its `seq`, which the database assigns.
    /// Pass the transaction of the change being recorded, so the event commits
    /// together with it.
    pub async fn append_audit_event(
        connection: &mut sqlx::AnyConnection,
        columns: &AuditColumns<'_>,
        event_json: &str,
    ) -> anyhow::Result<i64> {
        sqlx::query_scalar(
            "INSERT INTO admin_audit_events
                (id, time_ms, event, actor_subject, action, target_id, outcome, event_json)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             RETURNING seq",
        )
        .bind(columns.id)
        .bind(columns.time_ms)
        .bind(columns.event)
        .bind(columns.actor_subject)
        .bind(columns.action)
        .bind(columns.target_id)
        .bind(columns.outcome)
        .bind(event_json)
        .fetch_one(connection)
        .await
        .context("append admin audit event")
    }

    /// Newest events first, strictly older than `before` when given.
    pub async fn list_audit_events(
        &self,
        before: Option<i64>,
        limit: i64,
    ) -> anyhow::Result<Vec<AuditRow>> {
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT seq, event_json FROM admin_audit_events
             WHERE seq < $1 ORDER BY seq DESC LIMIT $2",
        )
        .bind(before.unwrap_or(i64::MAX))
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .context("list admin audit events")?;
        rows.into_iter()
            .map(|(seq, event_json)| {
                let event =
                    serde_json::from_str(&event_json).context("parse stored admin audit event")?;
                Ok(AuditRow { seq, event })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::AuditColumns;
    use crate::database::Database;

    async fn database() -> (Database, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("audit-log-{}.db", uuid::Uuid::new_v4()));
        let database = Database::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        (database, path)
    }

    fn columns(id: &str) -> AuditColumns<'_> {
        AuditColumns {
            id,
            time_ms: 1,
            event: "action.attempted",
            actor_subject: "alice",
            action: Some("devices.delete"),
            target_id: None,
            outcome: "authorized",
        }
    }

    async fn stored_seqs(database: &Database) -> Vec<i64> {
        sqlx::query_scalar("SELECT seq FROM admin_audit_events ORDER BY seq")
            .fetch_all(database.pool())
            .await
            .unwrap()
    }

    /// Appends `count` events from concurrent tasks, alternating between databases
    /// (standing in for controller replicas).
    async fn append_concurrently(databases: &[Database], count: usize) {
        let appends: Vec<_> = (0..count)
            .map(|n| {
                let database = databases[n % databases.len()].clone();
                tokio::spawn(async move {
                    let id = uuid::Uuid::new_v4().to_string();
                    Database::append_audit_event(
                        &mut *database.pool().acquire().await?,
                        &columns(&id),
                        &format!(r#"{{"n":{n}}}"#),
                    )
                    .await
                })
            })
            .collect();
        for append in appends {
            append.await.unwrap().unwrap();
        }
    }

    #[tokio::test]
    async fn appends_number_events_in_order_and_list_newest_first() {
        let (database, path) = database().await;
        assert_eq!(
            Database::append_audit_event(
                &mut database.pool().acquire().await.unwrap(),
                &columns("a"),
                r#"{"n":1}"#
            )
            .await
            .unwrap(),
            1
        );
        assert_eq!(
            Database::append_audit_event(
                &mut database.pool().acquire().await.unwrap(),
                &columns("b"),
                r#"{"n":2}"#
            )
            .await
            .unwrap(),
            2
        );
        let listed = database.list_audit_events(None, 10).await.unwrap();
        assert_eq!(listed.iter().map(|row| row.seq).collect::<Vec<_>>(), [2, 1]);
        let older = database.list_audit_events(Some(2), 10).await.unwrap();
        assert_eq!(older.len(), 1);
        assert_eq!(older[0].event["n"], 1);
        std::fs::remove_file(path).unwrap();
    }

    /// Every append gets its own `seq`, and every event is stored.
    async fn assert_unique_seqs(database: &Database, expected: usize) {
        let mut seqs = stored_seqs(database).await;
        assert_eq!(seqs.len(), expected);
        seqs.dedup();
        assert_eq!(seqs.len(), expected, "duplicate seq");
    }

    #[tokio::test]
    async fn concurrent_appends_get_unique_seqs() {
        let (database, path) = database().await;
        let replica = Database::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap();
        append_concurrently(&[database.clone(), replica], 100).await;
        assert_unique_seqs(&database, 100).await;
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn rows_cannot_be_updated_or_deleted() {
        let (database, path) = database().await;
        Database::append_audit_event(
            &mut database.pool().acquire().await.unwrap(),
            &columns("a"),
            r#"{"n":1}"#,
        )
        .await
        .unwrap();
        for statement in [
            "UPDATE admin_audit_events SET outcome = 'denied'",
            "DELETE FROM admin_audit_events",
        ] {
            let error = sqlx::query(statement)
                .execute(database.pool())
                .await
                .unwrap_err();
            assert!(error.to_string().contains("append-only"), "{error}");
        }
        let listed = database.list_audit_events(None, 10).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].event["n"], 1);
        std::fs::remove_file(path).unwrap();
    }

    /// Runs against a PostgreSQL database named by `AGENTDESKTOP_TEST_POSTGRES_URL`,
    /// e.g. `postgres://postgres:test@127.0.0.1/postgres`.
    #[tokio::test]
    #[ignore = "needs AGENTDESKTOP_TEST_POSTGRES_URL"]
    async fn postgres_log_is_append_only_with_unique_seqs() {
        let url = std::env::var("AGENTDESKTOP_TEST_POSTGRES_URL")
            .expect("AGENTDESKTOP_TEST_POSTGRES_URL names a PostgreSQL database");
        // A second connection runs the migrations again (a no-op), and stands in
        // for another replica appending to the same table.
        let replica = Database::connect(&url).await.unwrap();
        let database = Database::connect(&url).await.unwrap();
        let before = stored_seqs(&database).await;
        let id = uuid::Uuid::new_v4().to_string();
        let seq = Database::append_audit_event(
            &mut database.pool().acquire().await.unwrap(),
            &columns(&id),
            r#"{"n":1}"#,
        )
        .await
        .unwrap();
        assert!(before.iter().all(|&earlier| earlier < seq));
        for statement in [
            "UPDATE admin_audit_events SET outcome = 'denied'",
            "DELETE FROM admin_audit_events",
            "TRUNCATE admin_audit_events",
        ] {
            let error = sqlx::query(statement)
                .execute(database.pool())
                .await
                .unwrap_err();
            assert!(error.to_string().contains("append-only"), "{error}");
        }
        append_concurrently(&[database.clone(), replica], 100).await;
        assert_unique_seqs(&database, before.len() + 101).await;

        // A change and its completion commit together, or not at all.
        use crate::audit::{Actor, AuditEvent, AuditLog, ChangeError, EventKind, Outcome};
        let (audit, _) = AuditLog::in_memory(database.clone());
        let device_id = uuid::Uuid::new_v4().to_string();
        let attempt = || {
            AuditEvent::new(
                EventKind::ActionAttempted,
                Actor::device(&device_id),
                Outcome::Authorized,
            )
        };
        let count = || async { stored_seqs(&database).await.len() };
        let stored = count().await;
        let id = device_id.clone();
        audit
            .mutate(attempt(), move |mut transaction| async move {
                sqlx::query("INSERT INTO devices (id, hostname, created_at) VALUES ($1, 'h', 0)")
                    .bind(id)
                    .execute(&mut *transaction)
                    .await?;
                Ok((transaction, (), Outcome::Succeeded))
            })
            .await
            .unwrap();
        assert_eq!(count().await, stored + 1);
        assert!(database.get_device(&device_id).await.unwrap().is_some());
        let rolled_back: Result<(), ChangeError> = audit
            .mutate(attempt(), |mut transaction| async move {
                sqlx::query("DELETE FROM devices WHERE hostname = 'h'")
                    .execute(&mut *transaction)
                    .await?;
                anyhow::bail!("change failed after deleting")
            })
            .await;
        assert!(matches!(rolled_back, Err(ChangeError::Failed(_))));
        assert!(database.get_device(&device_id).await.unwrap().is_some());
        // Only the failed completion, recorded on its own.
        assert_eq!(count().await, stored + 2);
    }
}
