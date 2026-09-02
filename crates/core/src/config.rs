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
    /// LLM gateway used by managed developer tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_gateway: Option<LlmGatewayConfig>,
    /// Telemetry collected from managed developer tools.
    #[serde(default, skip_serializing_if = "TelemetryConfig::is_empty")]
    pub telemetry: TelemetryConfig,
    /// Per-program settings reconciled on this device.
    #[serde(default, skip_serializing_if = "ProgramsConfig::is_empty")]
    pub programs: ProgramsConfig,
}

/// Connection and authentication settings for an LLM gateway.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LlmGatewayConfig {
    /// Base HTTP or HTTPS URL of the LLM gateway.
    ///
    /// The URL must include a host and cannot include credentials, a query, or a fragment.
    #[cfg_attr(feature = "schema", schemars(with = "String"))]
    pub url: Url,
    /// Authentication mechanism used when connecting to this gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<LlmGatewayAuthentication>,
}

/// Authentication mechanisms supported by an LLM gateway.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum LlmGatewayAuthentication {
    /// Request a short-lived JWT from the controller using the device identity.
    ControllerJwt {
        /// Audience placed in the issued JWT. This must match the gateway's expected audience.
        audience: String,
        /// Client identifiers permitted to request credentials for this gateway.
        #[serde(rename = "allowedClientIds")]
        allowed_client_ids: BTreeSet<String>,
    },
    /// Sign the local user in with OIDC and send the resulting access token.
    Oidc {
        /// Exact OpenID Connect issuer URL.
        #[cfg_attr(feature = "schema", schemars(with = "String"))]
        issuer: Url,
        /// Public OpenID Connect client identifier.
        #[serde(rename = "clientId")]
        client_id: String,
        /// Loopback redirect URI registered for the native client.
        #[serde(rename = "redirectUri", default = "default_oidc_redirect_uri")]
        redirect_uri: String,
        /// Scopes requested during sign-in.
        #[serde(default = "default_gateway_oidc_scopes")]
        scopes: Vec<String>,
        /// Permit loopback HTTP endpoints for isolated local development.
        #[serde(rename = "allowInsecure", default)]
        allow_insecure: bool,
    },
}

impl LlmGatewayAuthentication {
    /// Returns whether this authentication mode uses the local credential helper.
    pub fn uses_credential_helper(&self) -> bool {
        true
    }
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
    /// HTTPS address of the controller's fleet API.
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
    /// OpenID Connect settings used for device enrollment and authorization.
    pub oidc: ControllerOidcConfig,
    /// Daemon configuration distributed to enrolled devices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_config: Option<ControllerDaemonConfig>,
    /// LLM gateway JWT signing settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway_jwt: Option<ControllerGatewayJwtConfig>,
    /// TLS identities used by the device-facing fleet API.
    ///
    /// A string selects a directory containing `controller.pem`,
    /// `controller-key.pem`, `device-ca.pem`, and `device-ca-key.pem`.
    pub tls: ControllerTlsConfig,
    /// Permit a non-HTTPS OIDC issuer for isolated local development.
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

/// Source of the daemon configuration distributed by the controller.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged, deny_unknown_fields)]
pub enum ControllerDaemonConfig {
    /// Read-only configuration loaded from a watched file.
    File(ControllerDaemonFileConfig),
    /// Writable configuration stored in the controller database.
    Database {
        database: ControllerDaemonDatabaseConfig,
    },
}

/// Controller-owned daemon configuration file and its revision.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerDaemonFileConfig {
    /// Path to the watched YAML configuration distributed to enrolled devices.
    ///
    /// Relative paths are resolved from the controller configuration directory.
    /// Valid file changes are published to connected devices automatically.
    pub path: PathBuf,
    /// Monotonically increasing revision assigned to the daemon configuration.
    #[serde(default = "default_daemon_config_revision")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    pub revision: u64,
}

/// Initial values for database-backed fleet configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerDaemonDatabaseConfig {
    /// Optional YAML file used only when the database has no fleet configuration.
    ///
    /// Relative paths are resolved from the controller configuration directory.
    /// When omitted, the database is initialized with an empty programs map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed_path: Option<PathBuf>,
    /// Initial fleet revision assigned when seeding the database.
    #[serde(default = "default_daemon_config_revision")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    pub seed_revision: u64,
}

/// Settings for issuing short-lived LLM gateway JWTs.
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

/// TLS configuration for the fleet API and device certificate issuer.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(untagged)]
pub enum ControllerTlsConfig {
    /// Directory containing the four standard TLS files.
    Directory(PathBuf),
    /// Explicit paths to each TLS file.
    Files(ControllerTlsFiles),
}

