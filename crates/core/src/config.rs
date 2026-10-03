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
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DaemonConfig {
    /// Local daemon startup settings. Not accepted in controller-delivered policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<DaemonStartupConfig>,
    /// Controller connection settings. Omit this field to run without fleet management.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<ControllerConnectionConfig>,
    /// LLM gateway used by managed developer tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_gateway: Option<LlmGatewayConfig>,
    /// Local execution sandbox required for managed developer tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxConfig>,
    /// Telemetry collected from managed developer tools.
    #[serde(default, skip_serializing_if = "TelemetryConfig::is_empty")]
    pub telemetry: TelemetryConfig,
    /// Per-program settings reconciled on this device.
    #[serde(default, skip_serializing_if = "ProgramsConfig::is_empty")]
    pub programs: ProgramsConfig,
    /// Interval between inventory refreshes. Defaults to `15m`, and must be
    /// greater than zero.
    ///
    /// Discovery walks user home directories and developer-tool configuration
    /// files, so this trades inventory freshness against local disk activity.
    #[serde(default = "default_inventory_interval", with = "humantime_serde")]
    #[cfg_attr(feature = "schema", schemars(with = "String"))]
    pub inventory_interval: Duration,
}

/// Settings read only at local daemon startup. Paths use the process working directory.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DaemonStartupConfig {
    /// Manage the current user’s tool settings instead of system settings.
    #[serde(default)]
    pub user: bool,
    /// Persistent daemon state directory.
    #[serde(default)]
    pub state_dir: Option<PathBuf>,
    /// Local API Unix socket or Windows named pipe.
    #[serde(default)]
    pub socket: Option<PathBuf>,
    /// Override the OIDC callback bind address.
    #[serde(default)]
    pub oidc_callback_listen: Option<SocketAddr>,
    /// Claude Code paths.
    #[serde(default)]
    pub claude_code: ToolConfigPath,
    /// Claude Desktop paths.
    #[serde(default)]
    pub claude_desktop: ClaudeDesktopStartupConfig,
    /// Codex paths.
    #[serde(default)]
    pub codex: ToolConfigPath,
    /// OpenCode paths.
    #[serde(default)]
    pub open_code: OpenCodeStartupConfig,
    /// Grok Build paths.
    #[serde(default)]
    pub grok: ToolConfigPath,
    /// GitHub Copilot CLI paths (`config` = the `providers.json` to manage;
    /// defaults to `COPILOT_PROVIDERS_CONFIG`, `$COPILOT_HOME/providers.json`,
    /// then `~/.copilot/providers.json`).
    #[serde(default)]
    pub copilot: ToolConfigPath,
    /// VS Code paths (`config` = the `chatLanguageModels.json` to manage under
    /// the `ownModels` variant of `copilotChat`, `settings` = the user
    /// `settings.json` to manage under the `githubModels` variant; each
    /// defaults to its file inside the per-OS VS Code user profile
    /// directory).
    #[serde(default)]
    pub vscode: VsCodeStartupConfig,
    /// Local loopback LLM proxy.
    #[serde(default)]
    pub llm_proxy: LlmProxyStartupConfig,
}

/// Local loopback LLM proxy settings. User mode only: the proxy hands out the
/// current user's gateway credential, so a system daemon never runs one.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LlmProxyStartupConfig {
    /// Loopback address to listen on. Unset disables the proxy. Rejected in system mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<SocketAddr>,
    /// Credential policy client ID used by the proxy. Defaults to `vscode`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

/// Local Claude Desktop paths.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeDesktopStartupConfig {
    /// Configuration file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<PathBuf>,
    /// Credential helper path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_helper: Option<PathBuf>,
}

/// Local tool configuration path.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ToolConfigPath {
    /// Configuration file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<PathBuf>,
}

/// VS Code paths: own type (not the shared `ToolConfigPath`) because the
/// `githubModels` variant of `copilotChat` manages a second
/// file, the user's `settings.json`, alongside `chatLanguageModels.json`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VsCodeStartupConfig {
    /// `chatLanguageModels.json` to manage (the `ownModels` variant of
    /// `copilotChat`). Defaults to that file inside the per-OS VS Code user
    /// profile directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<PathBuf>,
    /// The user `settings.json` to manage (the `githubModels` variant of
    /// `copilotChat`). Defaults to that file inside the per-OS VS Code user
    /// profile directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<PathBuf>,
}

