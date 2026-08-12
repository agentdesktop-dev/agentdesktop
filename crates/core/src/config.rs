//! Agentdesktop daemon and controller-managed configuration.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use url::Url;

/// Configuration for an Agentdesktop daemon.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DaemonConfig {
    /// Controller connection settings. Omit this field to run without fleet management.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<ControllerConnectionConfig>,
    /// Inference gateway used by managed developer tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inference_gateway: Option<InferenceGatewayConfig>,
    /// Telemetry collected from managed developer tools.
    #[serde(default, skip_serializing_if = "TelemetryConfig::is_empty")]
    pub telemetry: TelemetryConfig,
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

/// Telemetry events collected from managed developer tools.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Event names to collect. `tool.use.input` implies `tool.use` and includes tool arguments.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub events: BTreeSet<TelemetryEventName>,
}

impl TelemetryConfig {
    fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    pub fn collects_tool_use(&self) -> bool {
        self.events.contains(&TelemetryEventName::ToolUse)
            || self.events.contains(&TelemetryEventName::ToolUseInput)
    }

    pub fn includes_tool_input(&self) -> bool {
        self.events.contains(&TelemetryEventName::ToolUseInput)
    }

    pub fn collects_session_new(&self) -> bool {
        self.events.contains(&TelemetryEventName::SessionNew)
    }
}

/// A normalized telemetry event name.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum TelemetryEventName {
    /// A new developer-tool session.
    #[serde(rename = "session.new")]
    SessionNew,
    /// Tool invocation metadata.
    #[serde(rename = "tool.use")]
    ToolUse,
    /// Tool invocation metadata and input. This implies `tool.use`.
    #[serde(rename = "tool.use.input")]
    ToolUseInput,
}

/// Connection settings used by a daemon to reach the fleet controller.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerConnectionConfig {
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

/// Startup configuration for the Agentdesktop fleet controller.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerConfig {
    /// Address on which the device-facing gRPC fleet API listens.
    #[serde(default = "default_fleet_listen")]
    #[cfg_attr(feature = "schema", schemars(with = "String"))]
    pub fleet_listen: SocketAddr,
    /// Loopback address on which the controller management UI listens.
    #[serde(default = "default_admin_listen")]
    #[cfg_attr(feature = "schema", schemars(with = "String"))]
    pub admin_listen: SocketAddr,
    /// SQLite or PostgreSQL URL used for controller state.
    #[serde(default = "default_controller_database_url")]
    pub database_url: String,
    /// OpenID Connect enrollment settings. Omit to disable new enrollment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oidc: Option<ControllerOidcConfig>,
    /// Daemon configuration distributed to enrolled devices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_config: Option<ControllerDaemonConfig>,
    /// Inference-gateway JWT signing settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_jwt: Option<ControllerGatewayJwtConfig>,
    /// TLS identity used by the device-facing fleet API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<ControllerTlsConfig>,
    /// Permit plaintext remote fleet traffic and non-HTTPS OIDC.
    ///
    /// This escape hatch is only appropriate for isolated local development.
    #[serde(default)]
    pub allow_insecure_dev: bool,
}

/// OpenID Connect settings used for interactive device enrollment.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerOidcConfig {
    /// Exact OpenID Connect issuer URL.
    pub issuer: String,
    /// Public OpenID Connect client identifier.
    pub client_id: String,
    /// Redirect URI registered for the native enrollment client.
    #[serde(default = "default_oidc_redirect_uri")]
    pub redirect_uri: String,
}

/// Controller-owned daemon configuration file and its revision.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerDaemonConfig {
    /// Path to the watched YAML configuration distributed to enrolled devices.
    ///
    /// Relative paths are resolved from the controller configuration directory.
    /// Valid file changes are published to connected devices automatically.
    pub path: PathBuf,
    /// Monotonically increasing revision assigned to the daemon configuration.
    #[serde(default = "default_daemon_config_revision")]
    pub revision: u64,
}

/// Settings for issuing short-lived inference-gateway JWTs.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerGatewayJwtConfig {
    /// Path to the PEM-encoded RSA private signing key.
    ///
    /// Relative paths are resolved from the controller configuration directory.
    pub private_key: PathBuf,
    /// Issuer claim placed in generated JWTs.
    #[serde(default = "default_gateway_jwt_issuer")]
    pub issuer: String,
    /// Key identifier placed in generated JWT headers.
    #[serde(default = "default_gateway_jwt_key_id")]
    pub key_id: String,
    /// Lifetime of generated JWTs. Defaults to `5m`.
    #[serde(default = "default_gateway_jwt_lifetime", with = "humantime_serde")]
    #[cfg_attr(feature = "schema", schemars(with = "String"))]
    pub lifetime: Duration,
}