/// Explicit TLS file paths for the fleet API and device certificate issuer.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerTlsFiles {
    /// Path to the PEM-encoded TLS certificate chain.
    ///
    /// Relative paths are resolved from the controller configuration directory.
    pub certificate: PathBuf,
    /// Path to the PEM-encoded TLS private key.
    ///
    /// Relative paths are resolved from the controller configuration directory.
    pub key: PathBuf,
    /// PEM CA roots used to verify issued device client certificates.
    pub client_ca_certificate: PathBuf,
    /// PEM private key used to issue device certificates from `clientCaCertificate`.
    ///
    /// Enrolled daemons generate their own private key and send a CSR.
    pub client_ca_key: PathBuf,
}

impl ControllerTlsConfig {
    pub fn files(&self) -> ControllerTlsFiles {
        match self {
            Self::Directory(directory) => ControllerTlsFiles {
                certificate: directory.join("controller.pem"),
                key: directory.join("controller-key.pem"),
                client_ca_certificate: directory.join("device-ca.pem"),
                client_ca_key: directory.join("device-ca-key.pem"),
            },
            Self::Files(files) => files.clone(),
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
    "http://127.0.0.1:51327/callback".to_owned()
}

fn default_gateway_oidc_scopes() -> Vec<String> {
    vec!["openid".to_owned(), "offline_access".to_owned()]
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
    /// Whether this program uses the top-level LLM gateway.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub use_llm_gateway: bool,
    /// Upstream authentication used by this agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProgramAuthentication>,
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
    /// Whether this program uses the top-level LLM gateway.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub use_llm_gateway: bool,
    /// Upstream authentication used by this agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<ProgramAuthentication>,
    /// Arbitrary Claude Desktop managed-settings values, flattened into this object.
    #[serde(default, flatten)]
    pub settings: BTreeMap<String, serde_json::Value>,
}

/// Settings reconciled into Codex's organization-managed configuration.
///
/// Values under `managedConfig` are written to Codex's `managed_config.toml`.
/// When generated LLM-gateway settings overlap with those values,
/// Agentdesktop's generated values take precedence.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CodexConfig {
    /// Whether this program uses the top-level LLM gateway.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub use_llm_gateway: bool,
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
/// When generated LLM gateway settings overlap with those values,
/// Agentdesktop's generated values take precedence.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenCodeConfig {
    /// Whether this program uses the top-level LLM gateway.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub use_llm_gateway: bool,
    /// Model ID selected from `models` when using the LLM gateway.
    ///
    /// This is required when a top-level `llmGateway` is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Models exposed by the managed LLM gateway provider, keyed by model ID.
    ///
    /// Each value is an arbitrary OpenCode model configuration object. At least
    /// one model is required when a top-level `llmGateway` is configured.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, serde_json::Value>,
    /// Arbitrary values written to OpenCode's system-managed configuration.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub managed_config: BTreeMap<String, serde_json::Value>,
}

/// Upstream authentication selected by a managed agent.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ProgramAuthentication {
    /// Offer the model provider subscription associated with the local user.
    /// The user may skip it and continue with gateway identity only.
    Subscription,
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
        match daemon {
            ControllerDaemonConfig::File(daemon) => {
                resolve_relative(&mut daemon.path, directory);
            }
            ControllerDaemonConfig::Database { database } => {
                if let Some(seed_path) = &mut database.seed_path {
                    resolve_relative(seed_path, directory);
                }
            }
        }
    }
    if let Some(gateway) = &mut config.gateway_jwt {
        resolve_relative(&mut gateway.private_key, directory);
    }
    match &mut config.tls {
        ControllerTlsConfig::Directory(path) => resolve_relative(path, directory),
        ControllerTlsConfig::Files(tls) => {
            resolve_relative(&mut tls.certificate, directory);
            resolve_relative(&mut tls.key, directory);
            resolve_relative(&mut tls.client_ca_certificate, directory);
            resolve_relative(&mut tls.client_ca_key, directory);
        }
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
    let oidc = &config.oidc;
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
    if let Some(daemon) = &config.daemon_config {
        match daemon {
            ControllerDaemonConfig::File(daemon) if daemon.revision == 0 => {
                anyhow::bail!("daemonConfig.revision must be greater than zero");
            }
            ControllerDaemonConfig::Database { database } if database.seed_revision == 0 => {
                anyhow::bail!("daemonConfig.database.seedRevision must be greater than zero");
            }
            ControllerDaemonConfig::File(_) | ControllerDaemonConfig::Database { .. } => {}
        }
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
    if let Some(controller) = &config.controller
        && !controller.address.starts_with("https://")
    {
        anyhow::bail!("controller address must use HTTPS");
    }
    validate_daemon(config.llm_gateway.as_ref(), &config.programs)?;
    Ok(config)
}

impl DaemonConfig {
    /// Returns whether this configuration manages no gateway or developer tools.
    pub fn is_empty(&self) -> bool {
        self.llm_gateway.is_none() && self.telemetry.is_empty() && self.programs.is_empty()
    }
}

fn validate_daemon(
    llm_gateway: Option<&LlmGatewayConfig>,
    programs: &ProgramsConfig,
) -> anyhow::Result<()> {
    if let Some(gateway) = llm_gateway {
        if !matches!(gateway.url.scheme(), "http" | "https") {
            anyhow::bail!(
                "LLM gateway URL must use HTTP or HTTPS, got {}",
                gateway.url.scheme()
            );
        }
        if gateway.url.host().is_none() {
            anyhow::bail!("LLM gateway URL must include a host");
        }
        if !gateway.url.username().is_empty() || gateway.url.password().is_some() {
            anyhow::bail!("LLM gateway URL cannot include credentials");
        }
        if gateway.url.query().is_some() || gateway.url.fragment().is_some() {
            anyhow::bail!("LLM gateway URL cannot include a query or fragment");
        }
        if let Some(authentication) = &gateway.authentication {
            match authentication {
                LlmGatewayAuthentication::ControllerJwt {
                    audience,
                    allowed_client_ids,
                } => {
                    if audience.trim().is_empty() {
                        anyhow::bail!("LLM gateway JWT audience cannot be empty");
                    }
                    if allowed_client_ids.is_empty() {
                        anyhow::bail!("LLM gateway JWT allowedClientIds cannot be empty");
                    }
                    if let Some(client_id) = allowed_client_ids
                        .iter()
                        .find(|client_id| !valid_client_id(client_id))
                    {
                        anyhow::bail!("invalid LLM gateway client ID {client_id}");
                    }
                }
                LlmGatewayAuthentication::Oidc {
                    issuer,
                    client_id,
                    redirect_uri,
                    scopes,
                    allow_insecure,
                } => {
                    if issuer.host().is_none() {
                        anyhow::bail!("LLM gateway OIDC issuer must include a host");
                    }
                    match issuer.scheme() {
                        "https" => {}
                        "http" if *allow_insecure && issuer.host_str().is_some_and(is_loopback) => {
                        }
                        "http" if *allow_insecure => anyhow::bail!(
                            "LLM gateway OIDC allowInsecure only permits loopback issuers"
                        ),
                        "http" => anyhow::bail!(
                            "LLM gateway OIDC issuer must use HTTPS; allowInsecure is only for isolated loopback development"
                        ),
                        scheme => {
                            anyhow::bail!("LLM gateway OIDC issuer must use HTTPS, got {scheme}")
                        }
                    }
                    if !issuer.username().is_empty()
                        || issuer.password().is_some()
                        || issuer.query().is_some()
                        || issuer.fragment().is_some()
                    {
                        anyhow::bail!(
                            "LLM gateway OIDC issuer cannot contain credentials, a query, or a fragment"
                        );
                    }
                    if client_id.trim().is_empty() {
                        anyhow::bail!("LLM gateway OIDC clientId cannot be empty");
                    }
                    Url::parse(redirect_uri).context("parse LLM gateway OIDC redirectUri URL")?;
                    if scopes.is_empty() || scopes.iter().any(|scope| scope.trim().is_empty()) {
                        anyhow::bail!("LLM gateway OIDC scopes cannot be empty");
                    }
                }
            }
        }
    }

    for (name, subscription, uses_gateway) in
        [
            (
                "claudeCode",
                programs.claude_code.as_ref().is_some_and(|program| {
                    program.auth == Some(ProgramAuthentication::Subscription)
                }),
                programs
                    .claude_code
                    .as_ref()
                    .is_some_and(|program| program.use_llm_gateway),
            ),
            (
                "claudeDesktop",
                programs.claude_desktop.as_ref().is_some_and(|program| {
                    program.auth == Some(ProgramAuthentication::Subscription)
                }),
                programs
                    .claude_desktop
                    .as_ref()
                    .is_some_and(|program| program.use_llm_gateway),
            ),
        ]
    {
        if subscription && (!uses_gateway || llm_gateway.is_none()) {
            anyhow::bail!(
                "programs.{name}.auth subscription requires that program to use an LLM gateway"
            );
        }
        if subscription && llm_gateway.is_some_and(|gateway| gateway.authentication.is_none()) {
            anyhow::bail!(
                "programs.{name}.auth subscription requires oidc or controllerJwt gateway authentication"
            );
        }
    }

    if let Some(open_code) = &programs.open_code
        && llm_gateway.is_some()
        && open_code.use_llm_gateway
    {
        let model = open_code
            .model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
            .context("OpenCode requires model when llmGateway is configured")?;
        if !open_code.models.contains_key(model) {
            anyhow::bail!("OpenCode model {model} is not declared in models");
        }
    }
    Ok(())
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]")
}