/// Local OpenCode paths.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OpenCodeStartupConfig {
    /// Configuration file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<PathBuf>,
    /// Credential plugin path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<PathBuf>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            daemon: None,
            controller: None,
            llm_gateway: None,
            sandbox: None,
            telemetry: TelemetryConfig::default(),
            programs: ProgramsConfig::default(),
            inventory_interval: default_inventory_interval(),
        }
    }
}

/// Local execution restrictions applied to managed developer tools.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SandboxConfig {
    /// Network destinations available to sandboxed commands.
    #[serde(default)]
    pub network: SandboxNetworkConfig,
    /// Filesystem access available to sandboxed commands.
    #[serde(default)]
    pub filesystem: SandboxFilesystemConfig,
}

/// Network restrictions for sandboxed commands.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SandboxNetworkConfig {
    /// Domains sandboxed commands may contact. An empty set disables network access.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub allowed_domains: BTreeSet<String>,
}

/// Filesystem restrictions for sandboxed commands.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SandboxFilesystemConfig {
    /// Additional paths sandboxed commands may modify.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable: Vec<PathBuf>,
    /// Paths sandboxed commands may neither read nor modify.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub denied: Vec<PathBuf>,
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
    /// Base URL the local LLM proxy forwards to, when it differs from `url`.
    ///
    /// `url` is shared by every program that sets `useLlmGateway`, so it can
    /// only carry one path prefix. A gateway that puts each provider behind its
    /// own prefix therefore cannot serve both a program and the proxy from one
    /// value. Setting this leaves `url` to the programs and gives the proxy its
    /// own target. The same rules as for `url` apply.
    #[serde(rename = "proxyUrl", default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub proxy_url: Option<Url>,
    /// GitHub App OAuth used by the local proxy for the x-llm-token header.
    #[serde(
        rename = "githubOAuth",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub github_oauth: Option<GitHubOAuthConfig>,
}

/// GitHub App user authorization for Copilot requests through the local proxy.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitHubOAuthConfig {
    /// Where the GitHub credential comes from.
    #[serde(default)]
    pub source: GitHubTokenSource,
    /// GitHub App client ID. Required when `source` is `deviceFlow`, where the
    /// App must also enable Device Flow. Unused when `source` is `request`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
}

/// Where the local proxy gets the credential it puts in `x-llm-token`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum GitHubTokenSource {
    /// The daemon obtains its own token through GitHub App device authorization.
    #[default]
    DeviceFlow,
    /// The calling client already holds a credential and sends it; the proxy
    /// moves it aside and adds the gateway identity. Used by clients such as
    /// VS Code Copilot that manage their own GitHub session.
    Request,
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

fn default_inventory_interval() -> Duration {
    Duration::from_secs(15 * 60)
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
    /// Grok Build managed configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grok: Option<GrokConfig>,
    /// GitHub Copilot CLI managed configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copilot: Option<CopilotConfig>,
    /// VS Code Copilot Chat managed configuration (own models through the
    /// loopback proxy, or GitHub's models through the gateway).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vscode: Option<VsCodeConfig>,
}