/// TLS certificate and private key used by the fleet API.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerTlsConfig {
    /// Path to the PEM-encoded TLS certificate chain.
    ///
    /// Relative paths are resolved from the controller configuration directory.
    pub certificate: PathBuf,
    /// Path to the PEM-encoded TLS private key.
    ///
    /// Relative paths are resolved from the controller configuration directory.
    pub key: PathBuf,
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            fleet_listen: default_fleet_listen(),
            admin_listen: default_admin_listen(),
            database_url: default_controller_database_url(),
            oidc: None,
            daemon_config: None,
            gateway_jwt: None,
            tls: None,
            allow_insecure_dev: false,
        }
    }
}

fn default_fleet_listen() -> SocketAddr {
    "127.0.0.1:8443"
        .parse()
        .expect("valid fleet listen default")
}

fn default_admin_listen() -> SocketAddr {
    "127.0.0.1:8080"
        .parse()
        .expect("valid admin listen default")
}

fn default_controller_database_url() -> String {
    "sqlite://agentdesktop-controller.db?mode=rwc".to_owned()
}

fn default_oidc_redirect_uri() -> String {
    "http://127.0.0.1:5555/callback".to_owned()
}

fn default_daemon_config_revision() -> u64 {
    1
}

fn default_gateway_jwt_issuer() -> String {
    "agentdesktop-controller".to_owned()
}

fn default_gateway_jwt_key_id() -> String {
    "agentdesktop".to_owned()
}

fn default_gateway_jwt_lifetime() -> Duration {
    Duration::from_secs(5 * 60)
}

fn default_heartbeat_interval() -> Duration {
    Duration::from_secs(30)
}

/// Settings for developer tools managed by Agentdesktop.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgramsConfig {
    /// Claude Code managed-settings configuration. Arbitrary keys are passed through directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code: Option<ClaudeCodeConfig>,
    /// Claude Desktop managed configuration. Arbitrary keys are passed through directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_desktop: Option<ClaudeDesktopConfig>,
    /// Codex managed configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<CodexConfig>,
    /// OpenCode managed configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_code: Option<OpenCodeConfig>,
}

impl ProgramsConfig {
    fn is_empty(&self) -> bool {
        self.claude_code.is_none()
            && self.claude_desktop.is_none()
            && self.codex.is_none()
            && self.open_code.is_none()
    }
}

/// Settings reconciled into Claude Code's managed configuration.
///
/// Arbitrary keys are written directly to Agentdesktop's managed-settings
/// drop-in. Generated gateway settings take precedence when values overlap.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ClaudeCodeConfig {
    /// Whether this program uses the top-level inference gateway.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub use_inference_gateway: bool,
    /// Arbitrary Claude Code managed-settings values, flattened into this object.
    #[serde(default, flatten)]
    pub settings: BTreeMap<String, serde_json::Value>,
}

/// Settings reconciled into Claude Desktop's managed configuration.
///
/// Arbitrary keys are written directly to the managed settings file. Generated
/// gateway settings take precedence when values overlap.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ClaudeDesktopConfig {
    /// Whether this program uses the top-level inference gateway.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub use_inference_gateway: bool,
    /// Arbitrary Claude Desktop managed-settings values, flattened into this object.
    #[serde(default, flatten)]
    pub settings: BTreeMap<String, serde_json::Value>,
}

/// Settings reconciled into Codex's organization-managed configuration.
///
/// Values under `managedConfig` are written to Codex's `managed_config.toml`.
/// When generated inference-gateway settings overlap with those values,
/// Agentdesktop's generated values take precedence.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexConfig {
    /// Whether this program uses the top-level inference gateway.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub use_inference_gateway: bool,
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
/// Agentdesktop's generated values take precedence.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenCodeConfig {
    /// Whether this program uses the top-level inference gateway.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub use_inference_gateway: bool,
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

/// Loads and validates a controller YAML configuration file from `path`.
pub fn load_controller(path: &Path) -> anyhow::Result<ControllerConfig> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("read controller configuration from {}", path.display()))?;
    let mut config = parse_controller(&contents)
        .with_context(|| format!("parse controller configuration from {}", path.display()))?;
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    if let Some(daemon) = &mut config.daemon_config {
        resolve_relative(&mut daemon.path, directory);
    }
    if let Some(gateway) = &mut config.gateway_jwt {
        resolve_relative(&mut gateway.private_key, directory);
    }
    if let Some(tls) = &mut config.tls {
        resolve_relative(&mut tls.certificate, directory);
        resolve_relative(&mut tls.key, directory);
    }
    Ok(config)
}

