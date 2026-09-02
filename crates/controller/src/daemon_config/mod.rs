//! Fleet configuration lifecycle: sources, the live store, and admin operations.

use std::{
    fmt,
    sync::{Arc, RwLock},
};

use agentdesktop_core::config::ControllerDaemonConfig;
use agentdesktop_proto::fleet::DaemonConfig;
use anyhow::Context;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

mod database;
mod file;

const MAX_DAEMON_CONFIG_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
pub struct FleetConfigurationSnapshot {
    pub daemon: DaemonConfig,
    pub version: String,
}

#[derive(Debug)]
pub enum ReplaceFleetConfigurationError {
    ReadOnly,
    Conflict,
    Invalid(anyhow::Error),
    Backend(anyhow::Error),
}

impl fmt::Display for ReplaceFleetConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ReadOnly => formatter.write_str("fleet configuration source is read-only"),
            Self::Conflict => formatter.write_str("fleet configuration changed; reload and retry"),
            Self::Invalid(error) | Self::Backend(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for ReplaceFleetConfigurationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Invalid(error) | Self::Backend(error) => Some(error.as_ref()),
            Self::ReadOnly | Self::Conflict => None,
        }
    }
}

#[async_trait]
pub trait FleetConfigurationSource: Send + Sync {
    async fn snapshot(&self) -> anyhow::Result<FleetConfigurationSnapshot>;

    async fn replace(
        &self,
        expected_version: &str,
        yaml: String,
    ) -> Result<FleetConfigurationSnapshot, ReplaceFleetConfigurationError>;

    fn start_watch(self: Arc<Self>, store: DaemonConfigStore) -> anyhow::Result<()>;

    fn writable(&self) -> bool;

    fn kind(&self) -> &'static str;
}

async fn open_source(
    config: &ControllerDaemonConfig,
    controller_database: &crate::database::Database,
) -> anyhow::Result<Arc<dyn FleetConfigurationSource>> {
    match config {
        ControllerDaemonConfig::File(config) => {
            Ok(Arc::new(file::FileConfigurationSource::new(config.clone())))
        }
        ControllerDaemonConfig::Database { database } => Ok(Arc::new(
            database::DatabaseConfigurationSource::connect(controller_database.clone(), database)
                .await?,
        )),
    }
}

/// Fleet configuration source paired with the live store it publishes into.
#[derive(Clone)]
pub struct FleetConfiguration {
    store: DaemonConfigStore,
    source: Option<Arc<dyn FleetConfigurationSource>>,
}

/// Fleet configuration as currently served to administrators.
pub struct ActiveFleetConfiguration {
    pub daemon: Option<DaemonConfig>,
    pub version: Option<String>,
    pub source_error: Option<String>,
}

impl FleetConfiguration {
    pub fn new(
        store: DaemonConfigStore,
        source: Option<Arc<dyn FleetConfigurationSource>>,
    ) -> Self {
        Self { store, source }
    }

    /// Opens the configured source, loads the initial configuration, and starts its watch.
    pub async fn open(
        definition: Option<&ControllerDaemonConfig>,
        database: &crate::database::Database,
    ) -> anyhow::Result<Self> {
        let source = match definition {
            Some(definition) => Some(open_source(definition, database).await?),
            None => None,
        };
        let initial = match &source {
            Some(source) => Some(source.snapshot().await?.daemon),
            None => None,
        };
        let store = DaemonConfigStore::new(initial);
        if let Some(source) = source.clone() {
            source.start_watch(store.clone())?;
        }
        Ok(Self { store, source })
    }

    pub fn store(&self) -> &DaemonConfigStore {
        &self.store
    }

    pub fn writable(&self) -> bool {
        self.source
            .as_deref()
            .is_some_and(FleetConfigurationSource::writable)
    }