impl ProgramsConfig {
    fn is_empty(&self) -> bool {
        self.claude_code.is_none()
            && self.claude_desktop.is_none()
            && self.codex.is_none()
            && self.open_code.is_none()
            && self.grok.is_none()
            && self.copilot.is_none()
            && self.vscode.is_none()
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

/// Settings reconciled into Grok Build's organization-managed configuration.
///
/// Values under `managedConfig` are written to Grok's `managed_config.toml`.
/// When generated LLM-gateway settings overlap with those values,
/// agentdesktop's generated values take precedence.
/// Only system mode is supported. Grok can delete or replace the user-level
/// managed file during startup, so `--user` rejects `programs.grok`.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrokConfig {
    /// Whether this program uses the top-level LLM gateway.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub use_llm_gateway: bool,
    /// Catalog ID and API model used when pointing Grok at the LLM gateway.
    ///
    /// This is required when a top-level `llmGateway` is configured. If `models`
    /// is empty, agentdesktop creates a catalog entry with this ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Extra Grok `[model.<id>]` catalog entries, keyed by catalog ID.
    ///
    /// Each value is an arbitrary Grok model object. Generated gateway
    /// `base_url` and `auth_provider` values take precedence. When gateway
    /// authentication is configured, `api_key` and `env_key` are removed from
    /// these entries, including values supplied through `managedConfig`.
    /// When this map is non-empty, `model` must name one of its keys.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, serde_json::Value>,
    /// Arbitrary values written to Grok's organization-managed TOML configuration.
    ///
    /// Use Grok's native snake_case configuration keys. TOML has no null value,
    /// so null values cannot be reconciled.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub managed_config: BTreeMap<String, serde_json::Value>,
}

/// Settings reconciled into GitHub Copilot CLI's `providers.json`.
///
/// `providers.json` is merged, never owned outright: the CLI has no
/// managed-settings equivalent for providers, and users may have their own
/// BYOK entries. See `provider::copilot::reconcile` in the agent crate for
/// how this is turned into the merged document. User mode only: the file
/// lives in the user's Copilot directory, so a system daemon rejects the
/// program.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CopilotConfig {
    /// Whether this program uses the top-level LLM gateway.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub use_llm_gateway: bool,
    /// Copilot CLI model entries, keyed by the model ID the CLI shows
    /// (`copilot --model agentdesktop/<id>`).
    ///
    /// At least one is required when a top-level `llmGateway` is configured.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, CopilotModel>,
}

/// One Copilot CLI model entry. The entry's `id` is the map key; `provider`
/// and `modelId` are the typed fields below; any other key (for example
/// `wireModel`, the name sent upstream when it differs) is passed through to
/// the CLI unchanged.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CopilotModel {
    /// Which managed provider entry serves the model.
    #[serde(default)]
    pub provider: CopilotProvider,
    /// The CLI's `modelId` for the entry. Defaults to the entry's ID. The
    /// name sent to the gateway is `wireModel` when that pass-through key is
    /// set, else this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    /// Further Copilot CLI model keys, passed through unchanged.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// The two provider entries agentdesktop writes into `providers.json`, one per
/// API shape the proxy's `/copilot-cli` route serves.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum CopilotProvider {
    /// OpenAI-compatible, `http://<listen>/copilot-cli/v1`.
    #[default]
    #[serde(rename = "agentdesktop")]
    Agentdesktop,
    /// Anthropic, `http://<listen>/copilot-cli`.
    #[serde(rename = "agentdesktop-anthropic")]
    AgentdesktopAnthropic,
}

impl CopilotProvider {
    /// The provider entry's `name` in `providers.json`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Agentdesktop => CopilotConfig::PROVIDER_OPENAI,
            Self::AgentdesktopAnthropic => CopilotConfig::PROVIDER_ANTHROPIC,
        }
    }
}

