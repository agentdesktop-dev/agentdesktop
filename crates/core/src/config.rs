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
    /// Inference gateway used as the local desired-state baseline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_gateway: Option<InferenceGatewayConfig>,
    /// Per-program settings used as the local desired-state baseline.
    #[serde(default, skip_serializing_if = "ProgramsConfig::is_empty")]
    pub programs: ProgramsConfig,
}

/// Desired configuration distributed by the controller and reconciled by a daemon.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DesiredConfig {
    /// Inference gateway used by managed developer tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_gateway: Option<InferenceGatewayConfig>,
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
    /// Claude Code managed-settings configuration. Arbitrary keys are passed through directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code: Option<ClaudeCodeConfig>,
    /// Codex managed configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexConfig>,
    /// OpenCode managed configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_code: Option<OpenCodeConfig>,
}

impl ProgramsConfig {
    fn is_empty(&self) -> bool {
        self.claude_code.is_none() && self.codex.is_none() && self.open_code.is_none()
    }
}

/// Settings reconciled into Claude Code's managed configuration.
///
/// Arbitrary keys are written directly to AgentDesktop's managed-settings
/// drop-in. Generated gateway settings take precedence when values overlap.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeConfig {
    /// Arbitrary Claude Code managed-settings values, flattened into this object.
    #[serde(default, flatten)]
    pub settings: BTreeMap<String, serde_json::Value>,
}

/// Settings reconciled into Codex's organization-managed configuration.
///
/// Values under `managedConfig` are written to Codex's `managed_config.toml`.
/// When generated inference-gateway settings overlap with those values,
/// AgentDesktop's generated values take precedence.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexConfig {
    /// Arbitrary values written to Codex's organization-managed TOML configuration.
    ///
    /// Use Codex's native snake_case configuration keys. TOML has no null value,
    /// so null values cannot be reconciled.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub managed_config: BTreeMap<String, serde_json::Value>,
}

/// Settings reconciled into OpenCode's system-managed configuration.
///
/// Values under `managedConfig` are written to OpenCode's managed JSONC file.
/// When generated inference-gateway settings overlap with those values,
/// AgentDesktop's generated values take precedence.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenCodeConfig {
    /// Model ID selected from `models` when using the inference gateway.
    ///
    /// This is required when a top-level `inferenceGateway` is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Models exposed by the managed inference-gateway provider, keyed by model ID.
    ///
    /// Each value is an arbitrary OpenCode model configuration object. At least
    /// one model is required when a top-level `inferenceGateway` is configured.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, serde_json::Value>,
    /// Arbitrary values written to OpenCode's system-managed configuration.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub managed_config: BTreeMap<String, serde_json::Value>,
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
    validate_desired(config.inference_gateway.as_ref(), &config.programs)?;
    Ok(config)
}

/// Parses and validates a controller-managed desired configuration document.
pub fn parse_desired(contents: &str) -> anyhow::Result<DesiredConfig> {
    let config: DesiredConfig =
        crate::serdes::yamlviajson::from_str(contents).context("parse desired configuration")?;
    validate_desired(config.inference_gateway.as_ref(), &config.programs)?;
    Ok(config)
}

impl DaemonConfig {
    /// Returns the local desired-state portion of this daemon configuration.
    pub fn desired_config(&self) -> DesiredConfig {
        DesiredConfig {
            inference_gateway: self.inference_gateway.clone(),
            programs: self.programs.clone(),
        }
    }
}

impl DesiredConfig {
    /// Returns whether this configuration manages no gateway or developer tools.
    pub fn is_empty(&self) -> bool {
        self.inference_gateway.is_none() && self.programs.is_empty()
    }
}

fn validate_desired(
    inference_gateway: Option<&InferenceGatewayConfig>,
    programs: &ProgramsConfig,
) -> anyhow::Result<()> {
    if let Some(gateway) = inference_gateway {
        if !matches!(gateway.url.scheme(), "http" | "https") {
            anyhow::bail!(
                "inference gateway URL must use HTTP or HTTPS, got {}",
                gateway.url.scheme()
            );
        }
        if gateway.url.host().is_none() {
            anyhow::bail!("inference gateway URL must include a host");
        }
        if !gateway.url.username().is_empty() || gateway.url.password().is_some() {
            anyhow::bail!("inference gateway URL cannot include credentials");
        }
        if gateway.url.query().is_some() || gateway.url.fragment().is_some() {
            anyhow::bail!("inference gateway URL cannot include a query or fragment");
        }
        if let Some(InferenceGatewayAuthentication::ControllerJwt { audience }) =
            &gateway.authentication
            && audience.trim().is_empty()
        {
            anyhow::bail!("inference gateway JWT audience cannot be empty");
        }
    }

    if let Some(open_code) = &programs.open_code
        && inference_gateway.is_some()
    {
        let model = open_code
            .model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
            .context("OpenCode requires model when inferenceGateway is configured")?;
        if !open_code.models.contains_key(model) {
            anyhow::bail!("OpenCode model {model} is not declared in models");
        }
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
inferenceGateway:
  url: http://127.0.0.1:8080
programs:
  claudeCode: {}
"#;

        let daemon = parse_daemon(document).expect("valid daemon configuration");
        assert!(daemon.controller.is_some());
        assert!(daemon.desired_config().inference_gateway.is_some());
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
        parse_desired(include_str!("../../../config.codex.yaml.example"))
            .expect("Codex desired configuration example");
        parse_desired(include_str!("../../../config.opencode.yaml.example"))
            .expect("OpenCode desired configuration example");
    }

    #[test]
    fn rejects_an_invalid_inference_gateway_url() {
        let error = parse_desired(
            r#"
inferenceGateway:
  url: ftp://gateway.example.com
"#,
        )
        .expect_err("invalid gateway should fail");

        assert!(error.to_string().contains("must use HTTP or HTTPS"));
    }

    #[test]
    fn open_code_requires_a_declared_gateway_model() {
        let error = parse_desired(
            r#"
inferenceGateway:
  url: https://gateway.example.com
programs:
  openCode:
    model: missing
    models:
      available: {}
"#,
        )
        .expect_err("undeclared model should fail");

        assert!(
            error
                .to_string()
                .contains("OpenCode model missing is not declared in models")
        );
    }
}