    pub fn kind(&self) -> Option<&'static str> {
        self.source.as_deref().map(FleetConfigurationSource::kind)
    }

    /// Reads the source, falling back to the last good configuration on failure.
    pub async fn resolve(&self) -> ActiveFleetConfiguration {
        let Some(source) = self.source.as_deref() else {
            return ActiveFleetConfiguration {
                daemon: None,
                version: None,
                source_error: None,
            };
        };
        let result = source.snapshot().await.and_then(|snapshot| {
            if source.writable() {
                self.store.publish_monotonic(snapshot.daemon.clone())?;
            } else {
                self.store.publish(snapshot.daemon.clone());
            }
            Ok(snapshot)
        });
        match result {
            Ok(snapshot) => ActiveFleetConfiguration {
                daemon: Some(snapshot.daemon),
                version: Some(snapshot.version),
                source_error: None,
            },
            Err(error) => ActiveFleetConfiguration {
                daemon: self.store.current(),
                version: None,
                source_error: Some(format!("{error:#}")),
            },
        }
    }

    /// Replaces the source configuration and publishes the accepted revision.
    pub async fn replace(
        &self,
        expected_version: &str,
        yaml: String,
    ) -> Result<FleetConfigurationSnapshot, ReplaceFleetConfigurationError> {
        let source = self
            .source
            .as_deref()
            .ok_or(ReplaceFleetConfigurationError::ReadOnly)?;
        let snapshot = source.replace(expected_version, yaml).await?;
        self.store
            .publish_monotonic(snapshot.daemon.clone())
            .map_err(ReplaceFleetConfigurationError::Backend)?;
        Ok(snapshot)
    }
}

/// Live, last-known-good daemon configuration shared by controller surfaces.
#[derive(Clone)]
pub struct DaemonConfigStore {
    sender: watch::Sender<Option<DaemonConfig>>,
    reload_error: Arc<RwLock<Option<String>>>,
}

impl DaemonConfigStore {
    pub fn new(initial: Option<DaemonConfig>) -> Self {
        let (sender, _) = watch::channel(initial);
        Self {
            sender,
            reload_error: Arc::new(RwLock::new(None)),
        }
    }

    pub fn current(&self) -> Option<DaemonConfig> {
        self.sender.borrow().clone()
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<DaemonConfig>> {
        self.sender.subscribe()
    }

    pub fn reload_error(&self) -> Option<String> {
        self.reload_error
            .read()
            .expect("daemon-config reload status lock poisoned")
            .clone()
    }

    /// Publishes a file-sourced configuration; content may change at a fixed revision.
    fn publish(&self, next: DaemonConfig) -> bool {
        let changed = self.sender.send_if_modified(|current| {
            if current.as_ref().is_some_and(|current| {
                current.revision == next.revision && current.sha256 == next.sha256
            }) {
                return false;
            }
            *current = Some(next.clone());
            true
        });
        *self
            .reload_error
            .write()
            .expect("daemon-config reload status lock poisoned") = None;
        changed
    }

    /// Publishes only strictly newer revisions; stale or reused revisions are rejected.
    fn publish_monotonic(&self, next: DaemonConfig) -> anyhow::Result<bool> {
        let mut rejection = None;
        let changed = self.sender.send_if_modified(|current| {
            if let Some(current) = current.as_ref() {
                if next.revision < current.revision {
                    rejection = Some(format!(
                        "fleet configuration revision {} is older than active revision {}",
                        next.revision, current.revision
                    ));
                    return false;
                }
                if next.revision == current.revision {
                    if next.sha256 != current.sha256 {
                        rejection = Some(format!(
                            "fleet configuration revision {} cannot be reused with different content",
                            next.revision
                        ));
                    }
                    return false;
                }
            }
            *current = Some(next.clone());
            true
        });
        if let Some(rejection) = rejection {
            anyhow::bail!(rejection);
        }
        *self
            .reload_error
            .write()
            .expect("daemon-config reload status lock poisoned") = None;
        Ok(changed)
    }

    fn record_error(&self, error: &anyhow::Error) {
        *self
            .reload_error
            .write()
            .expect("daemon-config reload status lock poisoned") = Some(format!("{error:#}"));
    }
}

fn compile(yaml: Vec<u8>, revision: u64) -> anyhow::Result<DaemonConfig> {
    if revision == 0 {
        anyhow::bail!("fleet configuration revision must be greater than zero");
    }
    validate_yaml(&yaml)?;
    let sha256 = Sha256::digest(&yaml).to_vec();
    Ok(DaemonConfig {
        revision,
        yaml,
        sha256,
    })
}

fn validate_yaml(yaml: &[u8]) -> anyhow::Result<()> {
    let text = std::str::from_utf8(yaml).context("daemon configuration is not UTF-8")?;
    agentdesktop_core::config::parse_daemon(text).context("validate daemon configuration")?;
    Ok(())
}

fn validate_size(yaml: &[u8]) -> anyhow::Result<()> {
    if yaml.len() > MAX_DAEMON_CONFIG_BYTES {
        anyhow::bail!(
            "fleet configuration exceeds the maximum size of {MAX_DAEMON_CONFIG_BYTES} bytes"
        );
    }
    Ok(())
}

fn validate_writable_yaml(yaml: &[u8]) -> anyhow::Result<()> {
    validate_size(yaml)?;
    validate_yaml(yaml)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn publish_monotonic_rejects_stale_and_reused_revisions() {
        let current = compile(b"programs: {}\n".to_vec(), 4).expect("current config");
        let store = DaemonConfigStore::new(Some(current.clone()));

        let stale = compile(b"programs:\n  codex: {}\n".to_vec(), 3).expect("stale config");
        assert!(store.publish_monotonic(stale).is_err());

        let reused =
            compile(b"programs:\n  claudeCode: {}\n".to_vec(), 4).expect("reused revision");
        assert!(store.publish_monotonic(reused).is_err());

        assert!(
            !store
                .publish_monotonic(current.clone())
                .expect("duplicate event")
        );

        let active = store.current().expect("active config");
        assert_eq!(active.revision, 4);
        assert_eq!(active.sha256, current.sha256);
    }

    struct TestConfigurationSource {
        snapshot: Mutex<FleetConfigurationSnapshot>,
        writable: bool,
    }

    #[async_trait]
    impl FleetConfigurationSource for TestConfigurationSource {
        async fn snapshot(&self) -> anyhow::Result<FleetConfigurationSnapshot> {
            Ok(self.snapshot.lock().expect("snapshot lock").clone())
        }

        async fn replace(
            &self,
            expected_version: &str,
            yaml: String,
        ) -> Result<FleetConfigurationSnapshot, ReplaceFleetConfigurationError> {
            let mut current = self.snapshot.lock().expect("snapshot lock");
            if current.version != expected_version {
                return Err(ReplaceFleetConfigurationError::Conflict);
            }
            let yaml = yaml.into_bytes();
            let snapshot = FleetConfigurationSnapshot {
                daemon: DaemonConfig {
                    revision: current.daemon.revision + 1,
                    sha256: Sha256::digest(&yaml).to_vec(),
                    yaml,
                },
                version: "2".to_owned(),
            };
            *current = snapshot.clone();
            Ok(snapshot)
        }

        fn start_watch(self: Arc<Self>, _store: DaemonConfigStore) -> anyhow::Result<()> {
            Ok(())
        }

        fn writable(&self) -> bool {
            self.writable
        }

        fn kind(&self) -> &'static str {
            if self.writable { "database" } else { "file" }
        }
    }