impl CopilotConfig {
    /// Provider name for the OpenAI-shaped route (`/copilot-cli/v1`).
    pub const PROVIDER_OPENAI: &'static str = "agentdesktop";
    /// Provider name for the Anthropic-shaped route (`/copilot-cli`).
    pub const PROVIDER_ANTHROPIC: &'static str = "agentdesktop-anthropic";
    /// The entry's ID comes from the map key; a pass-through key must not set it.
    pub const RESERVED_MODEL_KEYS: [&'static str; 1] = ["id"];

    /// The model objects written to `providers.json`, in ID order: `id`,
    /// `provider`, `modelId` (defaulting to the ID) and the pass-through keys.
    pub fn model_documents(&self) -> Vec<serde_json::Value> {
        self.models
            .iter()
            .map(|(id, model)| {
                let mut object = serde_json::Map::new();
                object.insert(
                    "provider".to_owned(),
                    serde_json::Value::String(model.provider.name().to_owned()),
                );
                object.insert("id".to_owned(), serde_json::Value::String(id.clone()));
                object.insert(
                    "modelId".to_owned(),
                    serde_json::Value::String(model.model_id.clone().unwrap_or_else(|| id.clone())),
                );
                for (key, value) in &model.extra {
                    object.insert(key.clone(), value.clone());
                }
                serde_json::Value::Object(object)
            })
            .collect()
    }

    /// Rejects an empty model ID, an empty `modelId`, a pass-through `id`
    /// and a pass-through `apiKey`. Called from daemon config validation, so
    /// the controller refuses the config before any device sees it.
    pub fn validate(&self) -> anyhow::Result<()> {
        for (id, model) in &self.models {
            if id.trim().is_empty() {
                anyhow::bail!("programs.copilot.models has an entry with an empty ID");
            }
            if model
                .model_id
                .as_deref()
                .is_some_and(|value| value.trim().is_empty())
            {
                anyhow::bail!("programs.copilot.models.{id}.modelId must not be empty");
            }
            if let Some(key) = model.extra.keys().find(|key| {
                Self::RESERVED_MODEL_KEYS
                    .iter()
                    .any(|reserved| reserved.eq_ignore_ascii_case(key))
            }) {
                anyhow::bail!(
                    "programs.copilot.models.{id}.{key} is set by agentdesktop and cannot be overridden"
                );
            }
            if model
                .extra
                .keys()
                .any(|key| key.eq_ignore_ascii_case("apiKey"))
            {
                anyhow::bail!(
                    "programs.copilot.models.{id}.apiKey is not allowed: the local proxy adds the gateway credential"
                );
            }
        }
        Ok(())
    }
}

/// Settings reconciled into VS Code's Copilot Chat configuration. Under
/// `copilotChat: ownModels` this is `chatLanguageModels.json`, pointing
/// Copilot Chat's built-in "Custom Endpoint" model provider at the local LLM
/// proxy's `/vscode-copilot` route; under `copilotChat: githubModels`
/// it is the user `settings.json`, pointing Copilot Chat's
/// CAPI endpoint at the loopback proxy's `/vscode-copilot-capi/<pairing>`
/// route so VS Code keeps GitHub's own models and its own Copilot token while
/// the daemon adds the gateway identity. See `provider::vscode::reconcile`
/// and `provider::vscode::settings` in the agent crate for how each is turned
/// into its merged document. User mode only: both files live in the user's
/// VS Code profile directory, so a system daemon rejects the program.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VsCodeConfig {
    /// Whether this program uses the top-level LLM gateway.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub use_llm_gateway: bool,
    /// Which Copilot Chat model source VS Code is pointed at: agentdesktop's
    /// own custom models (`ownModels`), or GitHub's own models reached
    /// through the gateway (`githubModels`).
    #[serde(default)]
    pub copilot_chat: VsCodeCopilotChat,
    /// Custom model entries exposed to VS Code's Copilot Chat model picker,
    /// keyed by the model ID VS Code sends as `model`. Only meaningful under
    /// `copilotChat: ownModels`; `githubModels` rejects a non-empty map.
    ///
    /// At least one is required when a top-level `llmGateway` is configured
    /// and `copilotChat` is `ownModels`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, VsCodeModel>,
}

/// Which Copilot Chat model source VS Code is pointed at.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum VsCodeCopilotChat {
    /// Custom models served through the local LLM proxy's `/vscode-copilot`
    /// route (VS Code's "Custom Endpoint" provider).
    #[default]
    OwnModels,
    /// GitHub's own models, reached through the gateway's pass-through route
    /// (`llmGateway.proxyUrl`) with the user's GitHub Copilot token forwarded:
    /// the daemon points `github.copilot.advanced.debug.overrideCapiUrl` in
    /// the user `settings.json` at the loopback proxy's
    /// `/vscode-copilot-capi/<pairing>` route. `models` must be empty.
    GithubModels,
}