/// Returns whether a caller-provided LLM gateway client identifier is valid.
pub fn valid_client_id(client_id: &str) -> bool {
    !client_id.is_empty()
        && client_id.len() <= 64
        && client_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

#[cfg(test)]
mod tests {
    use super::{
        ControllerDaemonConfig, LlmGatewayAuthentication, load_controller, parse_controller,
        parse_daemon,
    };

    #[test]
    fn daemon_configuration_supports_local_and_managed_options() {
        let document = r#"
controller:
  address: https://127.0.0.1:8443
llmGateway:
  url: http://127.0.0.1:8080
programs: { claudeCode: { useLlmGateway: false } }
"#;

        let daemon = parse_daemon(document).expect("valid daemon configuration");
        assert!(daemon.controller.is_some());
        assert!(daemon.llm_gateway.is_some());
        assert!(!daemon.programs.claude_code.unwrap().use_llm_gateway);
    }

    #[test]
    fn daemon_controller_requires_https() {
        let plaintext = r#"
controller:
  address: http://controller.example.com
"#;
        assert!(parse_daemon(plaintext).is_err());

        let valid = r#"
controller:
  address: https://controller.example.com
"#;
        assert!(parse_daemon(valid).is_ok());
    }

    #[test]
    fn controller_tls_accepts_explicit_files_and_directory_shorthand() {
        let explicit = r#"
tls:
  certificate: /server.pem
  key: /server-key.pem
  clientCaCertificate: /device-ca.pem
  clientCaKey: /device-ca-key.pem
oidc:
  issuer: https://idp.example.com
  clientId: agentdesktop
"#;
        parse_controller(explicit).expect("explicit TLS files");

        let directory = r#"
tls: /etc/agentdesktop/tls
oidc:
  issuer: https://idp.example.com
  clientId: agentdesktop
"#;
        let controller = parse_controller(directory).expect("TLS directory shorthand");
        assert_eq!(
            controller.tls.files().certificate,
            std::path::PathBuf::from("/etc/agentdesktop/tls/controller.pem")
        );
    }

    #[test]
    fn controller_daemon_configuration_supports_file_and_database_sources() {
        let file = parse_controller(
            r#"
tls: /etc/agentdesktop/tls
oidc:
    issuer: https://idp.example.com
    clientId: agentdesktop
daemonConfig:
    path: daemon.yaml
    revision: 4
"#,
        )
        .expect("file-backed daemon configuration");
        assert!(matches!(
            file.daemon_config,
            Some(ControllerDaemonConfig::File(_))
        ));

        let database = parse_controller(
            r#"
tls: /etc/agentdesktop/tls
oidc:
    issuer: https://idp.example.com
    clientId: agentdesktop
daemonConfig:
    database:
        seedPath: daemon.yaml
        seedRevision: 4
"#,
        )
        .expect("database-backed daemon configuration");
        let Some(ControllerDaemonConfig::Database { database }) = database.daemon_config else {
            panic!("expected database-backed daemon configuration");
        };
        assert_eq!(
            database.seed_path,
            Some(std::path::PathBuf::from("daemon.yaml"))
        );
        assert_eq!(database.seed_revision, 4);

        let mixed = r#"
tls: /etc/agentdesktop/tls
oidc:
    issuer: https://idp.example.com
    clientId: agentdesktop
daemonConfig:
    path: daemon.yaml
    database: {}
"#;
        assert!(parse_controller(mixed).is_err());
    }

    #[test]
    fn controller_database_seed_path_is_relative_to_controller_config() {
        let directory = std::env::temp_dir().join(format!(
            "agentdesktop-controller-config-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create controller config directory");
        let config_path = directory.join("controller.yaml");
        std::fs::write(
            &config_path,
            r#"
tls: tls
oidc:
    issuer: https://idp.example.com
    clientId: agentdesktop
daemonConfig:
    database:
        seedPath: fleet.yaml
"#,
        )
        .expect("write controller configuration");

        let config = load_controller(&config_path).expect("load controller configuration");
        let Some(ControllerDaemonConfig::Database { database }) = config.daemon_config else {
            panic!("expected database-backed daemon configuration");
        };
        assert_eq!(database.seed_path, Some(directory.join("fleet.yaml")));

        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn checked_in_examples_use_their_declared_configuration_surface() {
        parse_controller(include_str!("../../../examples/claude/controller.yaml"))
            .expect("controller example");
        parse_daemon(include_str!("../../../examples/claude/agentdesktop.yaml"))
            .expect("controller-connected daemon example");
        parse_daemon(include_str!("../../../examples/claude/claude-code.yaml"))
            .expect("Claude Code daemon configuration example");
        parse_daemon(include_str!(
            "../../../examples/claude-subscription/config.yaml"
        ))
        .expect("Claude subscription user configuration example");
        parse_daemon(include_str!("../../../examples/standalone/config.yaml"))
            .expect("standalone daemon configuration example");
    }

    #[test]
    fn standalone_oidc_uses_simple_native_client_defaults() {
        let daemon = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: https://login.example.com
    clientId: agentdesktop
"#,
        )
        .expect("valid standalone OIDC configuration");

        assert!(daemon.controller.is_none());
        let Some(LlmGatewayAuthentication::Oidc {
            redirect_uri,
            scopes,
            ..
        }) = daemon
            .llm_gateway
            .and_then(|gateway| gateway.authentication)
        else {
            panic!("expected OIDC authentication");
        };
        assert_eq!(redirect_uri, "http://127.0.0.1:51327/callback");
        assert_eq!(scopes, ["openid", "offline_access"]);

        let remote_plaintext = r#"llmGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: http://login.example.com
    clientId: agentdesktop
    allowInsecure: true
"#;
        assert!(
            parse_daemon(remote_plaintext)
                .unwrap_err()
                .to_string()
                .contains("loopback")
        );
    }

    #[test]
    fn claude_subscription_composes_with_oidc() {
        let daemon = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: https://login.example.com
    clientId: agentdesktop
programs:
  claudeCode:
    auth: subscription
"#,
        )
        .expect("valid Claude subscription and OIDC configuration");

        let gateway = daemon.llm_gateway.expect("LLM gateway");
        let Some(LlmGatewayAuthentication::Oidc {
            redirect_uri,
            scopes,
            ..
        }) = gateway.authentication
        else {
            panic!("expected OIDC authentication");
        };
        assert_eq!(redirect_uri, "http://127.0.0.1:51327/callback");
        assert_eq!(scopes, ["openid", "offline_access"]);
        assert_eq!(
            daemon.programs.claude_code.unwrap().auth,
            Some(super::ProgramAuthentication::Subscription)
        );
    }

    #[test]
    fn subscription_requires_gateway_identity_authentication() {
        let error = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  claudeCode:
    auth: subscription
"#,
        )
        .expect_err("subscription without identity must fail");
        assert!(format!("{error:#}").contains("requires oidc or controllerJwt"));
    }

    #[test]
    fn claude_subscription_composes_with_controller_jwt() {
        let daemon = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-code]
programs:
  claudeCode:
    auth: subscription
"#,
        )
        .expect("valid Claude subscription and controller JWT configuration");

        let gateway = daemon.llm_gateway.expect("LLM gateway");
        assert!(matches!(
            gateway.authentication,
            Some(LlmGatewayAuthentication::ControllerJwt { .. })
        ));
        assert_eq!(
            daemon.programs.claude_code.unwrap().auth,
            Some(super::ProgramAuthentication::Subscription)
        );
    }

    #[test]
    fn controller_jwt_requires_an_explicit_valid_client_allowlist() {
        let missing = r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
"#;
        let error = parse_daemon(missing).expect_err("missing allowlist must fail");
        assert!(format!("{error:#}").contains("allowedClientIds"));

        let invalid = r#"llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: ["not a client"]
"#;
        let error = parse_daemon(invalid).expect_err("invalid allowlist entry must fail");
        assert!(format!("{error:#}").contains("invalid LLM gateway client ID"));
    }

    #[test]
    fn controller_requires_secure_remote_transports() {
        assert!(
            parse_controller(
                r#"
fleetListen: 0.0.0.0:8443
tls: /etc/agentdesktop/tls
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
tls: /etc/agentdesktop/tls
oidc:
  issuer: http://idp.example.com
  clientId: agentdesktop
"#,
        )
        .expect("explicit insecure development configuration");
    }

    #[test]
    fn rejects_an_invalid_llm_gateway_url() {
        let error = parse_daemon(
            r#"
llmGateway:
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
llmGateway:
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