/// Parses and validates a controller YAML configuration document.
pub fn parse_controller(contents: &str) -> anyhow::Result<ControllerConfig> {
    let config: ControllerConfig =
        crate::serdes::yamlviajson::from_str(contents).context("parse controller configuration")?;
    if !config.admin_listen.ip().is_loopback() {
        anyhow::bail!("adminListen must use a loopback address");
    }
    if !config.fleet_listen.ip().is_loopback() && config.tls.is_none() && !config.allow_insecure_dev
    {
        anyhow::bail!(
            "a non-loopback fleetListen requires tls; allowInsecureDev is only for isolated development"
        );
    }
    if let Some(oidc) = &config.oidc {
        if oidc.client_id.trim().is_empty() {
            anyhow::bail!("oidc.clientId cannot be empty");
        }
        let issuer = Url::parse(&oidc.issuer).context("parse oidc.issuer URL")?;
        match issuer.scheme() {
            "https" => {}
            "http" if config.allow_insecure_dev => {}
            "http" => anyhow::bail!(
                "oidc.issuer must use HTTPS; allowInsecureDev is only for isolated development"
            ),
            scheme => anyhow::bail!("oidc.issuer must use HTTPS, got {scheme}"),
        }
        Url::parse(&oidc.redirect_uri).context("parse oidc.redirectUri URL")?;
    }
    if let Some(daemon) = &config.daemon_config
        && daemon.revision == 0
    {
        anyhow::bail!("daemonConfig.revision must be greater than zero");
    }
    if let Some(gateway) = &config.gateway_jwt {
        if gateway.issuer.trim().is_empty() {
            anyhow::bail!("gatewayJwt.issuer cannot be empty");
        }
        if gateway.key_id.trim().is_empty() {
            anyhow::bail!("gatewayJwt.keyId cannot be empty");
        }
        if gateway.lifetime.is_zero() {
            anyhow::bail!("gatewayJwt.lifetime must be greater than zero");
        }
    }
    Ok(config)
}

fn resolve_relative(path: &mut PathBuf, directory: &Path) {
    if path.is_relative() {
        *path = directory.join(&*path);
    }
}

/// Parses and validates a daemon YAML configuration document.
pub fn parse_daemon(contents: &str) -> anyhow::Result<DaemonConfig> {
    let config: DaemonConfig =
        crate::serdes::yamlviajson::from_str(contents).context("parse daemon configuration")?;
    validate_daemon(config.inference_gateway.as_ref(), &config.programs)?;
    Ok(config)
}

impl DaemonConfig {
    /// Returns whether this configuration manages no gateway or developer tools.
    pub fn is_empty(&self) -> bool {
        self.inference_gateway.is_none() && self.telemetry.is_empty() && self.programs.is_empty()
    }
}

fn validate_daemon(
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
        && open_code.use_inference_gateway
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

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

#[cfg(test)]
mod tests {
    use super::{parse_controller, parse_daemon};

    #[test]
    fn daemon_configuration_supports_local_and_managed_options() {
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
        assert!(daemon.inference_gateway.is_some());
    }

    #[test]
    fn checked_in_examples_use_their_declared_configuration_surface() {
        parse_controller(include_str!("../../../examples/claude/controller.yaml"))
            .expect("controller example");
        parse_daemon(include_str!("../../../examples/claude/agentdesktopd.yaml"))
            .expect("controller-connected daemon example");
        parse_daemon(include_str!("../../../examples/claude/claude-code.yaml"))
            .expect("Claude Code daemon configuration example");
    }

    #[test]
    fn controller_requires_secure_remote_transports() {
        assert!(
            parse_controller(
                r#"
fleetListen: 0.0.0.0:8443
oidc:
  issuer: http://idp.example.com
  clientId: agentdesktop
"#,
            )
            .is_err()
        );
        parse_controller(
            r#"
fleetListen: 0.0.0.0:8443
allowInsecureDev: true
oidc:
  issuer: http://idp.example.com
  clientId: agentdesktop
"#,
        )
        .expect("explicit insecure development configuration");
    }

    #[test]
    fn rejects_an_invalid_inference_gateway_url() {
        let error = parse_daemon(
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
        let error = parse_daemon(
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