/// One VS Code custom model entry. The entry's `id` is the map key; other
/// typed fields below match VS Code's `chatLanguageModels.json` model shape;
/// any other key is passed through to VS Code unchanged.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct VsCodeModel {
    /// Display name shown in VS Code's model picker. Defaults to
    /// `"<id> (agentdesktop)"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Whether the model supports tool calling.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub tool_calling: bool,
    /// Whether the model supports image input.
    #[serde(default)]
    pub vision: bool,
    /// Maximum input tokens accepted by the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    /// Maximum output tokens produced by the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Further VS Code model keys, passed through unchanged.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

impl Default for VsCodeModel {
    fn default() -> Self {
        Self {
            name: None,
            tool_calling: true,
            vision: false,
            max_input_tokens: None,
            max_output_tokens: None,
            extra: BTreeMap::new(),
        }
    }
}

impl VsCodeConfig {
    /// The `name` of the single vendor entry agentdesktop owns in
    /// `chatLanguageModels.json`, matching the Copilot CLI provider's name.
    pub const VENDOR_NAME: &'static str = "agentdesktop";
    /// Keys agentdesktop sets on every model entry; a pass-through key must
    /// not set them.
    pub const RESERVED_MODEL_KEYS: [&'static str; 3] = ["id", "url", "requestHeaders"];

    /// The configured part of each model object in the vendor entry's
    /// `models` array, in ID order: `id`, `name` (defaulting to
    /// `"<id> (agentdesktop)"`), `toolCalling`, `vision`, the optional token
    /// limits and the pass-through keys. The daemon adds the proxy `url` and
    /// the pairing `requestHeaders`; the core crate knows neither.
    pub fn model_documents(&self) -> Vec<serde_json::Value> {
        self.models
            .iter()
            .map(|(id, model)| {
                let mut object = serde_json::Map::new();
                object.insert("id".to_owned(), serde_json::Value::String(id.clone()));
                object.insert(
                    "name".to_owned(),
                    serde_json::Value::String(
                        model
                            .name
                            .clone()
                            .unwrap_or_else(|| format!("{id} ({})", Self::VENDOR_NAME)),
                    ),
                );
                object.insert(
                    "toolCalling".to_owned(),
                    serde_json::Value::Bool(model.tool_calling),
                );
                object.insert("vision".to_owned(), serde_json::Value::Bool(model.vision));
                if let Some(limit) = model.max_input_tokens {
                    object.insert("maxInputTokens".to_owned(), serde_json::Value::from(limit));
                }
                if let Some(limit) = model.max_output_tokens {
                    object.insert("maxOutputTokens".to_owned(), serde_json::Value::from(limit));
                }
                for (key, value) in &model.extra {
                    object.insert(key.clone(), value.clone());
                }
                serde_json::Value::Object(object)
            })
            .collect()
    }

    /// Rejects an empty model ID, a pass-through `id`, `url` or
    /// `requestHeaders`, a pass-through `apiKey`, and a non-empty `models`
    /// under `copilotChat: githubModels` (that variant has no custom models
    /// of its own; GitHub's own model picker is used instead). Called from
    /// daemon config validation, so the controller refuses the config before
    /// any device sees it.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.copilot_chat == VsCodeCopilotChat::GithubModels && !self.models.is_empty() {
            anyhow::bail!(
                "programs.vscode.models is not allowed when copilotChat is githubModels: GitHub's own models are used instead"
            );
        }
        for (id, model) in &self.models {
            if id.trim().is_empty() {
                anyhow::bail!("programs.vscode.models has an entry with an empty ID");
            }
            if model
                .extra
                .keys()
                .any(|key| key.eq_ignore_ascii_case("apiKey"))
            {
                anyhow::bail!(
                    "programs.vscode.models.{id}.apiKey is not allowed: the local proxy adds the gateway credential"
                );
            }
            if let Some(key) = model.extra.keys().find(|key| {
                Self::RESERVED_MODEL_KEYS
                    .iter()
                    .any(|reserved| reserved.eq_ignore_ascii_case(key))
            }) {
                anyhow::bail!(
                    "programs.vscode.models.{id}.{key} is set by agentdesktop and cannot be overridden"
                );
            }
        }
        Ok(())
    }
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
    parse_local_daemon(&contents)
        .with_context(|| format!("parse configuration from {}", path.display()))
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
    let config = parse_local_daemon(contents)?;
    if config.daemon.is_some() {
        anyhow::bail!("daemon startup settings are only allowed in the local configuration file");
    }
    Ok(config)
}

