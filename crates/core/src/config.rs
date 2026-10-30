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
    /// Wrap-mode transparency policy. Top-level, not under `programs` or
    /// `daemon.containment`, because it is controller-delivered POLICY over
    /// which program executables launch wrapped, while `programs` holds
    /// per-program settings reconciliation and `daemon.containment` is the
    /// device-local startup configuration of the launch machinery.
    /// Accepted in controller-delivered
    /// policy AND the local file, unlike `daemon.containment` (local-only,
    /// startup settings, which must also be configured for the policy to
    /// take effect). With a controller only the controller-delivered block
    /// applies (a local block is ignored); a change takes effect at the
    /// next launch and the next violation-scan tick after it is applied.
    /// Without a controller the local block is read at startup only; with
    /// `--once`/`--dry-run` the block is validated but no scan runs and no
    /// posture is reported.
    ///
    /// A daemon that predates this setting rejects a configuration that
    /// contains it, so upgrade the controller and then every device before
    /// setting it in controller-delivered policy. A program field added to
    /// `containment.programs` by a later release is the same break again:
    /// upgrade before naming it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment: Option<ContainmentPolicy>,
    /// Telemetry collected from managed developer tools.
    #[serde(default, skip_serializing_if = "TelemetryConfig::is_empty")]
    pub telemetry: TelemetryConfig,
    /// Per-program settings reconciled on this device.
    #[serde(default, skip_serializing_if = "ProgramsConfig::is_empty")]
    pub programs: ProgramsConfig,
    /// Whether the device user may choose which configured programs are managed.
    ///
    /// A daemon that predates this setting rejects a configuration that
    /// contains it, so upgrade devices before setting it in
    /// controller-delivered policy.
    #[serde(default, skip_serializing_if = "ProgramSelectionConfig::is_default")]
    pub program_selection: ProgramSelectionConfig,
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
    /// Pi paths.
    #[serde(default)]
    pub pi: PiStartupConfig,
    /// Local loopback LLM proxy.
    #[serde(default)]
    pub llm_proxy: LlmProxyStartupConfig,
    /// Usage collection without a controller, from a gateway on this machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageStartupConfig>,
    /// Sandboxed launch (`agentdesktop run`): local startup settings only.
    /// The controller-delivered wrap policy lives in the TOP-LEVEL
    /// `containment` block, not here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment: Option<ContainmentStartupConfig>,
    /// Opt-in interval between periodic re-applies of the current
    /// configuration, which repair drift (a managed file deleted or edited by
    /// hand, a fixed conflict, a loosened mode) without rewriting anything
    /// unchanged. Unset means no periodic re-apply; a controller-managed
    /// device still re-applies the controller's configuration on every
    /// reconnect. Must be greater than zero and at most 30 days; read at
    /// startup only (restart the daemon after changing it).
    #[serde(default, with = "humantime_serde::option")]
    #[cfg_attr(feature = "schema", schemars(with = "Option<String>"))]
    pub reconcile_interval: Option<Duration>,
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

/// Sandboxed launch settings.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainmentStartupConfig {
    /// Windows only. Absolute path to MXC `wxc-exec.exe`; no default. Read at
    /// startup only (restart the daemon after changing it). Requires user
    /// mode; ignored with `--once` and `--dry-run`.
    pub executor: PathBuf,
    /// Outbound network for the sandboxed program: `deny` (default) or `allow`. With `deny` the box reaches only loopback and configured egress routes. Read at startup only (restart the daemon after changing it); ignored with `--once` and `--dry-run`.
    #[serde(default, skip_serializing_if = "ContainmentEgress::is_default")]
    pub egress: ContainmentEgress,
    /// Sandbox engine: `mxc` (default, the only engine). Read at startup only
    /// (restart the daemon after changing it); ignored with `--once` and `--dry-run`.
    #[serde(default, skip_serializing_if = "ContainmentEngine::is_default")]
    pub engine: ContainmentEngine,
    /// Central L7 egress endpoint (an agentgateway HTTPS listener whose
    /// per-prefix routes are the central allowlist). When set, the relay sends
    /// the box's git smart-HTTP requests to `https://<egress host>/<path>`
    /// instead of directly to the remote. CONNECT is always refused, with or
    /// without it. Read at startup only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_upstream: Option<EgressUpstreamConfig>,
    /// Git over the relay: the remotes the box may fetch from and push to, and
    /// the static credentials the relay attaches host-side. Absent = no git
    /// egress. Read at startup only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitConfig>,
}

/// The central L7 egress endpoint the git leg chains to.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EgressUpstreamConfig {
    /// HTTPS URL of the egress endpoint; must include a host and no
    /// credentials, query or fragment.
    #[cfg_attr(feature = "schema", schemars(with = "String"))]
    pub url: Url,
    /// Reserved for gateway-hop authentication, which is not available yet:
    /// startup refuses a config that sets it (the relay sends only the git
    /// credential in `Authorization`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<LlmGatewayAuthentication>,
}

/// Git repository access configuration.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitConfig {
    /// List of HTTPS URL prefixes the box may fetch from.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    /// List of HTTPS URL prefixes the box may push to (subset of allow).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub push_allow: Vec<String>,
    /// Static git credentials for allowed hosts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<GitCredentialEntry>,
    /// Git over ssh: the box's ssh signs through a filtered agent proxy (the
    /// key never enters the box) and reaches each host through a loopback
    /// leg the daemon originates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<GitSshConfig>,
}

/// Git SSH access configuration.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitSshConfig {
    /// Bare hostnames (port fixed at 22) the box may fetch from and push to
    /// over ssh; host-granular. Empty or absent: no ssh at all.
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "deserialize_lowercase_hosts"
    )]
    pub hosts: Vec<String>,
    /// SHA256 fingerprints (`SHA256:...`, as `ssh-add -l` prints them) of the
    /// agent keys offered to the box. Empty or absent: no key is offered.
    /// The literal `"*"` offers every key in the agent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
}

fn deserialize_lowercase_hosts<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let hosts: Vec<String> = Vec::deserialize(deserializer)?;
    Ok(hosts.into_iter().map(|h| h.to_ascii_lowercase()).collect())
}

/// Git credential entry for a URL prefix.
#[derive(Clone, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GitCredentialEntry {
    /// URL prefix this credential applies to.
    pub url_prefix: String,
    /// Username for HTTP basic auth.
    pub username: String,
    /// Secret (password or token) for HTTP basic auth (never serialized).
    #[serde(skip_serializing)]
    pub secret: String,
}

// The secret never reaches a log or error through `{:?}` (the containing
// configs derive Debug over this).
impl std::fmt::Debug for GitCredentialEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitCredentialEntry")
            .field("url_prefix", &self.url_prefix)
            .field("username", &self.username)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// Checks `daemon.containment`. With `windows == true` the executor must be an