    #[tokio::test]
    async fn replacing_configuration_publishes_the_new_revision() {
        let initial_yaml = b"programs: {}\n".to_vec();
        let source: Arc<dyn FleetConfigurationSource> = Arc::new(TestConfigurationSource {
            snapshot: Mutex::new(FleetConfigurationSnapshot {
                daemon: DaemonConfig {
                    revision: 1,
                    sha256: Sha256::digest(&initial_yaml).to_vec(),
                    yaml: initial_yaml.clone(),
                },
                version: "1".to_owned(),
            }),
            writable: true,
        });
        let store = DaemonConfigStore::new(Some(DaemonConfig {
            revision: 1,
            sha256: Sha256::digest(&initial_yaml).to_vec(),
            yaml: initial_yaml,
        }));
        let fleet = FleetConfiguration::new(store.clone(), Some(source));

        let snapshot = fleet
            .replace("1", "programs:\n  claudeCode: {}\n".to_owned())
            .await
            .expect("replace configuration");

        assert_eq!(snapshot.daemon.revision, 2);
        assert_eq!(snapshot.version, "2");
        assert_eq!(store.current().expect("published config").revision, 2);
    }

    #[tokio::test]
    async fn resolving_file_configuration_repairs_a_missed_watch_event() {
        let initial = compile(b"programs: {}\n".to_vec(), 7).expect("initial config");
        let replacement =
            compile(b"programs:\n  codex: {}\n".to_vec(), 7).expect("replacement config");
        let source: Arc<dyn FleetConfigurationSource> = Arc::new(TestConfigurationSource {
            snapshot: Mutex::new(FleetConfigurationSnapshot {
                daemon: replacement.clone(),
                version: "file:7".to_owned(),
            }),
            writable: false,
        });
        let store = DaemonConfigStore::new(Some(initial));
        let fleet = FleetConfiguration::new(store.clone(), Some(source));

        let active = fleet.resolve().await;

        assert_eq!(
            active.daemon.expect("active config").sha256,
            replacement.sha256
        );
        assert_eq!(
            store.current().expect("repaired config").sha256,
            replacement.sha256
        );
    }
}
