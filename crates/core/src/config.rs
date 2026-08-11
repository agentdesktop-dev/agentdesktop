//! AgentDesktop daemon and controller-managed configuration.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use url::Url;

/// Local startup configuration for an AgentDesktop daemon.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DaemonConfig {
    /// Controller connection settings. Omit this field to run without fleet management.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<ControllerConfig>,
    /// Named inference gateways used as the local desired-state baseline.
    ///
    /// Names may contain letters, numbers, `.`, `-`, and `_`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inference_gateways: BTreeMap<String, InferenceGatewayConfig>,
    /// Per-program settings used as the local desired-state baseline.
    #[serde(default, skip_serializing_if = "ProgramsConfig::is_empty")]
    pub programs: ProgramsConfig,
}

/// Desired configuration distributed by the controller and reconciled by a daemon.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesiredConfig {
    /// Named inference gateways that managed developer tools can use.
    ///
    /// Names may contain letters, numbers, `.`, `-`, and `_`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub inference_gateways: BTreeMap<String, InferenceGatewayConfig>,
    /// Per-program settings reconciled on this device.
    #[serde(default, skip_serializing_if = "ProgramsConfig::is_empty")]
    pub programs: ProgramsConfig,
}

/// Connection and authentication settings for an inference gateway.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InferenceGatewayConfig {
    /// Base HTTP or HTTPS URL of the inference gateway.
    ///
    /// The URL must include a host and cannot include credentials, a query, or a fragment.
    #[cfg_attr(feature = "schema", schemars(with = "String"))]
    pub url: Url,
    /// Authentication mechanism used when connecting to this gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<InferenceGatewayAuthentication>,
}

/// Authentication mechanisms supported by an inference gateway.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum InferenceGatewayAuthentication {
    /// Request a short-lived JWT from the controller using the device identity.
    ControllerJwt {
        /// Audience placed in the issued JWT. This must match the gateway's expected audience.
        audience: String,
    },
}

/// Connection settings for the fleet controller.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerConfig {
    /// HTTP or HTTPS address of the controller's fleet API.
    pub address: String,
    /// Path to a PEM-encoded CA certificate used to verify the controller.
    ///
    /// Omit this field to use the operating system's trusted certificate roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_certificate_path: Option<PathBuf>,
    /// Interval between device heartbeats. Defaults to `30s`.
    #[serde(default = "default_heartbeat_interval", with = "humantime_serde")]
    #[cfg_attr(feature = "schema", schemars(with = "String"))]
    pub heartbeat_interval: Duration,
}

fn default_heartbeat_interval() -> Duration {
    Duration::from_secs(30)
}

/// Settings for developer tools managed by AgentDesktop.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgramsConfig {
    /// Claude Code managed-settings configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code: Option<ClaudeCodeConfig>,
}

impl ProgramsConfig {
    fn is_empty(&self) -> bool {
        self.claude_code.is_none()
    }
}

/// Settings reconciled into Claude Code's managed configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeCodeConfig {
    /// Name of an entry in `inferenceGateways` that Claude Code should use.
    pub inference_gateway: String,
}

/// Loads and validates a daemon YAML configuration file from `path`.
pub fn load_daemon(path: &Path) -> anyhow::Result<DaemonConfig> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read configuration from {}", path.display()))?;
    parse_daemon(&contents).with_context(|| format!("parse configuration from {}", path.display()))
}

/// Parses and validates a daemon YAML configuration document.
pub fn parse_daemon(contents: &str) -> anyhow::Result<DaemonConfig> {
    let config: DaemonConfig =
        crate::serdes::yamlviajson::from_str(contents).context("parse daemon configuration")?;
    validate_desired(&config.inference_gateways, &config.programs)?;
    Ok(config)
}

/// Parses and validates a controller-managed desired configuration document.
pub fn parse_desired(contents: &str) -> anyhow::Result<DesiredConfig> {
    let config: DesiredConfig =
        crate::serdes::yamlviajson::from_str(contents).context("parse desired configuration")?;
    validate_desired(&config.inference_gateways, &config.programs)?;
    Ok(config)
}

impl DaemonConfig {
    /// Returns the local desired-state portion of this daemon configuration.
    pub fn desired_config(&self) -> DesiredConfig {
        DesiredConfig {
            inference_gateways: self.inference_gateways.clone(),
            programs: self.programs.clone(),
        }
    }
}

fn validate_desired(
    inference_gateways: &BTreeMap<String, InferenceGatewayConfig>,
    programs: &ProgramsConfig,
) -> anyhow::Result<()> {
    for (name, gateway) in inference_gateways {
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            anyhow::bail!(
                "inference gateway name {name:?} must contain only letters, numbers, '.', '-', or '_'"
            );
        }
        if !matches!(gateway.url.scheme(), "http" | "https") {
            anyhow::bail!(
                "inference gateway {name} URL must use HTTP or HTTPS, got {}",
                gateway.url.scheme()
            );
        }
        if gateway.url.host().is_none() {
            anyhow::bail!("inference gateway {name} URL must include a host");
        }
        if !gateway.url.username().is_empty() || gateway.url.password().is_some() {
            anyhow::bail!("inference gateway {name} URL cannot include credentials");
        }
        if gateway.url.query().is_some() || gateway.url.fragment().is_some() {
            anyhow::bail!("inference gateway {name} URL cannot include a query or fragment");
        }
        if let Some(InferenceGatewayAuthentication::ControllerJwt { audience }) =
            &gateway.authentication
            && audience.trim().is_empty()
        {
            anyhow::bail!("inference gateway {name} JWT audience cannot be empty");
        }
    }

    if let Some(claude_code) = &programs.claude_code
        && !inference_gateways.contains_key(&claude_code.inference_gateway)
    {
        anyhow::bail!(
            "Claude Code references unknown inference gateway {}",
            claude_code.inference_gateway
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_daemon, parse_desired};

    #[test]
    fn daemon_options_are_not_valid_desired_configuration() {
        let document = r#"
controller:
  address: http://127.0.0.1:8443
inferenceGateways:
  local:
    url: http://127.0.0.1:8080
programs:
  claudeCode:
    inferenceGateway: local
"#;

        let daemon = parse_daemon(document).expect("valid daemon configuration");
        assert!(daemon.controller.is_some());
        assert!(
            daemon
                .desired_config()
                .inference_gateways
                .contains_key("local")
        );
        assert!(parse_desired(document).is_err());
    }

    #[test]
    fn checked_in_examples_use_their_declared_configuration_surface() {
        parse_daemon(include_str!("../../../config.yaml")).expect("base daemon example");
        parse_daemon(include_str!("../../../config.controller.yaml.example"))
            .expect("controller-connected daemon example");
        parse_daemon(include_str!("../../../config.docker.yaml")).expect("Docker daemon example");
        parse_desired(include_str!("../../../config.claude-code.yaml.example"))
            .expect("Claude Code desired configuration example");
    }
}