/// absolute Windows path (`X:\...` or `\\server\share\...`, judged on the
/// string) to a file whose name ends in `.exe`, case-insensitively.
pub fn validate_containment_startup(
    cfg: &ContainmentStartupConfig,
    windows: bool,
) -> Result<(), String> {
    if windows {
        let text = cfg.executor.to_string_lossy();
        let bytes = text.as_bytes();
        let drive_absolute = bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'\\';
        let unc_absolute = text.starts_with("\\\\")
            && text[2..]
                .split('\\')
                .filter(|part| !part.is_empty())
                .count()
                >= 3;
        let exe = text
            .rsplit('\\')
            .next()
            .is_some_and(|name| name.to_ascii_lowercase().ends_with(".exe"));
        if !(drive_absolute || unc_absolute) || !exe {
            return Err(
                "daemon.containment.executor must be an absolute path to wxc-exec.exe".to_owned(),
            );
        }
    }

    // Validate egress_upstream URL like LlmGatewayConfig.url, HTTPS only
    // (the leg carries the git credential).
    if let Some(upstream) = &cfg.egress_upstream {
        if upstream.url.scheme() != "https" {
            return Err("daemon.containment.egressUpstream.url must be an https:// URL".to_owned());
        }
        // v1 defers gateway-hop authentication (AC 2): a configured block
        // would otherwise be silently ignored.
        if upstream.authentication.is_some() {
            return Err(
                "daemon.containment.egressUpstream.authentication is not supported yet; remove it"
                    .to_owned(),
            );
        }
        if upstream.url.host().is_none() {
            return Err("daemon.containment.egressUpstream.url must include a host".to_owned());
        }
        if upstream.url.username() != "" || upstream.url.password().is_some() {
            return Err(
                "daemon.containment.egressUpstream.url cannot include credentials".to_owned(),
            );
        }
        if upstream.url.query().is_some() {
            return Err("daemon.containment.egressUpstream.url cannot include a query".to_owned());
        }
        if upstream.url.fragment().is_some() {
            return Err(
                "daemon.containment.egressUpstream.url cannot include a fragment".to_owned(),
            );
        }
    }

    // Validate git allow/pushAllow URLs must be https:// (and parse, with a
    // host: the relay reparses them on every request).
    if let Some(git) = &cfg.git {
        for (idx, url_str) in git.allow.iter().enumerate() {
            if !Url::parse(url_str)
                .is_ok_and(|url| url.scheme() == "https" && url.host_str().is_some())
            {
                return Err(format!(
                    "daemon.containment.git.allow[{}] must be an https:// URL",
                    idx
                ));
            }
        }
        for (idx, url_str) in git.push_allow.iter().enumerate() {
            if !Url::parse(url_str)
                .is_ok_and(|url| url.scheme() == "https" && url.host_str().is_some())
            {
                return Err(format!(
                    "daemon.containment.git.pushAllow[{}] must be an https:// URL",
                    idx
                ));
            }
        }
        // With egressUpstream the relay sends only the path to the egress
        // endpoint, so a second host could never be told apart there: refuse
        // more than one distinct (host, port) across both lists (DV-8).
        if cfg.egress_upstream.is_some() {
            let mut hosts: Vec<(String, Option<u16>)> = Vec::new();
            for url_str in git.allow.iter().chain(&git.push_allow) {
                if let Ok(url) = Url::parse(url_str)
                    && let Some(host) = url.host_str()
                {
                    let key = (host.to_ascii_lowercase(), url.port_or_known_default());
                    if !hosts.contains(&key) {
                        hosts.push(key);
                    }
                }
            }
            if hosts.len() > 1 {
                return Err(
                    "daemon.containment.git: with egressUpstream set, git.allow and git.pushAllow must name a single host (the egress endpoint routes by path only)"
                        .to_owned(),
                );
            }
        }

        // Validate git.ssh configuration (D5, D6, AC 1).
        if let Some(ssh_config) = &git.ssh {
            // D6: refuse both egressUpstream and git.ssh.hosts
            if cfg.egress_upstream.is_some() && !ssh_config.hosts.is_empty() {
                return Err(
                    "daemon.containment.git.ssh: with egressUpstream set, ssh egress is not available (the egress endpoint is L7 https only)"
                        .to_owned(),
                );
            }

            // Validate hosts: bare hostname only (no scheme, path, port)
            for (idx, host) in ssh_config.hosts.iter().enumerate() {
                if !is_bare_hostname(host) {
                    return Err(format!(
                        "daemon.containment.git.ssh.hosts[{}] must be a bare hostname (no scheme, path, or port)",
                        idx
                    ));
                }
            }

            // Validate keys: must be "SHA256:..." or "*"
            for (idx, key) in ssh_config.keys.iter().enumerate() {
                if key != "*" && !key.starts_with("SHA256:") {
                    return Err(format!(
                        "daemon.containment.git.ssh.keys[{}] must be a SHA256:... fingerprint or \"*\"",
                        idx
                    ));
                }
            }
        }
    }

    Ok(())
}

/// A bare DNS hostname or IPv4 literal: dot-separated non-empty labels of
/// ASCII letters, digits and `-`, no label starting with `-`. Everything else
/// (scheme, path, port, whitespace, ssh_config patterns, `%` tokens) is
/// refused: the value is written verbatim into the generated `ssh_config`
/// and passed to `ssh-keygen -F`.
fn is_bare_hostname(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

/// Token and cost collection for a daemon without a controller. agentgateway
/// on this machine exports its access logs (`frontendPolicies.accessLog.otlp`)
/// to the daemon, which keeps this device's usage for the desktop app.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageStartupConfig {
    /// Loopback address that receives agentgateway's OTLP/gRPC export.
    pub listen: SocketAddr,
    /// Path to a file containing the bearer token agentgateway sends with each export. The token must be at least 32 characters.
    pub token_file: PathBuf,
}

/// Local Claude Desktop paths.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ClaudeDesktopStartupConfig {
    /// Configuration file. On Windows the managed settings live in the
    /// policy registry key instead, and this file is only removed when an
    /// earlier agentdesktop wrote it.
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

/// Local Pi paths.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PiStartupConfig {
    /// Model catalog file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<PathBuf>,
    /// Settings file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<PathBuf>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            daemon: None,
            controller: None,
            llm_gateway: None,
            sandbox: None,
            containment: None,
            telemetry: TelemetryConfig::default(),
            programs: ProgramsConfig::default(),
            program_selection: ProgramSelectionConfig::default(),
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
    /// What the programs behind the local LLM proxy (`programs.copilot`,
    /// `programs.vscode`) do while the daemon runs without that proxy:
    /// `failClosed` (the default) leaves their managed entries in place, so
    /// the tools stay pointed at the loopback port and fail instead
    /// of falling back to GitHub past the gateway (with no entries in place
    /// yet, the tools keep their own settings); `failOpen` removes them
    /// until the proxy is back (VS Code on `githubModels` then talks to GitHub
    /// directly, the Copilot CLI and VS Code on `ownModels` lose the gateway's
    /// models).
    #[serde(default, skip_serializing_if = "ProxyUnavailable::is_default")]
    pub when_proxy_unavailable: ProxyUnavailable,
}

/// The `llmGateway.whenProxyUnavailable` policy.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ProxyUnavailable {
    /// Keep the managed entries: the tools fail instead of bypassing the
    /// gateway.
    #[default]
    FailClosed,
    /// Remove the managed entries until the proxy is back.
    FailOpen,
}

impl ProxyUnavailable {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Outbound network posture for sandboxed programs.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ContainmentEgress {
    /// Plain outbound access; model requests still go through the local LLM proxy.
    Allow,
    /// Loopback only.
    #[default]
    Deny,
}

impl ContainmentEgress {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Sandbox engine: `mxc` (default, the only engine). Read at startup only
/// (restart the daemon after changing it); ignored with `--once` and `--dry-run`.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ContainmentEngine {
    /// MXC (Windows AppContainer).
    #[default]
    Mxc,
}

impl ContainmentEngine {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Wrap-mode transparency policy, delivered either by the controller or the
/// local file (unlike `daemon.containment`, which stays local-only). A
/// program named here launches wrapped by default; see [`ContainmentMode`].
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainmentPolicy {
    #[serde(default, skip_serializing_if = "ContainmentProgramsConfig::is_empty")]
    pub programs: ContainmentProgramsConfig,
}

/// One typed field per profiled program; not a map, so a daemon that does
/// not know a program rejects the whole policy at parse time.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainmentProgramsConfig {
    /// Wrap-mode policy for Claude Code (launch program `claude-code`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code: Option<ContainmentProgramPolicy>,
    /// Wrap-mode policy for OpenCode (launch program `opencode`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_code: Option<ContainmentProgramPolicy>,
    /// Wrap-mode policy for GitHub Copilot CLI (launch program `copilot`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copilot: Option<ContainmentProgramPolicy>,
}

impl ContainmentProgramsConfig {
    fn is_empty(&self) -> bool {
        self.claude_code.is_none() && self.open_code.is_none() && self.copilot.is_none()
    }
}

/// Per-program wrap-mode policy.
// `executable`'s string-level shape (absolute Windows path, `.exe`, no
// forbidden character, no `\Users\` segment) is validated in
// `validate_containment_policy` on every platform, because the controller
// validates on Linux; the file-name-equals-profile and not-under-home checks
// are the agent's.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ContainmentProgramPolicy {
    /// Defaults to `wrap` when a policy names the program.
    #[serde(default)]
    pub mode: ContainmentMode,
    /// Absolute Windows path to the program's managed executable (`X:\...`
    /// or `\\server\share\...`, ending in `.exe`, not under `\Users\`).
    /// Required in wrap mode; ignored (and may be omitted) in native mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executable: Option<PathBuf>,
}

/// Whether a profiled program launches into the box by default (`Wrap`) or
/// keeps its plain native launch (`Native`, the explicit per-program
/// opt-out). A third value (`off` and anything else) is refused by serde.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ContainmentMode {
    #[default]
    Wrap,
    Native,
}