fn parse_local_daemon(contents: &str) -> anyhow::Result<DaemonConfig> {
    let config: DaemonConfig =
        crate::serdes::yamlviajson::from_str(contents).context("parse daemon configuration")?;
    if let Some(controller) = &config.controller
        && !controller.address.starts_with("https://")
    {
        anyhow::bail!("controller address must use HTTPS");
    }
    if config.inventory_interval.is_zero() {
        anyhow::bail!("inventoryInterval must be greater than zero");
    }
    validate_daemon(
        config.llm_gateway.as_ref(),
        config.sandbox.as_ref(),
        &config.programs,
    )?;
    Ok(config)
}

impl DaemonConfig {
    /// Returns whether this configuration manages no gateway or developer tools.
    pub fn is_empty(&self) -> bool {
        self.llm_gateway.is_none()
            && self.sandbox.is_none()
            && self.telemetry.is_empty()
            && self.programs.is_empty()
    }
}

fn validate_daemon(
    llm_gateway: Option<&LlmGatewayConfig>,
    sandbox: Option<&SandboxConfig>,
    programs: &ProgramsConfig,
) -> anyhow::Result<()> {
    if sandbox.is_some() {
        if programs.claude_desktop.is_some() {
            anyhow::bail!("sandbox is not supported for Claude Desktop");
        }
        if programs.open_code.is_some() {
            anyhow::bail!("sandbox is not supported for OpenCode");
        }
        if programs.grok.is_some() {
            anyhow::bail!("sandbox is not supported for Grok Build");
        }
        if programs.copilot.is_some() {
            anyhow::bail!("sandbox is not supported for GitHub Copilot CLI");
        }
        if programs.vscode.is_some() {
            anyhow::bail!("sandbox is not supported for VS Code");
        }
    }
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
        if let Some(proxy_url) = &gateway.proxy_url {
            if !matches!(proxy_url.scheme(), "http" | "https") {
                anyhow::bail!(
                    "LLM gateway proxyUrl must use HTTP or HTTPS, got {}",
                    proxy_url.scheme()
                );
            }
            if proxy_url.host().is_none() {
                anyhow::bail!("LLM gateway proxyUrl must include a host");
            }
            if !proxy_url.username().is_empty() || proxy_url.password().is_some() {
                anyhow::bail!("LLM gateway proxyUrl cannot include credentials");
            }
            if proxy_url.query().is_some() || proxy_url.fragment().is_some() {
                anyhow::bail!("LLM gateway proxyUrl cannot include a query or fragment");
            }
        }
        if let Some(github) = &gateway.github_oauth {
            match github.source {
                GitHubTokenSource::DeviceFlow => {
                    let client_id = github.client_id.as_deref().unwrap_or_default();
                    if client_id.trim().is_empty() {
                        anyhow::bail!(
                            "llmGateway.githubOAuth.clientId is required when source is deviceFlow"
                        );
                    }
                }
                // clientId is accepted and ignored here, so that switching
                // source back and forth does not require editing two fields.
                GitHubTokenSource::Request => {}
            }
            if gateway.authentication.is_none() {
                anyhow::bail!("llmGateway.githubOAuth requires gateway authentication");
            }
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
    if let Some(grok) = &programs.grok
        && llm_gateway.is_some()
        && grok.use_llm_gateway
    {
        let model = grok
            .model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
            .context("Grok Build requires model when llmGateway is configured")?;
        if !grok.models.is_empty() && !grok.models.contains_key(model) {
            anyhow::bail!("Grok Build model {model} is not declared in models");
        }
    }
    if let Some(copilot) = &programs.copilot {
        copilot.validate()?;
        if llm_gateway.is_some() && copilot.use_llm_gateway && copilot.models.is_empty() {
            anyhow::bail!(
                "GitHub Copilot CLI requires at least one entry in models when llmGateway is configured"
            );
        }
    }
    if let Some(vscode) = &programs.vscode {
        vscode.validate()?;
        // Matched, not defaulted: a new model source must decide its own rules.
        match vscode.copilot_chat {
            VsCodeCopilotChat::OwnModels => {
                if llm_gateway.is_some() && vscode.use_llm_gateway && vscode.models.is_empty() {
                    anyhow::bail!(
                        "VS Code requires at least one entry in models when llmGateway is configured"
                    );
                }
            }
            VsCodeCopilotChat::GithubModels => {
                if vscode.use_llm_gateway
                    && llm_gateway.is_some_and(|gateway| gateway.proxy_url.is_none())
                {
                    anyhow::bail!(
                        "VS Code copilotChat githubModels requires llmGateway.proxyUrl when the gateway is used"
                    );
                }
            }
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
    use super::{DaemonConfig, LlmGatewayAuthentication, parse_controller, parse_daemon};

    #[test]
    fn startup_settings_are_local_only_and_reject_unknown_fields() {
        let yaml = "daemon:\n  user: true\n  stateDir: /tmp/device\n  socket: /tmp/device.sock\n";
        let config = super::parse_local_daemon(yaml).unwrap();
        let startup = config.daemon.unwrap();
        assert!(startup.user);
        assert_eq!(
            startup.state_dir.unwrap(),
            std::path::Path::new("/tmp/device")
        );
        assert_eq!(
            startup.socket.unwrap(),
            std::path::Path::new("/tmp/device.sock")
        );
        assert!(
            parse_daemon(yaml)
                .unwrap_err()
                .to_string()
                .contains("only allowed in the local")
        );
        assert!(super::parse_local_daemon("daemon: { stateDr: /tmp/device }").is_err());
    }

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
    fn daemon_inventory_interval_defaults_and_parses_durations() {
        let default = parse_daemon("programs: {}").expect("valid daemon configuration");
        assert_eq!(
            default.inventory_interval,
            std::time::Duration::from_secs(15 * 60)
        );
        assert_eq!(
            DaemonConfig::default().inventory_interval,
            default.inventory_interval
        );

        let configured = parse_daemon("inventoryInterval: 2m").expect("valid daemon configuration");
        assert_eq!(
            configured.inventory_interval,
            std::time::Duration::from_secs(120)
        );
    }

    #[test]
    fn daemon_inventory_interval_rejects_zero() {
        let error = parse_daemon("inventoryInterval: 0s")
            .expect_err("a zero inventory interval is not schedulable");
        assert!(
            error.to_string().contains("inventoryInterval"),
            "unexpected error: {error}"
        );
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
    fn rejects_an_invalid_llm_gateway_proxy_url() {
        for (proxy_url, message) in [
            (
                "ftp://gateway.example.com",
                "proxyUrl must use HTTP or HTTPS",
            ),
            ("file:///etc/passwd", "proxyUrl must use HTTP or HTTPS"),
            (
                "https://user:secret@gateway.example.com",
                "proxyUrl cannot include credentials",
            ),
            (
                "https://gateway.example.com/?a=b",
                "proxyUrl cannot include a query",
            ),
        ] {
            let error = parse_daemon(&format!(
                "llmGateway:\n  url: https://gateway.example.com\n  proxyUrl: {proxy_url}\n"
            ))
            .expect_err(proxy_url);
            assert!(error.to_string().contains(message), "{proxy_url}: {error}");
        }
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

    #[test]
    fn grok_requires_a_model_when_using_the_gateway() {
        let error = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  grok: {}
"#,
        )
        .expect_err("Grok without a model should fail");

        assert!(
            error
                .to_string()
                .contains("Grok Build requires model when llmGateway is configured")
        );
    }

    #[test]
    fn grok_requires_a_declared_gateway_model_when_models_are_listed() {
        let error = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  grok:
    model: missing
    models:
      available: {}
"#,
        )
        .expect_err("undeclared Grok model should fail");

        assert!(
            error
                .to_string()
                .contains("Grok Build model missing is not declared in models")
        );
    }
}
