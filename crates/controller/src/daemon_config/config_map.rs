//! Writable fleet configuration stored in a Kubernetes ConfigMap.

use std::sync::Arc;

use agentdesktop_core::config::ControllerDaemonConfigMap;
use anyhow::Context;
use async_trait::async_trait;
use futures::{StreamExt, pin_mut};
use k8s_openapi::api::core::v1::ConfigMap;
use kube::{
    Api, Client,
    api::PostParams,
    runtime::{WatchStreamExt, watcher},
};
use tracing::{error, info, warn};

use super::{
    DaemonConfigStore, FleetConfigurationSnapshot, FleetConfigurationSource,
    ReplaceFleetConfigurationError, compile, validate_size, validate_writable_yaml,
};

#[derive(Clone)]
pub(super) struct ConfigMapConfigurationSource {
    api: Api<ConfigMap>,
    config: ControllerDaemonConfigMap,
}

#[derive(Debug)]
enum ConfigMapReplacement {
    Unchanged(FleetConfigurationSnapshot),
    Replace(Box<ConfigMap>),
}

impl ConfigMapConfigurationSource {
    pub(super) async fn connect(config: ControllerDaemonConfigMap) -> anyhow::Result<Self> {
        let client = Client::try_default()
            .await
            .context("initialize Kubernetes client for fleet configuration")?;
        Ok(Self {
            api: Api::namespaced(client, &config.namespace),
            config,
        })
    }

    async fn watch(self: Arc<Self>, store: DaemonConfigStore) {
        let field_selector = format!("metadata.name={}", self.config.name);
        let stream = watcher(
            self.api.clone(),
            watcher::Config::default().fields(&field_selector),
        )
        .default_backoff();
        pin_mut!(stream);
        while let Some(event) = stream.next().await {
            match event {
                Ok(watcher::Event::Apply(config_map) | watcher::Event::InitApply(config_map)) => {
                    match config_map_snapshot(&config_map, &self.config) {
                        Ok(snapshot) => match store.publish_monotonic(snapshot.daemon) {
                            Ok(true) => {
                                info!(
                                    namespace = self.config.namespace,
                                    name = self.config.name,
                                    version = snapshot.version,
                                    "reloaded fleet configuration from ConfigMap"
                                );
                            }
                            Ok(false) => {}
                            Err(error) => {
                                store.record_error(&error);
                                warn!(
                                    namespace = self.config.namespace,
                                    name = self.config.name,
                                    error = %error,
                                    "ignored conflicting fleet configuration revision"
                                );
                            }
                        },
                        Err(error) => {
                            store.record_error(&error);
                            error!(
                                namespace = self.config.namespace,
                                name = self.config.name,
                                error = %format!("{error:#}"),
                                "failed to reload fleet configuration; retaining last good configuration"
                            );
                        }
                    }
                }
                Ok(watcher::Event::Delete(_)) => {
                    warn!(
                        namespace = self.config.namespace,
                        name = self.config.name,
                        "fleet configuration ConfigMap was deleted; retaining last good configuration"
                    );
                }
                Ok(watcher::Event::Init | watcher::Event::InitDone) => {}
                Err(error) => {
                    warn!(
                        namespace = self.config.namespace,
                        name = self.config.name,
                        error = %error,
                        "fleet configuration watch error"
                    );
                }
            }
        }
    }
}

#[async_trait]
impl FleetConfigurationSource for ConfigMapConfigurationSource {
    async fn snapshot(&self) -> anyhow::Result<FleetConfigurationSnapshot> {
        let config_map = self.api.get(&self.config.name).await.with_context(|| {
            format!(
                "read fleet configuration ConfigMap {}/{}",
                self.config.namespace, self.config.name
            )
        })?;
        config_map_snapshot(&config_map, &self.config)
    }

    async fn replace(
        &self,
        expected_version: &str,
        yaml: String,
    ) -> Result<FleetConfigurationSnapshot, ReplaceFleetConfigurationError> {
        let config_map = self.api.get(&self.config.name).await.map_err(|error| {
            ReplaceFleetConfigurationError::Backend(anyhow::Error::new(error).context(format!(
                "read fleet configuration ConfigMap {}/{}",
                self.config.namespace, self.config.name
            )))
        })?;
        let config_map =
            match prepare_config_map_replacement(config_map, &self.config, expected_version, yaml)?
            {
                ConfigMapReplacement::Unchanged(snapshot) => return Ok(snapshot),
                ConfigMapReplacement::Replace(config_map) => config_map,
            };
        let replaced = self
            .api
            .replace(&self.config.name, &PostParams::default(), &config_map)
            .await
            .map_err(|error| match &error {
                kube::Error::Api(response) if response.code == 409 => {
                    ReplaceFleetConfigurationError::Conflict
                }
                _ => ReplaceFleetConfigurationError::Backend(anyhow::Error::new(error).context(
                    format!(
                        "replace fleet configuration ConfigMap {}/{}",
                        self.config.namespace, self.config.name
                    ),
                )),
            })?;
        config_map_snapshot(&replaced, &self.config)
            .map_err(ReplaceFleetConfigurationError::Backend)
    }

    fn start_watch(self: Arc<Self>, store: DaemonConfigStore) -> anyhow::Result<()> {
        tokio::spawn(self.watch(store));
        Ok(())
    }

    fn writable(&self) -> bool {
        true
    }

    fn kind(&self) -> &'static str {
        "configMap"
    }
}