/// String-level validation of a top-level `containment` block: run on every
/// platform (the controller validates on Linux), because this knob is
/// accepted in controller-delivered policy too.
pub fn validate_containment_policy(policy: &ContainmentPolicy) -> Result<(), String> {
    for (field, program) in [
        ("claudeCode", &policy.programs.claude_code),
        ("openCode", &policy.programs.open_code),
        ("copilot", &policy.programs.copilot),
    ] {
        let Some(program) = program else { continue };
        if program.mode != ContainmentMode::Wrap {
            continue;
        }
        let path_field = format!("containment.programs.{field}.executable");
        let Some(executable) = &program.executable else {
            return Err(format!("{path_field} is required in wrap mode"));
        };
        let text = executable.to_string_lossy();
        let bytes = text.as_bytes();
        let drive_absolute = bytes.len() > 2
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && bytes[2] == b'\\';
        let unc_absolute = text.starts_with("\\\\")
            && text[2..]
                .split('\\')
                .filter(|part| !part.is_empty())
                .count()
                >= 3;
        if !(drive_absolute || unc_absolute) {
            return Err(format!(
                "{path_field} must be an absolute Windows path (X:\\... or \\\\server\\share\\...)"
            ));
        }
        let ends_in_exe = text
            .rsplit('\\')
            .next()
            .is_some_and(|name| name.to_ascii_lowercase().ends_with(".exe"));
        if !ends_in_exe {
            return Err(format!("{path_field} must end in .exe"));
        }
        if text.contains(['"', '%', '!']) {
            return Err(format!(
                "{path_field} must not contain the characters \" % !"
            ));
        }
        if text.to_ascii_lowercase().contains(r"\users\") {
            return Err(format!("{path_field} must not be under \\Users\\"));
        }
    }
    Ok(())
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
        /// Sign in with the OAuth 2.0 Device Authorization Grant (RFC 8628)
        /// instead of a browser redirect to a local callback.
        ///
        /// Use this on hosts without a local browser, such as servers reached
        /// over SSH. Run `agentdesktop-headless login` to get a verification
        /// URL and user code to approve from any other device. The identity
        /// provider must support device authorization for this client.
        #[serde(
            rename = "deviceAuthorization",
            default,
            skip_serializing_if = "std::ops::Not::not"
        )]
        device_authorization: bool,
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
    /// Enroll with the OAuth 2.0 Device Authorization Grant (RFC 8628) instead
    /// of a browser redirect to a local callback.
    ///
    /// Use this on hosts without a local browser, such as servers reached over
    /// SSH. The daemon logs a verification URL and user code to approve from
    /// any other device. The identity provider must support device
    /// authorization for the controller's client.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub device_authorization: bool,
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
    /// Token and cost collection from agentgateway access logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<ControllerUsageConfig>,
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

/// Collection of LLM token and cost records that agentgateway exports to the
/// fleet API as OTLP access logs over gRPC.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControllerUsageConfig {
    /// Path to a file containing the bearer token agentgateway sends with each export. The token must be at least 32 characters.
    ///
    /// Relative paths are resolved from the controller configuration directory.
    pub token_file: PathBuf,
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

/// Whether the device user may choose which configured programs are managed.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgramSelectionConfig {
    /// Let the device user opt out of all configured programs, or choose the
    /// ones to manage, with `agentdesktop programs set`.
    /// Agentdesktop removes the settings it wrote for a program the user opts
    /// out of, and reports the program as opted out.
    ///
    /// Defaults to `false`: every configured program is managed and a choice
    /// stored on the device is ignored until this is enabled again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_user_choice: bool,
}

impl ProgramSelectionConfig {
    fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

/// Settings for developer tools managed by Agentdesktop.
///
/// A policy can mix user-only and system-only programs. Each daemon skips
/// programs its mode cannot manage, reporting them as inactive with a reason.
/// Skipped programs' files are left untouched, including during cleanup.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgramsConfig {
    /// Claude Code managed-settings configuration. Arbitrary keys are passed through directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude_code: Option<ClaudeCodeConfig>,
    /// Claude Desktop system-managed configuration. User daemons skip it.
    /// Arbitrary keys are passed through directly.
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
    /// Pi coding-agent harness managed configuration.
    ///
    /// Managed in user mode (`--user`); system daemons skip it with an inactive reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pi: Option<PiConfig>,
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
            && self.pi.is_none()
    }
}

/// MCP server allowlist entry for `allowedMcpServers`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum McpServerRule {
    /// User-assigned label; not a security control.
    ServerName(String),
    /// Command array, must be non-empty.
    ServerCommand(Vec<String>),
    /// Server URL, must be non-empty.
    ServerUrl(String),
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
    /// Allow only managed hooks. Claude Code reads this only from managed sources.
    /// Unset: not written to Claude Code's settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_managed_hooks_only: Option<bool>,
    /// Allow only managed MCP servers. Claude Code reads this only from managed sources.
    /// Unset: not written to Claude Code's settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_managed_mcp_servers_only: Option<bool>,
    /// Allowed MCP servers list. Unset: not written to Claude Code's settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_mcp_servers: Option<Vec<McpServerRule>>,
    /// Arbitrary Claude Code managed-settings values, flattened into this object.
    #[serde(default, flatten)]
    pub settings: BTreeMap<String, serde_json::Value>,
}

