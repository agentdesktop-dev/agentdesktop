//! Writable fleet configuration stored in the controller database.

use std::{sync::Arc, time::Duration};

use agentdesktop_core::config::ControllerDaemonDatabaseConfig;
use anyhow::Context;
use async_trait::async_trait;
use tokio::time::MissedTickBehavior;
use tracing::{error, info, warn};

use crate::database::{Database, FleetConfigurationRecord, FleetConfigurationReplacement};

use super::{
    DaemonConfigStore, FleetConfigurationSnapshot, FleetConfigurationSource,
    ReplaceFleetConfigurationError, compile, validate_size, validate_writable_yaml,
};

const DEFAULT_FLEET_CONFIGURATION: &str = "programs: {}\n";
const RELOAD_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub(super) struct DatabaseConfigurationSource {
    database: Database,
}

impl DatabaseConfigurationSource {
    pub(super) async fn connect(
        database: Database,
        config: &ControllerDaemonDatabaseConfig,
    ) -> anyhow::Result<Self> {
        if database.fleet_configuration().await?.is_none() {
            let yaml = match &config.seed_path {
                Some(path) => std::fs::read_to_string(path).with_context(|| {
                    format!("read fleet configuration seed from {}", path.display())
                })?,
                None => DEFAULT_FLEET_CONFIGURATION.to_owned(),
            };
            validate_writable_yaml(yaml.as_bytes()).context("validate fleet configuration seed")?;
            database
                .initialize_fleet_configuration(yaml, config.seed_revision)
                .await?;
        }
        Ok(Self { database })
    }

    async fn snapshot_from_database(&self) -> anyhow::Result<FleetConfigurationSnapshot> {
        let record = self
            .database
            .fleet_configuration()
            .await?
            .context("fleet configuration is not initialized")?;
        record_snapshot(record)
    }

    async fn watch(self: Arc<Self>, store: DaemonConfigStore) {
        let mut interval = tokio::time::interval(RELOAD_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            match self.snapshot_from_database().await {
                Ok(snapshot) => match store.publish_monotonic(snapshot.daemon) {
                    Ok(true) => info!("reloaded fleet configuration from database"),
                    Ok(false) => {}
                    Err(error) => {
                        store.record_error(&error);
                        warn!(
                            error = %error,
                            "ignored conflicting fleet configuration revision from database"
                        );
                    }
                },
                Err(error) => {
                    store.record_error(&error);
                    error!(
                        error = %format!("{error:#}"),
                        "failed to reload fleet configuration; retaining last good configuration"
                    );
                }
            }
        }
    }
}

#[async_trait]
impl FleetConfigurationSource for DatabaseConfigurationSource {
    async fn snapshot(&self) -> anyhow::Result<FleetConfigurationSnapshot> {
        self.snapshot_from_database().await
    }

    async fn replace(
        &self,
        expected_version: &str,
        yaml: String,
    ) -> Result<FleetConfigurationSnapshot, ReplaceFleetConfigurationError> {
        validate_writable_yaml(yaml.as_bytes()).map_err(ReplaceFleetConfigurationError::Invalid)?;
        let expected_revision = expected_version
            .parse::<u64>()
            .map_err(|_| ReplaceFleetConfigurationError::Conflict)?;
        match self
            .database
            .replace_fleet_configuration(expected_revision, yaml)
            .await
            .map_err(ReplaceFleetConfigurationError::Backend)?
        {
            FleetConfigurationReplacement::Unchanged(record)
            | FleetConfigurationReplacement::Replaced(record) => {
                record_snapshot(record).map_err(ReplaceFleetConfigurationError::Backend)
            }
            FleetConfigurationReplacement::Conflict => {
                Err(ReplaceFleetConfigurationError::Conflict)
            }
        }
    }

    fn start_watch(self: Arc<Self>, store: DaemonConfigStore) -> anyhow::Result<()> {
        tokio::spawn(self.watch(store));
        Ok(())
    }

    fn writable(&self) -> bool {
        true
    }

    fn kind(&self) -> &'static str {
        "database"
    }
}

fn record_snapshot(record: FleetConfigurationRecord) -> anyhow::Result<FleetConfigurationSnapshot> {
    validate_size(record.yaml.as_bytes())?;
    Ok(FleetConfigurationSnapshot {
        version: record.revision.to_string(),
        daemon: compile(record.yaml.into_bytes(), record.revision)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn seeds_and_replaces_database_configuration() {
        let temporary_directory = std::env::temp_dir();
        let database_path = temporary_directory.join(format!(
            "agentdesktop-database-source-test-{}.db",
            std::process::id()
        ));
        let seed_path = temporary_directory.join(format!(
            "agentdesktop-database-source-seed-{}.yaml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&database_path);
        std::fs::write(&seed_path, "programs: {}\n").expect("write fleet configuration seed");
        let database = Database::connect(&format!("sqlite://{}?mode=rwc", database_path.display()))
            .await
            .expect("connect test database");
        let source = DatabaseConfigurationSource::connect(
            database.clone(),
            &ControllerDaemonDatabaseConfig {
                seed_path: Some(seed_path.clone()),
                seed_revision: 7,
            },
        )
        .await
        .expect("connect database configuration source");

        let initial = source.snapshot().await.expect("read seeded configuration");
        assert_eq!(initial.daemon.revision, 7);
        assert_eq!(initial.version, "7");
        assert_eq!(initial.daemon.yaml, b"programs: {}\n");
        assert!(matches!(
            source
                .replace("invalid", "programs:\n  codex: {}\n".to_owned())
                .await,
            Err(ReplaceFleetConfigurationError::Conflict)
        ));

        let replacement = source
            .replace("7", "programs:\n  claudeCode: {}\n".to_owned())
            .await
            .expect("replace database configuration");
        assert_eq!(replacement.daemon.revision, 8);
        assert_eq!(replacement.version, "8");
        assert!(matches!(
            source
                .replace("7", "programs:\n  codex: {}\n".to_owned())
                .await,
            Err(ReplaceFleetConfigurationError::Conflict)
        ));

        drop(source);
        drop(database);
        let _ = std::fs::remove_file(database_path);
        let _ = std::fs::remove_file(seed_path);
    }

    #[test]
    fn rejects_oversized_database_configuration() {
        let error = record_snapshot(FleetConfigurationRecord {
            yaml: " ".repeat(super::super::MAX_DAEMON_CONFIG_BYTES + 1),
            revision: 1,
        })
        .expect_err("reject oversized fleet configuration");
        assert!(error.to_string().contains("exceeds the maximum size"));
    }
}