fn prepare_config_map_replacement(
    mut config_map: ConfigMap,
    config: &ControllerDaemonConfigMap,
    expected_version: &str,
    yaml: String,
) -> Result<ConfigMapReplacement, ReplaceFleetConfigurationError> {
    validate_writable_yaml(yaml.as_bytes()).map_err(ReplaceFleetConfigurationError::Invalid)?;
    let current = config_map_snapshot(&config_map, config)
        .map_err(ReplaceFleetConfigurationError::Backend)?;
    if current.version != expected_version {
        return Err(ReplaceFleetConfigurationError::Conflict);
    }
    if current.daemon.yaml == yaml.as_bytes() {
        return Ok(ConfigMapReplacement::Unchanged(current));
    }
    let revision = current.daemon.revision.checked_add(1).ok_or_else(|| {
        ReplaceFleetConfigurationError::Backend(anyhow::anyhow!(
            "fleet configuration revision is exhausted"
        ))
    })?;
    let data = config_map.data.get_or_insert_default();
    data.insert(config.data_key.clone(), yaml);
    data.insert(config.revision_key.clone(), revision.to_string());
    Ok(ConfigMapReplacement::Replace(Box::new(config_map)))
}

fn config_map_snapshot(
    config_map: &ConfigMap,
    config: &ControllerDaemonConfigMap,
) -> anyhow::Result<FleetConfigurationSnapshot> {
    let data = config_map.data.as_ref().with_context(|| {
        format!(
            "fleet configuration ConfigMap {}/{} has no data",
            config.namespace, config.name
        )
    })?;
    let yaml = data
        .get(&config.data_key)
        .with_context(|| format!("ConfigMap data has no {} key", config.data_key))?;
    // compile() parses the YAML below; only the size needs checking here.
    validate_size(yaml.as_bytes())?;
    let revision = data
        .get(&config.revision_key)
        .with_context(|| format!("ConfigMap data has no {} key", config.revision_key))?
        .parse::<u64>()
        .with_context(|| {
            format!(
                "ConfigMap {} must be a positive integer",
                config.revision_key
            )
        })?;
    let version = config_map
        .metadata
        .resource_version
        .clone()
        .context("fleet configuration ConfigMap has no resourceVersion")?;
    Ok(FleetConfigurationSnapshot {
        daemon: compile(yaml.as_bytes().to_vec(), revision)?,
        version,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    use super::*;

    fn test_config_map() -> (ControllerDaemonConfigMap, ConfigMap) {
        let config = ControllerDaemonConfigMap::new("agentdesktop".to_owned(), "fleet".to_owned());
        let config_map = ConfigMap {
            metadata: ObjectMeta {
                resource_version: Some("17".to_owned()),
                ..Default::default()
            },
            data: Some(BTreeMap::from([
                ("daemon.yaml".to_owned(), "programs: {}\n".to_owned()),
                ("revision".to_owned(), "4".to_owned()),
            ])),
            ..Default::default()
        };
        (config, config_map)
    }

    #[test]
    fn parses_config_map_snapshot() {
        let (config, config_map) = test_config_map();

        let snapshot = config_map_snapshot(&config_map, &config).expect("valid snapshot");

        assert_eq!(snapshot.version, "17");
        assert_eq!(snapshot.daemon.revision, 4);
        assert_eq!(snapshot.daemon.yaml, b"programs: {}\n");
    }

    #[test]
    fn prepares_config_map_replacement_with_next_revision() {
        let (config, config_map) = test_config_map();

        let replacement = prepare_config_map_replacement(
            config_map,
            &config,
            "17",
            "programs:\n  claudeCode: {}\n".to_owned(),
        )
        .expect("replacement plan");
        let ConfigMapReplacement::Replace(config_map) = replacement else {
            panic!("expected a replacement");
        };

        let data = config_map.data.expect("replacement data");
        assert_eq!(data["revision"], "5");
        assert_eq!(data["daemon.yaml"], "programs:\n  claudeCode: {}\n");
        assert_eq!(config_map.metadata.resource_version.as_deref(), Some("17"));
    }

    #[test]
    fn unchanged_config_map_save_is_idempotent() {
        let (config, config_map) = test_config_map();

        let replacement =
            prepare_config_map_replacement(config_map, &config, "17", "programs: {}\n".to_owned())
                .expect("unchanged plan");
        let ConfigMapReplacement::Unchanged(snapshot) = replacement else {
            panic!("expected unchanged snapshot");
        };

        assert_eq!(snapshot.daemon.revision, 4);
        assert_eq!(snapshot.version, "17");
    }

    #[test]
    fn stale_config_map_save_conflicts() {
        let (config, config_map) = test_config_map();

        let error = prepare_config_map_replacement(
            config_map,
            &config,
            "16",
            "programs:\n  codex: {}\n".to_owned(),
        )
        .expect_err("stale save must fail");

        assert!(matches!(error, ReplaceFleetConfigurationError::Conflict));
    }

    #[test]
    fn rejects_invalid_config_map_revision() {
        let config = ControllerDaemonConfigMap::new("agentdesktop".to_owned(), "fleet".to_owned());
        let config_map = ConfigMap {
            metadata: ObjectMeta {
                resource_version: Some("17".to_owned()),
                ..Default::default()
            },
            data: Some(BTreeMap::from([
                ("daemon.yaml".to_owned(), "programs: {}\n".to_owned()),
                ("revision".to_owned(), "0".to_owned()),
            ])),
            ..Default::default()
        };

        let error = config_map_snapshot(&config_map, &config).expect_err("zero revision");

        assert!(error.to_string().contains("must be greater than zero"));
    }
}