impl ClaudeCodeConfig {
    /// Called from daemon config validation, so the controller refuses the config before any device sees it.
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(servers) = &self.allowed_mcp_servers {
            for (i, rule) in servers.iter().enumerate() {
                match rule {
                    McpServerRule::ServerName(name) => {
                        if name.is_empty()
                            || !name
                                .chars()
                                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                        {
                            anyhow::bail!(
                                "programs.claudeCode.allowedMcpServers[{i}]: serverName must match [A-Za-z0-9_-]+"
                            );
                        }
                    }
                    McpServerRule::ServerCommand(cmd) => {
                        if cmd.is_empty() {
                            anyhow::bail!(
                                "programs.claudeCode.allowedMcpServers[{i}]: serverCommand must be a non-empty array"
                            );
                        }
                    }
                    McpServerRule::ServerUrl(url) => {
                        if url.trim().is_empty() {
                            anyhow::bail!(
                                "programs.claudeCode.allowedMcpServers[{i}]: serverUrl must be non-empty"
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }
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
/// managed file during startup, so user daemons skip it with an inactive reason.
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
/// lives in the user's Copilot directory, so system daemons skip it with an
/// inactive reason.
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
/// VS Code profile directory, so system daemons skip it with an inactive reason.
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

/// Settings reconciled into Pi's user `models.json` and `settings.json`.
///
/// Generated `providers.agentdesktop` values take precedence over overlapping
/// `managedConfig` keys. Pi stores configuration under `~/.pi/agent` (or
/// `PI_CODING_AGENT_DIR`), so management requires `--user`. System daemons skip it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PiConfig {
    /// Whether this program uses the top-level LLM gateway.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub use_llm_gateway: bool,
    /// Model ID selected when pointing Pi at the LLM gateway.
    ///
    /// This is required when a top-level `llmGateway` is configured. If `models`
    /// is empty, agentdesktop creates a catalog entry with this ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Extra Pi model objects, keyed by model ID.
    ///
    /// Each value is an arbitrary Pi `models.json` model object. The map key
    /// sets its `id`. Generated gateway `baseUrl` and `apiKey` values take
    /// precedence, including per-model URLs. When this map is non-empty,
    /// `model` must name one of its keys. Every key must contain a
    /// non-whitespace character.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub models: BTreeMap<String, serde_json::Value>,
    /// Pi API dialect for the managed provider. Defaults to `anthropic-messages`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// Arbitrary values merged into Pi's `models.json`.
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
    if let Some(usage) = &mut config.usage {
        resolve_relative(&mut usage.token_file, directory);
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

/// The longest accepted `daemon.reconcileInterval`.
const MAX_RECONCILE_INTERVAL: Duration = Duration::from_secs(30 * 24 * 60 * 60);

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
    if config
        .daemon
        .as_ref()
        .and_then(|daemon| daemon.reconcile_interval)
        .is_some_and(|interval| interval.is_zero() || interval > MAX_RECONCILE_INTERVAL)
    {
        anyhow::bail!(
            "daemon.reconcileInterval must be greater than zero and at most 30 days (leave it unset for no periodic re-apply)"
        );
    }
    if let Some(usage) = config
        .daemon
        .as_ref()
        .and_then(|daemon| daemon.usage.as_ref())
    {
        if config.controller.is_some() {
            anyhow::bail!(
                "daemon.usage is for a daemon without a controller; the controller collects usage for enrolled devices"
            );
        }
        if !usage.listen.ip().is_loopback() {
            anyhow::bail!("daemon.usage.listen must be a loopback address");
        }
    }
    if let Some(containment) = config
        .daemon
        .as_ref()
        .and_then(|daemon| daemon.containment.as_ref())
    {
        validate_containment_startup(containment, cfg!(windows)).map_err(anyhow::Error::msg)?;
    }
    validate_daemon(
        config.llm_gateway.as_ref(),
        config.sandbox.as_ref(),
        config.containment.as_ref(),
        &config.programs,
    )?;
    Ok(config)
}

impl DaemonConfig {
    /// Returns whether this configuration manages no gateway or developer tools.
    pub fn is_empty(&self) -> bool {
        self.llm_gateway.is_none()
            && self.sandbox.is_none()
            && self.containment.is_none()
            && self.telemetry.is_empty()
            && self.programs.is_empty()
    }
}

fn validate_daemon(
    llm_gateway: Option<&LlmGatewayConfig>,
    sandbox: Option<&SandboxConfig>,
    containment: Option<&ContainmentPolicy>,
    programs: &ProgramsConfig,
) -> anyhow::Result<()> {
    if let Some(containment) = containment {
        validate_containment_policy(containment).map_err(anyhow::Error::msg)?;
    }
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
        if programs.pi.is_some() {
            anyhow::bail!("sandbox is not supported for Pi");
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
                    ..
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
        if subscription
            && llm_gateway.is_some_and(|gateway| {
                matches!(
                    gateway.authentication,
                    Some(LlmGatewayAuthentication::Oidc {
                        device_authorization: true,
                        ..
                    })
                )
            })
        {
            anyhow::bail!(
                "programs.{name}.auth subscription requires a browser sign-in and cannot be combined with llmGateway OIDC deviceAuthorization"
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
    if let Some(pi) = &programs.pi
        && llm_gateway.is_some()
        && pi.use_llm_gateway
    {
        let model = pi
            .model
            .as_deref()
            .filter(|model| !model.trim().is_empty())
            .context("Pi requires model when llmGateway is configured")?;
        if pi.models.keys().any(|model| model.trim().is_empty()) {
            anyhow::bail!("Pi model IDs cannot be empty");
        }
        if !pi.models.is_empty() && !pi.models.contains_key(model) {
            anyhow::bail!("Pi model {model} is not declared in models");
        }
    }
    if let Some(claude_code) = &programs.claude_code {
        claude_code.validate()?;
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
        DaemonConfig, LlmGatewayAuthentication, load_controller, parse_controller, parse_daemon,
    };

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
    fn pi_startup_paths_are_local_only() {
        let yaml = "daemon:\n  pi:\n    models: custom/pi-models.json\n    settings: custom/pi-settings.json\n";
        let config = super::parse_local_daemon(yaml).expect("local Pi paths are supported");
        let value = serde_json::to_value(config).unwrap();
        assert_eq!(value["daemon"]["pi"]["models"], "custom/pi-models.json");
        assert_eq!(value["daemon"]["pi"]["settings"], "custom/pi-settings.json");
        assert!(
            parse_daemon(yaml)
                .unwrap_err()
                .to_string()
                .contains("only allowed in the local")
        );
        assert!(super::parse_local_daemon("daemon: { pi: { model: wrong } }").is_err());
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
    fn user_program_choice_is_off_unless_the_policy_allows_it() {
        let default = parse_daemon("programs: {}").expect("valid daemon configuration");
        assert!(!default.program_selection.allow_user_choice);
        assert!(
            !serde_json::to_string(&default)
                .unwrap()
                .contains("programSelection"),
            "the default is not serialized"
        );

        let allowed = parse_daemon("programSelection: { allowUserChoice: true }")
            .expect("valid daemon configuration");
        assert!(allowed.program_selection.allow_user_choice);
        assert!(parse_daemon("programSelection: { allowUserChoise: true }").is_err());
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
    fn daemon_reconcile_interval_is_unset_by_default_and_parses_humantime() {
        let default =
            super::parse_local_daemon("programs: {}").expect("valid daemon configuration");
        assert!(default.daemon.is_none());

        let configured = super::parse_local_daemon("daemon:\n  reconcileInterval: 90s\n")
            .expect("valid daemon configuration");
        assert_eq!(
            configured.daemon.unwrap().reconcile_interval,
            Some(std::time::Duration::from_secs(90))
        );
    }

    #[test]
    fn daemon_reconcile_interval_rejects_zero() {
        let error = super::parse_local_daemon("daemon:\n  reconcileInterval: 0s\n")
            .expect_err("a zero reconcile interval is not schedulable");
        assert!(
            error.to_string().contains("daemon.reconcileInterval"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn daemon_reconcile_interval_rejects_more_than_thirty_days() {
        assert!(super::parse_local_daemon("daemon:\n  reconcileInterval: 30days\n").is_ok());
        let error = super::parse_local_daemon("daemon:\n  reconcileInterval: 31days\n")
            .expect_err("an interval beyond 30 days is refused");
        assert!(
            error.to_string().contains("daemon.reconcileInterval"),
            "{error}"
        );
    }

    #[test]
    fn daemon_reconcile_interval_is_rejected_in_pushed_configuration() {
        let error = parse_daemon("daemon:\n  reconcileInterval: 5m\nprograms: {}")
            .expect_err("daemon.* is local-only, like every other startup setting");
        assert!(
            error.to_string().contains("only allowed in the local"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn daemon_usage_listens_on_loopback_without_a_controller() {
        let config = super::parse_local_daemon(
            "daemon:\n  usage:\n    listen: 127.0.0.1:4319\n    tokenFile: /etc/agentdesktop/usage-token\n",
        )
        .expect("valid daemon configuration");
        let usage = config.daemon.unwrap().usage.unwrap();
        assert_eq!(usage.listen, "127.0.0.1:4319".parse().unwrap());
        assert_eq!(
            usage.token_file,
            std::path::Path::new("/etc/agentdesktop/usage-token")
        );

        for (yaml, reason) in [
            (
                "daemon:\n  usage:\n    listen: 0.0.0.0:4319\n    tokenFile: usage-token\n",
                "daemon.usage.listen must be a loopback address",
            ),
            (
                "controller:\n  address: https://controller.example.com\ndaemon:\n  usage:\n    listen: 127.0.0.1:4319\n    tokenFile: usage-token\n",
                "daemon.usage is for a daemon without a controller",
            ),
        ] {
            let error = super::parse_local_daemon(yaml).expect_err(reason);
            assert!(error.to_string().contains(reason), "{error}");
        }
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
    fn controller_usage_token_file_is_relative_to_the_configuration() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "agentdesktop-usage-config-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("create configuration directory");
        let path = directory.join("controller.yaml");
        std::fs::write(
            &path,
            r#"
tls: /etc/agentdesktop/tls
oidc:
  issuer: https://idp.example.com
  clientId: agentdesktop
usage:
  tokenFile: secrets/usage-token
"#,
        )
        .expect("write configuration");

        let controller = load_controller(&path).expect("load controller configuration");
        assert_eq!(
            controller.usage.expect("usage configuration").token_file,
            directory.join("secrets/usage-token")
        );
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
        let standalone =
            super::parse_local_daemon(include_str!("../../../examples/standalone/config.yaml"))
                .expect("standalone daemon configuration example");
        assert!(
            standalone.daemon.and_then(|daemon| daemon.usage).is_some(),
            "the standalone example receives usage from its gateway"
        );
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
    fn oidc_device_authorization_is_opt_in() {
        let daemon = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: https://login.example.com
    clientId: agentdesktop
    deviceAuthorization: true
"#,
        )
        .expect("valid OIDC device authorization configuration");
        let Some(LlmGatewayAuthentication::Oidc {
            device_authorization,
            ..
        }) = daemon
            .llm_gateway
            .and_then(|gateway| gateway.authentication)
        else {
            panic!("expected OIDC authentication");
        };
        assert!(device_authorization);

        let subscription = r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: https://login.example.com
    clientId: agentdesktop
    deviceAuthorization: true
programs:
  claudeCode:
    auth: subscription
"#;
        assert!(
            parse_daemon(subscription)
                .unwrap_err()
                .to_string()
                .contains("deviceAuthorization")
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

    #[test]
    fn pi_parses_gateway_catalog_settings() {
        let daemon = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  pi:
    model: claude-sonnet-4-5
    api: anthropic-messages
    models:
      claude-sonnet-4-5:
        name: Claude Sonnet
"#,
        )
        .expect("valid Pi configuration");
        let pi = daemon.programs.pi.expect("Pi program");
        assert!(pi.use_llm_gateway);
        assert_eq!(pi.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(pi.api.as_deref(), Some("anthropic-messages"));
        assert!(pi.models.contains_key("claude-sonnet-4-5"));
    }

    #[test]
    fn pi_requires_a_model_when_using_the_gateway() {
        let error = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  pi: {}
"#,
        )
        .expect_err("Pi without a model should fail");

        assert!(
            error
                .to_string()
                .contains("Pi requires model when llmGateway is configured")
        );
    }

    #[test]
    fn pi_requires_a_declared_gateway_model_when_models_are_listed() {
        let error = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  pi:
    model: missing
    models:
      available: {}
"#,
        )
        .expect_err("undeclared Pi model should fail");

        assert!(
            error
                .to_string()
                .contains("Pi model missing is not declared in models")
        );
    }

    #[test]
    fn pi_rejects_empty_model_ids() {
        let error = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  pi:
    model: claude-sonnet-4-5
    models:
      claude-sonnet-4-5: {}
      "": {}
"#,
        )
        .expect_err("empty Pi model id should fail");

        assert!(error.to_string().contains("Pi model IDs cannot be empty"));
    }

    #[test]
    fn accepts_mixed_user_and_system_programs() {
        parse_daemon(
            "programs:\n  pi: {}\n  claudeDesktop: {}\n  grok: {}\n  copilot: {}\n  vscode: {}",
        )
        .expect("each daemon manages the programs supported in its mode");
    }

    fn containment(executor: &str) -> super::ContainmentStartupConfig {
        super::ContainmentStartupConfig {
            executor: executor.into(),
            egress: super::ContainmentEgress::default(),
            engine: super::ContainmentEngine::default(),
            egress_upstream: None,
            git: None,
        }
    }

    const CONTAINMENT_EXECUTOR_ERROR: &str =
        "daemon.containment.executor must be an absolute path to wxc-exec.exe";

    #[test]
    fn containment_executor_on_windows_must_be_an_absolute_path_to_an_exe() {
        for executor in [
            r"wxc-exec.exe",
            r"mxc\wxc-exec.exe",
            r"\mxc\wxc-exec.exe",
            r"C:wxc-exec.exe",
        ] {
            assert_eq!(
                super::validate_containment_startup(&containment(executor), true),
                Err(CONTAINMENT_EXECUTOR_ERROR.to_owned()),
                "{executor} is not absolute"
            );
        }
    }

    #[test]
    fn containment_executor_on_windows_accepts_drive_and_unc_paths_and_any_exe_case() {
        for executor in [
            r"C:\mxc\x64\wxc-exec.exe",
            r"C:\mxc\x64\WXC-EXEC.EXE",
            r"C:\mxc\x64\wxc-exec.Exe",
            r"\\server\share\mxc\wxc-exec.exe",
        ] {
            assert_eq!(
                super::validate_containment_startup(&containment(executor), true),
                Ok(()),
                "{executor}"
            );
        }
    }

    #[test]
    fn containment_executor_on_windows_rejects_a_cmd_file() {
        assert_eq!(
            super::validate_containment_startup(&containment(r"C:\mxc\wxc-exec.cmd"), true),
            Err(CONTAINMENT_EXECUTOR_ERROR.to_owned())
        );
        assert_eq!(
            super::validate_containment_startup(&containment(r"C:\mxc\wxc-exec"), true),
            Err(CONTAINMENT_EXECUTOR_ERROR.to_owned())
        );
    }

    #[test]
    fn containment_executor_is_not_checked_off_windows() {
        for executor in ["wxc-exec.exe", "relative/wxc-exec.cmd", "/opt/mxc/wxc-exec"] {
            assert_eq!(
                super::validate_containment_startup(&containment(executor), false),
                Ok(()),
                "{executor}"
            );
        }
    }

    #[test]
    fn daemon_containment_is_a_startup_setting_with_an_executor_path() {
        let config = super::parse_local_daemon(
            "daemon:\n  user: true\n  containment:\n    executor: 'C:\\mxc\\wxc-exec.exe'\n",
        )
        .expect("a containment block parses off Windows");
        let containment = config
            .daemon
            .unwrap()
            .containment
            .expect("containment block");
        assert_eq!(
            containment.executor,
            std::path::Path::new(r"C:\mxc\wxc-exec.exe")
        );
    }

    #[test]
    fn daemon_containment_rejects_unknown_fields_and_a_missing_executor() {
        assert!(
            super::parse_local_daemon(
                "daemon:\n  containment:\n    executor: /x/wxc-exec\n    timeout: 5s\n"
            )
            .is_err()
        );
        assert!(super::parse_local_daemon("daemon:\n  containment: {}\n").is_err());
    }

    #[test]
    fn daemon_containment_is_local_only_and_absent_by_default() {
        let yaml = "daemon:\n  containment:\n    executor: 'C:\\mxc\\wxc-exec.exe'\n";
        assert!(
            parse_daemon(yaml)
                .unwrap_err()
                .to_string()
                .contains("only allowed in the local")
        );
        let startup = super::DaemonStartupConfig::default();
        assert!(startup.containment.is_none());
        assert!(
            serde_json::to_value(&startup)
                .unwrap()
                .get("containment")
                .is_none(),
            "an unset containment block is not serialized"
        );
    }

    #[cfg(windows)]
    #[test]
    fn parse_rejects_a_relative_containment_executor_on_windows() {
        let error =
            super::parse_local_daemon("daemon:\n  containment:\n    executor: wxc-exec.exe\n")
                .expect_err("relative executor");
        assert!(
            error.to_string().contains(CONTAINMENT_EXECUTOR_ERROR),
            "{error:#}"
        );
        super::parse_local_daemon(
            "daemon:\n  containment:\n    executor: 'C:\\mxc\\wxc-exec.exe'\n",
        )
        .expect("absolute executor");
    }

    // Managed-settings lock validation tests

    #[test]
    fn claudecode_config_accepts_both_boolean_locks() {
        let yaml = r#"
programs:
  claudeCode:
    allowManagedHooksOnly: true
    allowManagedMcpServersOnly: false
"#;
        let config = parse_daemon(yaml).expect("valid config");
        let claude = config.programs.claude_code.as_ref().unwrap();
        assert_eq!(claude.allow_managed_hooks_only, Some(true));
        assert_eq!(claude.allow_managed_mcp_servers_only, Some(false));
    }

    #[test]
    fn claudecode_config_rejects_string_for_boolean_keys() {
        let yaml_hooks = r#"
programs:
  claudeCode:
    allowManagedHooksOnly: "yes"
"#;
        let error = parse_daemon(yaml_hooks).expect_err("string for boolean");
        let msg = format!("{error:#}");
        assert!(
            msg.contains("programs.claudeCode.allowManagedHooksOnly"),
            "error should name the key path: {msg}"
        );

        let yaml_mcp = r#"
programs:
  claudeCode:
    allowManagedMcpServersOnly: "true"
"#;
        let error = parse_daemon(yaml_mcp).expect_err("string for boolean");
        let msg = format!("{error:#}");
        assert!(
            msg.contains("programs.claudeCode.allowManagedMcpServersOnly"),
            "error should name the key path: {msg}"
        );
    }

    #[test]
    fn claudecode_config_accepts_empty_allowed_mcp_servers() {
        let yaml = r#"
programs:
  claudeCode:
    allowedMcpServers: []
"#;
        let config = parse_daemon(yaml).expect("valid config");
        let claude = config.programs.claude_code.as_ref().unwrap();
        assert_eq!(
            claude.allowed_mcp_servers.as_ref().map(|v| v.len()),
            Some(0)
        );
    }

    #[test]
    fn claudecode_config_accepts_one_entry_of_each_kind() {
        let yaml = r#"
programs:
  claudeCode:
    allowedMcpServers:
      - serverName: "my-server"
      - serverCommand: ["npx", "-y", "server"]
      - serverUrl: "https://mcp.example.com/*"
"#;
        let config = parse_daemon(yaml).expect("valid config");
        let claude = config.programs.claude_code.as_ref().unwrap();
        let servers = claude.allowed_mcp_servers.as_ref().unwrap();
        assert_eq!(servers.len(), 3);
    }

    #[test]
    fn claudecode_config_rejects_invalid_allowed_mcp_servers() {
        // serverCommand as string not array
        let yaml_cmd_string = r#"
programs:
  claudeCode:
    allowedMcpServers:
      - serverCommand: "npx"
"#;
        let error = parse_daemon(yaml_cmd_string).expect_err("string command");
        let msg = format!("{error:#}");
        assert!(
            msg.contains("programs.claudeCode.allowedMcpServers[0]"),
            "error should name the path: {msg}"
        );

        // Two keys in one entry
        let yaml_two_keys = r#"
programs:
  claudeCode:
    allowedMcpServers:
      - serverName: "test"
        serverUrl: "https://example.com"
"#;
        let error = parse_daemon(yaml_two_keys).expect_err("two keys");
        let msg = format!("{error:#}");
        assert!(
            msg.contains("programs.claudeCode.allowedMcpServers"),
            "error should name the path: {msg}"
        );

        // serverName with invalid characters
        let yaml_invalid_name = r#"
programs:
  claudeCode:
    allowedMcpServers:
      - serverName: "a b"
"#;
        let error = parse_daemon(yaml_invalid_name).expect_err("invalid server name");
        let msg = format!("{error:#}");
        assert!(
            msg.contains("programs.claudeCode.allowedMcpServers[0]"),
            "error should name the path: {msg}"
        );

        // serverName empty
        let yaml_empty_name = r#"
programs:
  claudeCode:
    allowedMcpServers:
      - serverName: ""
"#;
        let error = parse_daemon(yaml_empty_name).expect_err("empty server name");
        let msg = format!("{error:#}");
        assert!(
            msg.contains("programs.claudeCode.allowedMcpServers[0]"),
            "error should name the path: {msg}"
        );
        assert!(
            msg.contains("serverName must match"),
            "error should explain the rule: {msg}"
        );

        // serverUrl empty
        let yaml_empty_url = r#"
programs:
  claudeCode:
    allowedMcpServers:
      - serverUrl: ""
"#;
        let error = parse_daemon(yaml_empty_url).expect_err("empty url");
        let msg = format!("{error:#}");
        assert!(
            msg.contains("programs.claudeCode.allowedMcpServers[0]"),
            "error should name the path: {msg}"
        );
        assert!(
            msg.contains("serverUrl must be non-empty"),
            "error should explain the rule: {msg}"
        );

        // serverCommand empty array
        let yaml_empty_cmd = r#"
programs:
  claudeCode:
    allowedMcpServers:
      - serverCommand: []
"#;
        let error = parse_daemon(yaml_empty_cmd).expect_err("empty command");
        let msg = format!("{error:#}");
        assert!(
            msg.contains("programs.claudeCode.allowedMcpServers[0]"),
            "error should name the path: {msg}"
        );
        assert!(
            msg.contains("serverCommand must be a non-empty array"),
            "error should explain the rule: {msg}"
        );

        // Non-object entry
        let yaml_non_object = r#"
programs:
  claudeCode:
    allowedMcpServers:
      - "just a string"
"#;
        let error = parse_daemon(yaml_non_object).expect_err("non-object entry");
        let msg = format!("{error:#}");
        assert!(
            msg.contains("programs.claudeCode.allowedMcpServers"),
            "error should name the path: {msg}"
        );

        // allowedMcpServers as object not array
        let yaml_not_array = r#"
programs:
  claudeCode:
    allowedMcpServers: {}
"#;
        let error = parse_daemon(yaml_not_array).expect_err("object not array");
        let msg = format!("{error:#}");
        assert!(
            msg.contains("programs.claudeCode.allowedMcpServers"),
            "error should name the path: {msg}"
        );
    }

    #[test]
    fn claudecode_config_passes_through_other_keys() {
        let yaml = r#"
programs:
  claudeCode:
    companyAnnouncements: "hello"
    allowManagedHooksOnly: true
"#;
        let config = parse_daemon(yaml).expect("valid config");
        let claude = config.programs.claude_code.as_ref().unwrap();
        assert_eq!(claude.allow_managed_hooks_only, Some(true));
        assert!(claude.settings.contains_key("companyAnnouncements"));
    }

    // AC 1: ContainmentEgress tests

    #[test]
    fn containment_egress_allow_parses() {
        let config = super::parse_local_daemon(
            "daemon:\n  containment:\n    executor: 'C:\\mxc\\wxc-exec.exe'\n    egress: allow\n",
        )
        .expect("allow parses");
        let egress = config.daemon.unwrap().containment.unwrap().egress;
        assert_eq!(egress, super::ContainmentEgress::Allow);
    }

    #[test]
    fn containment_egress_deny_parses() {
        let config = super::parse_local_daemon(
            "daemon:\n  containment:\n    executor: 'C:\\mxc\\wxc-exec.exe'\n    egress: deny\n",
        )
        .expect("deny parses");
        let egress = config.daemon.unwrap().containment.unwrap().egress;
        assert_eq!(egress, super::ContainmentEgress::Deny);
    }

    #[test]
    fn containment_egress_absent_defaults_to_deny() {
        // PR 4 (AC 4): the default flipped from allow to deny.
        let config = super::parse_local_daemon(
            "daemon:\n  containment:\n    executor: 'C:\\mxc\\wxc-exec.exe'\n",
        )
        .expect("absent defaults");
        let egress = config.daemon.unwrap().containment.unwrap().egress;
        assert_eq!(egress, super::ContainmentEgress::Deny);
    }

    #[test]
    fn containment_egress_rejects_deny_capitalized() {
        assert!(
            super::parse_local_daemon(
                "daemon:\n  containment:\n    executor: /x/wxc-exec\n    egress: Deny\n"
            )
            .is_err(),
            "Deny capitalized is rejected"
        );
    }

    #[test]
    fn containment_egress_rejects_block() {
        assert!(
            super::parse_local_daemon(
                "daemon:\n  containment:\n    executor: /x/wxc-exec\n    egress: block\n"
            )
            .is_err(),
            "block is rejected"
        );
    }

    #[test]
    fn containment_egress_unset_not_serialized() {
        // PR 4: default flipped to Deny, so Deny is not serialized and Allow is
        let cfg = super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("/x/wxc-exec"),
            egress: super::ContainmentEgress::Deny,
            engine: super::ContainmentEngine::default(),
            egress_upstream: None,
            git: None,
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert!(
            json.get("egress").is_none(),
            "default egress (Deny) is not serialized"
        );

        let cfg_allow = super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("/x/wxc-exec"),
            egress: super::ContainmentEgress::Allow,
            engine: super::ContainmentEngine::default(),
            egress_upstream: None,
            git: None,
        };
        let json_allow = serde_json::to_value(&cfg_allow).unwrap();
        assert_eq!(
            json_allow.get("egress").and_then(|v| v.as_str()),
            Some("allow"),
            "deny egress is serialized"
        );
    }

    #[test]
    fn containment_engine_explicit_mxc_parses() {
        let config = super::parse_local_daemon(
            "daemon:\n  containment:\n    executor: 'C:\\mxc\\wxc-exec.exe'\n    engine: mxc\n",
        )
        .expect("explicit mxc engine parses");
        let engine = config.daemon.unwrap().containment.unwrap().engine;
        assert_eq!(engine, super::ContainmentEngine::Mxc);
    }

    #[test]
    fn containment_engine_rejects_mxc_capitalized() {
        assert!(
            super::parse_local_daemon(
                "daemon:\n  containment:\n    executor: 'C:\\mxc\\wxc-exec.exe'\n    engine: MXC\n"
            )
            .is_err(),
            "MXC capitalized is rejected"
        );
    }

    #[test]
    fn containment_engine_unset_not_serialized() {
        let cfg = super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("/x/wxc-exec"),
            egress: super::ContainmentEgress::Allow,
            engine: super::ContainmentEngine::default(),
            egress_upstream: None,
            git: None,
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert!(
            json.get("engine").is_none(),
            "default engine is not serialized"
        );
    }

    // PR 6 (AC-1): the top-level `containment` block (D1), accepted in both
    // the local file and controller-delivered policy, unlike
    // `daemon.containment`.

    const WRAP_CLAUDE_YAML: &str = "containment:\n  programs:\n    claudeCode:\n      executable: 'C:\\Program Files\\Anthropic\\ClaudeCode\\claude.exe'\n";

    #[test]
    fn containment_program_without_mode_defaults_to_wrap() {
        let config = super::parse_local_daemon(WRAP_CLAUDE_YAML).expect("wrap by default parses");
        let claude = config
            .containment
            .expect("block present")
            .programs
            .claude_code
            .expect("claudeCode present");
        assert_eq!(claude.mode, super::ContainmentMode::Wrap);
    }

    #[test]
    fn containment_mode_off_is_refused_naming_the_field() {
        let yaml = "containment:\n  programs:\n    claudeCode:\n      mode: off\n      executable: 'C:\\Program Files\\Anthropic\\ClaudeCode\\claude.exe'\n";
        let error = super::parse_local_daemon(yaml).unwrap_err();
        let chain = format!("{error:#}");
        assert!(
            chain.contains("containment.programs.claudeCode.mode"),
            "{chain}"
        );
    }

    #[test]
    fn containment_mode_third_value_is_refused() {
        let yaml = "containment:\n  programs:\n    claudeCode:\n      mode: bogus\n      executable: 'C:\\Program Files\\Anthropic\\ClaudeCode\\claude.exe'\n";
        assert!(super::parse_local_daemon(yaml).is_err());
    }

    #[test]
    fn containment_native_mode_does_not_require_executable() {
        let config = super::parse_local_daemon(
            "containment:\n  programs:\n    claudeCode:\n      mode: native\n",
        )
        .expect("native mode needs no executable");
        assert_eq!(
            config
                .containment
                .unwrap()
                .programs
                .claude_code
                .unwrap()
                .mode,
            super::ContainmentMode::Native
        );
    }

    #[test]
    fn containment_wrap_without_executable_is_refused_naming_the_field() {
        let error = super::parse_local_daemon("containment:\n  programs:\n    claudeCode: {}\n")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("containment.programs.claudeCode.executable"),
            "{error}"
        );
    }

    #[test]
    fn containment_wrap_relative_path_is_refused() {
        let yaml = "containment:\n  programs:\n    claudeCode:\n      executable: claude.exe\n";
        let error = super::parse_local_daemon(yaml).unwrap_err().to_string();
        assert!(
            error.contains("containment.programs.claudeCode.executable"),
            "{error}"
        );
    }

    #[test]
    fn containment_wrap_forbidden_character_is_refused() {
        for executable in [
            r#"C:\Program Files\Anthropic\ClaudeCode\"claude.exe"#,
            r"C:\Program Files\Anthropic\ClaudeCode\%claude.exe",
            r"C:\Program Files\Anthropic\ClaudeCode\!claude.exe",
        ] {
            let yaml = format!(
                "containment:\n  programs:\n    openCode:\n      executable: '{}'\n",
                executable.replace('\'', "''")
            );
            let error = super::parse_local_daemon(&yaml).unwrap_err().to_string();
            assert!(
                error.contains("containment.programs.openCode.executable"),
                "{executable}: {error}"
            );
        }
    }

    #[test]
    fn containment_wrap_users_path_is_refused_case_insensitively() {
        for path in [
            r"C:\Users\alice\.local\bin\claude.exe",
            r"C:\USERS\alice\.local\bin\claude.exe",
        ] {
            let yaml =
                format!("containment:\n  programs:\n    claudeCode:\n      executable: '{path}'\n");
            let error = super::parse_local_daemon(&yaml).unwrap_err().to_string();
            assert!(
                error.contains("containment.programs.claudeCode.executable"),
                "{path}: {error}"
            );
        }
    }

    #[test]
    fn containment_wrap_unc_path_ending_in_exe_case_insensitively_is_accepted() {
        let yaml = r"containment:
  programs:
    copilot:
      executable: '\\fileserver\share\Copilot.EXE'
";
        super::parse_local_daemon(yaml).expect("UNC path ending in .EXE is accepted");
    }

    #[test]
    fn containment_block_runs_the_same_validation_through_parse_daemon() {
        // N3: the controller validates on Linux; the string rules are not
        // windows()-gated, unlike daemon.containment's path syntax.
        let error = super::parse_daemon("containment:\n  programs:\n    claudeCode: {}\n")
            .unwrap_err()
            .to_string();
        assert!(error.contains("containment.programs.claudeCode.executable"));
    }

    #[test]
    fn containment_round_trips_through_yaml() {
        let config = super::parse_local_daemon(WRAP_CLAUDE_YAML).unwrap();
        let rendered = crate::serdes::yamlviajson::to_string(&config).unwrap();
        let reparsed = super::parse_local_daemon(&rendered).unwrap();
        assert_eq!(
            reparsed.containment.unwrap().programs.claude_code,
            config.containment.unwrap().programs.claude_code
        );
    }

    #[test]
    fn daemon_config_without_containment_round_trips_unchanged() {
        let yaml = "inventoryInterval: 15m\n";
        let config = super::parse_local_daemon(yaml).unwrap();
        assert!(config.containment.is_none());
        let rendered = crate::serdes::yamlviajson::to_string(&config).unwrap();
        assert!(
            !rendered.contains("containment"),
            "unset containment is not serialized: {rendered}"
        );
    }

    #[test]
    fn containment_counts_toward_daemon_config_is_empty() {
        let config = super::parse_local_daemon(WRAP_CLAUDE_YAML).unwrap();
        assert!(
            !config.is_empty(),
            "a config with only a containment block is not empty"
        );
    }

    #[test]
    fn containment_deny_unknown_fields_on_every_new_struct() {
        for yaml in [
            "containment:\n  bogus: true\n",
            "containment:\n  programs:\n    bogus: true\n",
            "containment:\n  programs:\n    claudeCode:\n      bogus: true\n",
        ] {
            assert!(
                super::parse_local_daemon(yaml).is_err(),
                "{yaml} is rejected"
            );
        }
    }

    /// DV-8: with egressUpstream set, git.allow + git.pushAllow must name a
    /// single (host, port) — the egress endpoint routes by path only.
    #[test]
    fn containment_git_single_host_with_egress_upstream() {
        let upstream = || {
            Some(super::EgressUpstreamConfig {
                url: url::Url::parse("https://egress.example.com").unwrap(),
                authentication: None,
            })
        };
        let cfg = |upstream, allow: &[&str], push_allow: &[&str]| super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("C:\\mxc\\wxc-exec.exe"),
            egress: super::ContainmentEgress::default(),
            engine: super::ContainmentEngine::default(),
            egress_upstream: upstream,
            git: Some(super::GitConfig {
                allow: allow.iter().map(|s| (*s).to_owned()).collect(),
                push_allow: push_allow.iter().map(|s| (*s).to_owned()).collect(),
                credentials: vec![],
                ssh: None,
            }),
        };
        // Two hosts in allow: refused.
        let result = super::validate_containment_startup(
            &cfg(
                upstream(),
                &["https://github.com/org/", "https://gitlab.com/org/"],
                &[],
            ),
            true,
        );
        assert!(
            result
                .as_ref()
                .is_err_and(|error| error.contains("single host")),
            "{result:?}"
        );
        // A second host via pushAllow: refused too.
        assert!(
            super::validate_containment_startup(
                &cfg(
                    upstream(),
                    &["https://github.com/org/"],
                    &["https://gitlab.com/x/"]
                ),
                true,
            )
            .is_err()
        );
        // Case-insensitive host, default port: one key.
        assert_eq!(
            super::validate_containment_startup(
                &cfg(
                    upstream(),
                    &["https://github.com/a/", "https://GitHub.com:443/b/"],
                    &[]
                ),
                true,
            ),
            Ok(())
        );
        // Same host, different ports: two keys, refused.
        assert!(
            super::validate_containment_startup(
                &cfg(
                    upstream(),
                    &["https://git.corp/", "https://git.corp:8443/"],
                    &[]
                ),
                true,
            )
            .is_err()
        );
        // Without egressUpstream, two hosts stay allowed.
        assert_eq!(
            super::validate_containment_startup(
                &cfg(
                    None,
                    &["https://github.com/org/", "https://gitlab.com/org/"],
                    &[]
                ),
                true,
            ),
            Ok(())
        );
        // An entry without a host never passes (with or without upstream).
        assert!(
            super::validate_containment_startup(&cfg(upstream(), &["https://"], &[]), true)
                .is_err()
        );
    }

    /// The credential secret never reaches a `{:?}` rendering (manual Debug).
    #[test]
    fn git_credential_entry_debug_redacts_secret() {
        let entry = super::GitCredentialEntry {
            url_prefix: "https://github.com/org/".to_owned(),
            username: "x-access-token".to_owned(),
            secret: "sup3rs3cret-PAT".to_owned(),
        };
        let rendered = format!("{entry:?}");
        assert!(!rendered.contains("sup3rs3cret-PAT"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
        let containment = super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("C:\\mxc\\wxc-exec.exe"),
            egress: super::ContainmentEgress::default(),
            engine: super::ContainmentEngine::default(),
            egress_upstream: None,
            git: Some(super::GitConfig {
                allow: vec!["https://github.com/org/".to_owned()],
                push_allow: vec![],
                credentials: vec![entry],
                ssh: None,
            }),
        };
        let rendered = format!("{containment:?}");
        assert!(!rendered.contains("sup3rs3cret-PAT"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    // PR 5 tests: ssh agent proxy and git over ssh

    #[test]
    fn git_ssh_config_parses_hosts_and_keys() {
        let config = super::parse_local_daemon(
            "daemon:\n  containment:\n    executor: 'C:\\mxc\\wxc-exec.exe'\n    git:\n      ssh:\n        hosts: ['github.com', 'gitlab.com']\n        keys: ['SHA256:abcd1234', '*']\n",
        )
        .expect("ssh config parses");
        let ssh = config
            .daemon
            .unwrap()
            .containment
            .unwrap()
            .git
            .unwrap()
            .ssh
            .unwrap();
        assert_eq!(ssh.hosts, vec!["github.com", "gitlab.com"]);
        assert_eq!(ssh.keys, vec!["SHA256:abcd1234", "*"]);
    }

    #[test]
    fn git_ssh_config_hosts_normalized_to_lowercase() {
        let config = super::parse_local_daemon(
            "daemon:\n  containment:\n    executor: 'C:\\mxc\\wxc-exec.exe'\n    git:\n      ssh:\n        hosts: ['GitHub.COM']\n",
        )
        .expect("ssh config parses");
        let ssh = config
            .daemon
            .unwrap()
            .containment
            .unwrap()
            .git
            .unwrap()
            .ssh
            .unwrap();
        assert_eq!(ssh.hosts, vec!["github.com"]);
    }

    #[test]
    fn git_ssh_config_absent_round_trips_absent() {
        let cfg = super::GitConfig {
            allow: vec![],
            push_allow: vec![],
            credentials: vec![],
            ssh: None,
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert!(json.get("ssh").is_none(), "absent ssh is not serialized");
    }

    #[test]
    fn git_ssh_config_empty_hosts_and_keys_not_serialized() {
        let cfg = super::GitSshConfig {
            hosts: vec![],
            keys: vec![],
        };
        let json = serde_json::to_value(&cfg).unwrap();
        assert!(json.get("hosts").is_none(), "empty hosts not serialized");
        assert!(json.get("keys").is_none(), "empty keys not serialized");
    }

    #[test]
    fn git_ssh_config_deny_unknown_fields() {
        assert!(
            super::parse_local_daemon(
                "daemon:\n  containment:\n    executor: 'C:\\mxc\\wxc-exec.exe'\n    git:\n      ssh:\n        host: ['github.com']\n",
            )
            .is_err(),
            "unknown field 'host' should be rejected"
        );
    }

    #[test]
    fn git_ssh_host_validation_rejects_scheme() {
        let cfg = super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("C:\\mxc\\wxc-exec.exe"),
            egress: super::ContainmentEgress::default(),
            engine: super::ContainmentEngine::default(),
            egress_upstream: None,
            git: Some(super::GitConfig {
                allow: vec![],
                push_allow: vec![],
                credentials: vec![],
                ssh: Some(super::GitSshConfig {
                    hosts: vec!["https://github.com".to_owned()],
                    keys: vec![],
                }),
            }),
        };
        let error = super::validate_containment_startup(&cfg, true).expect_err("scheme rejected");
        assert!(
            error.contains("daemon.containment.git.ssh.hosts[0]"),
            "error: {error}"
        );
        assert!(
            error.contains("bare hostname (no scheme, path, or port)"),
            "error: {error}"
        );
    }

    #[test]
    fn git_ssh_host_validation_charset() {
        fn validate_host(host: &str) -> Result<(), String> {
            let cfg = super::ContainmentStartupConfig {
                executor: std::path::PathBuf::from("C:\\mxc\\wxc-exec.exe"),
                egress: super::ContainmentEgress::default(),
                engine: super::ContainmentEngine::default(),
                egress_upstream: None,
                git: Some(super::GitConfig {
                    allow: vec![],
                    push_allow: vec![],
                    credentials: vec![],
                    ssh: Some(super::GitSshConfig {
                        hosts: vec![host.to_owned()],
                        keys: vec![],
                    }),
                }),
            };
            super::validate_containment_startup(&cfg, true)
        }
        for host in [
            "github.com",
            "httpbin.org",
            "httpd.corp.example",
            "git-01.corp.example",
            "github-demo",
            "10.0.0.5",
        ] {
            assert_eq!(validate_host(host), Ok(()), "accepted: {host}");
        }
        for host in [
            "a.com\n\tProxyCommand x",
            "a.com ProxyCommand",
            " github.com",
            "a\tb",
            "*",
            "*.corp.example",
            "git?.corp",
            "%h",
            "-oProxyCommand=x",
            "a..com",
            "github.com.",
            ".github.com",
        ] {
            assert_eq!(
                validate_host(host),
                Err(
                    "daemon.containment.git.ssh.hosts[0] must be a bare hostname (no scheme, path, or port)"
                        .to_owned()
                ),
                "refused: {host}"
            );
        }
    }

    #[test]
    fn git_ssh_host_validation_rejects_path() {
        let cfg = super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("C:\\mxc\\wxc-exec.exe"),
            egress: super::ContainmentEgress::default(),
            engine: super::ContainmentEngine::default(),
            egress_upstream: None,
            git: Some(super::GitConfig {
                allow: vec![],
                push_allow: vec![],
                credentials: vec![],
                ssh: Some(super::GitSshConfig {
                    hosts: vec!["github.com/org/repo".to_owned()],
                    keys: vec![],
                }),
            }),
        };
        let error = super::validate_containment_startup(&cfg, true).expect_err("path rejected");
        assert!(
            error.contains("daemon.containment.git.ssh.hosts[0]"),
            "error: {error}"
        );
        assert!(
            error.contains("bare hostname (no scheme, path, or port)"),
            "error: {error}"
        );
    }

    #[test]
    fn git_ssh_host_validation_rejects_port() {
        let cfg = super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("C:\\mxc\\wxc-exec.exe"),
            egress: super::ContainmentEgress::default(),
            engine: super::ContainmentEngine::default(),
            egress_upstream: None,
            git: Some(super::GitConfig {
                allow: vec![],
                push_allow: vec![],
                credentials: vec![],
                ssh: Some(super::GitSshConfig {
                    hosts: vec!["github.com:2222".to_owned()],
                    keys: vec![],
                }),
            }),
        };
        let error = super::validate_containment_startup(&cfg, true).expect_err("port rejected");
        assert!(
            error.contains("daemon.containment.git.ssh.hosts[0]"),
            "error: {error}"
        );
        assert!(
            error.contains("bare hostname (no scheme, path, or port)"),
            "error: {error}"
        );
    }

    #[test]
    fn git_ssh_host_validation_rejects_empty() {
        let cfg = super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("C:\\mxc\\wxc-exec.exe"),
            egress: super::ContainmentEgress::default(),
            engine: super::ContainmentEngine::default(),
            egress_upstream: None,
            git: Some(super::GitConfig {
                allow: vec![],
                push_allow: vec![],
                credentials: vec![],
                ssh: Some(super::GitSshConfig {
                    hosts: vec!["".to_owned()],
                    keys: vec![],
                }),
            }),
        };
        let error = super::validate_containment_startup(&cfg, true).expect_err("empty rejected");
        assert!(
            error.contains("daemon.containment.git.ssh.hosts[0]"),
            "error: {error}"
        );
    }

    #[test]
    fn git_ssh_key_validation_rejects_non_sha256() {
        let cfg = super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("C:\\mxc\\wxc-exec.exe"),
            egress: super::ContainmentEgress::default(),
            engine: super::ContainmentEngine::default(),
            egress_upstream: None,
            git: Some(super::GitConfig {
                allow: vec![],
                push_allow: vec![],
                credentials: vec![],
                ssh: Some(super::GitSshConfig {
                    hosts: vec![],
                    keys: vec!["MD5:1234abcd".to_owned()],
                }),
            }),
        };
        let error = super::validate_containment_startup(&cfg, true)
            .expect_err("non-SHA256 fingerprint rejected");
        assert!(
            error.contains("daemon.containment.git.ssh.keys[0]"),
            "error: {error}"
        );
        assert!(
            error.contains("must be a SHA256:... fingerprint or \"*\""),
            "error: {error}"
        );
    }

    #[test]
    fn git_ssh_key_validation_accepts_sha256() {
        let cfg = super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("C:\\mxc\\wxc-exec.exe"),
            egress: super::ContainmentEgress::default(),
            engine: super::ContainmentEngine::default(),
            egress_upstream: None,
            git: Some(super::GitConfig {
                allow: vec![],
                push_allow: vec![],
                credentials: vec![],
                ssh: Some(super::GitSshConfig {
                    hosts: vec!["github.com".to_owned()],
                    keys: vec!["SHA256:abcd1234".to_owned()],
                }),
            }),
        };
        assert!(super::validate_containment_startup(&cfg, true).is_ok());
    }

    #[test]
    fn git_ssh_key_validation_accepts_wildcard() {
        let cfg = super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("C:\\mxc\\wxc-exec.exe"),
            egress: super::ContainmentEgress::default(),
            engine: super::ContainmentEngine::default(),
            egress_upstream: None,
            git: Some(super::GitConfig {
                allow: vec![],
                push_allow: vec![],
                credentials: vec![],
                ssh: Some(super::GitSshConfig {
                    hosts: vec!["github.com".to_owned()],
                    keys: vec!["*".to_owned()],
                }),
            }),
        };
        assert!(super::validate_containment_startup(&cfg, true).is_ok());
    }

    #[test]
    fn git_ssh_with_egress_upstream_and_hosts_rejected() {
        let cfg = super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("C:\\mxc\\wxc-exec.exe"),
            egress: super::ContainmentEgress::default(),
            engine: super::ContainmentEngine::default(),
            egress_upstream: Some(super::EgressUpstreamConfig {
                url: url::Url::parse("https://gateway.example.com/forward").unwrap(),
                authentication: None,
            }),
            git: Some(super::GitConfig {
                allow: vec![],
                push_allow: vec![],
                credentials: vec![],
                ssh: Some(super::GitSshConfig {
                    hosts: vec!["github.com".to_owned()],
                    keys: vec![],
                }),
            }),
        };
        let error = super::validate_containment_startup(&cfg, true)
            .expect_err("egress_upstream + ssh.hosts rejected");
        assert!(
            error.contains("daemon.containment.git.ssh"),
            "error: {error}"
        );
        assert!(
            error.contains("with egressUpstream set, ssh egress is not available"),
            "error: {error}"
        );
        assert!(error.contains("L7 https only"), "error: {error}");
    }

    #[test]
    fn git_ssh_with_egress_upstream_and_empty_hosts_allowed() {
        let cfg = super::ContainmentStartupConfig {
            executor: std::path::PathBuf::from("C:\\mxc\\wxc-exec.exe"),
            egress: super::ContainmentEgress::default(),
            engine: super::ContainmentEngine::default(),
            egress_upstream: Some(super::EgressUpstreamConfig {
                url: url::Url::parse("https://gateway.example.com/forward").unwrap(),
                authentication: None,
            }),
            git: Some(super::GitConfig {
                allow: vec![],
                push_allow: vec![],
                credentials: vec![],
                ssh: Some(super::GitSshConfig {
                    hosts: vec![],
                    keys: vec![],
                }),
            }),
        };
        assert!(
            super::validate_containment_startup(&cfg, true).is_ok(),
            "egress_upstream + empty ssh.hosts allowed"
        );
    }
}
