use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    time::Duration,
};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Discovery {
    pub agents: Vec<Agent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_runtimes: Vec<ModelRuntime>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelRuntime {
    /// Local runtime that owns the discovered models.
    pub kind: String,
    pub models: Vec<LocalModel>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LocalModel {
    /// Runtime-scoped name used for inference requests.
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub kind: String,
    pub executable: PathBuf,
    pub version: Option<String>,
    /// MCP servers configured for this developer tool.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp_servers: Vec<McpServer>,
    /// Skills visible to this developer tool, represented only by their front matter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<Skill>,
}

/// A secret-free, tool-independent representation of a configured MCP server.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct McpServer {
    /// Name assigned to the server by the developer tool.
    pub name: String,
    /// MCP transport (`stdio`, `http`, or `sse`).
    pub transport: String,
    /// Executable used by a stdio server. Arguments and environment are intentionally omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Endpoint used by an HTTP or SSE server. Headers are intentionally omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Whether the server is enabled in its source configuration.
    pub enabled: bool,
    /// Configuration file from which this server was discovered.
    pub source: PathBuf,
}

/// A discovered agent skill represented by its YAML front matter.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Skill {
    /// Path to the skill's `SKILL.md` file.
    pub path: PathBuf,
    /// Complete YAML front matter converted to JSON-compatible values.
    pub front_matter: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub status: String,
    /// Present only when the daemon has a controller configured. Reflects
    /// whether the daemon's own connection to the controller is currently
    /// live, independent of local process health: a daemon can be fully
    /// healthy locally while its controller stream is down (auth rejection,
    /// network partition, stuck retry loop, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub controller: Option<ControllerConnectionStatus>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ControllerConnectionStatus {
    pub connected: bool,
    /// Last time the daemon observed its controller stream alive: when the
    /// stream opened, a heartbeat was handed to it, or a controller message
    /// arrived. Retained across disconnects so the UI can say how long the
    /// device has been out of contact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_unix_seconds: Option<u64>,
    /// Coarse reason for the most recent failure. Deliberately a closed set:
    /// the full error chain (addresses, paths, upstream messages) stays in the
    /// daemon logs and is never returned to the local API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<ControllerConnectionError>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ControllerConnectionError {
    /// The controller could not be reached or the stream failed; retrying.
    Unreachable,
    /// The controller rejected the device identity; the daemon is re-enrolling.
    IdentityRejected,
    /// The organization session could not be refreshed; the daemon is re-enrolling.
    SessionExpired,
    /// A local error (for example, the identity store) ended the controller
    /// session; the daemon is restarting it.
    LocalError,
    /// A reason reported by a newer daemon that this build does not know.
    #[serde(other)]
    Unknown,
}

/// Read-only startup information reported by the running daemon, not the desktop host.
/// Deliberately excludes tool configuration, credentials, and certificate contents.
/// Paths are lossy UTF-8 display strings, not paths for filesystem operations.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DaemonInfo {
    pub version: String,
    pub scope: DaemonScope,
    pub config_path: String,
    pub state_directory: String,
    #[serde(with = "humantime_serde")]
    pub inventory_interval: Duration,
    pub controller: Option<DaemonControllerInfo>,
    /// Local loopback LLM proxy, when configured. Absent in system mode and when
    /// `daemon.llmProxy.listen` is unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_proxy: Option<LlmProxyInfo>,
    /// Receiver for usage from a gateway on this machine, when
    /// `daemon.usage` is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_receiver: Option<UsageReceiverInfo>,
    /// Sandboxed launch (`agentdesktop run`), when `daemon.containment` is
    /// configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub containment: Option<ContainmentInfo>,
}

/// The admin source Claude Code selects for managed settings.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ManagedSettingsSource {
    Hklm,
    File,
    #[serde(other)]
    Unknown,
}

/// Whether `agentdesktop run` can launch a sandboxed program on this daemon.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ContainmentInfo {
    /// Configured `daemon.containment.executor` (lossy display string).
    pub executor: String,
    /// The sandbox engine name.
    pub engine: String,
    /// Whether launches are possible. `false` carries the reason in `error`.
    pub available: bool,
    /// Why launches are impossible, when `available` is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The admin source Claude Code selects (hklm, file, or null when no admin source delivers a policy).
    #[serde(default)]
    pub managed_settings_source: Option<ManagedSettingsSource>,
    /// Whether `allowManagedHooksOnly` is enforced.
    #[serde(default)]
    pub managed_hooks_locked: bool,
    /// Whether `allowManagedMcpServersOnly` with `allowedMcpServers` is enforced.
    #[serde(default)]
    pub managed_mcp_locked: bool,
    /// Error reading managed settings, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_settings_error: Option<String>,
    /// Posture of each wrap-mode program named by the `containment` policy,
    /// keyed by launch id (`claude-code`, `opencode`, `copilot`). Filled per
    /// request from the device-local violation ledger; empty without a
    /// `containment` policy, or on a daemon that is not a Windows user-mode
    /// daemon.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub programs: BTreeMap<String, ProgramPosture>,
}

/// Posture of one wrap-mode program, serialized camelCase: `mode`,
/// `executable`, `aliasesPresent`, `violations`,
/// `lastViolationUnixSeconds`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProgramPosture {
    pub mode: crate::config::ContainmentMode,
    /// Configured `containment.programs.<p>.executable` (lossy display string).
    pub executable: String,
    /// Every launcher payload file exists and the launcher directory
    /// appears in the machine PATH value (HKLM Session-Manager `Path`). Does
    /// not prove PATH order within a given live session.
    pub aliases_present: bool,
    /// Distinct `(pid, startTime)` native processes of the managed
    /// executable seen since daemon start; resets on daemon restart.
    pub violations: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_violation_unix_seconds: Option<u64>,
}

/// Body of `POST /v1/launch`.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LaunchRequest {
    /// Program ID to launch: `claude-code`, `opencode` or `copilot`.
    pub program: String,
    /// Workspace directory of the launch.
    pub cwd: PathBuf,
    /// Arguments passed to the program.
    pub args: Vec<String>,
    /// Ask the daemon to run the boxed program on this client's console
    /// (same-terminal hosting). Older daemons ignore the field and launch
    /// windowed; the answer's `console` says which mode was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console: Option<ConsoleAttach>,
}

/// The client console a launch may attach to.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConsoleAttach {
    /// The requesting client process. Only a process of the daemon's own user
    /// and session is eligible; any other pid downgrades to `Window`.
    pub client_pid: u32,
}

/// How a launch presents the boxed program's console.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum LaunchConsole {
    /// The boxed program gets its own console window (`start /WAIT`).
    #[default]
    Window,
    /// The boxed program runs on the requesting client's console; its exit
    /// code is the program's own.
    Attached,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum LaunchStatus {
    Running,
    Exited,
}

/// A sandboxed launch started by this daemon.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Launch {
    /// 16 lowercase hexadecimal characters.
    pub launch_id: String,
    pub program: String,
    pub cwd: PathBuf,
    /// `agentdesktop-<launch_id>`.
    pub container_id: String,
    pub status: LaunchStatus,
    pub exit_code: Option<i32>,
    /// Console mode actually used (older daemons answer without the field:
    /// `window`).
    #[serde(default)]
    pub console: LaunchConsole,
    pub started_at_unix_seconds: u64,
    pub ended_at_unix_seconds: Option<u64>,
    /// True once the executable configuration changed while this launch ran
    /// (set by the periodic check or the exit check; never cleared).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub integrity_drift: bool,
    /// Managed-settings source when this launch started.
    #[serde(default)]
    pub managed_settings_source: Option<ManagedSettingsSource>,
    /// Whether `allowManagedHooksOnly` was enforced when this launch started.
    #[serde(default)]
    pub managed_hooks_locked: bool,
    /// Whether `allowManagedMcpServersOnly` was enforced when this launch started.
    #[serde(default)]
    pub managed_mcp_locked: bool,
    /// The relay session this launch runs on; revoked when the box exits.
    /// Empty when an older daemon answers without the field.
    #[serde(default)]
    pub session_id: String,
}

/// State of the daemon's receiver for gateway usage.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct UsageReceiverInfo {
    /// Configured address; the bound address once the listener is up.
    pub listen: String,
    /// Whether the receiver accepts exports. `false` means its listener,
    /// token, or database failed at startup; it stays off until the daemon
    /// restarts.
    pub bound: bool,
    /// Why the receiver is off, when `bound` is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// State of the daemon's loopback LLM proxy listener.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LlmProxyInfo {
    /// Configured address; the bound address once the listener is up.
    pub listen: String,
    /// Whether the listener is bound. `false` means the bind failed at startup;
    /// the daemon keeps running with the proxy off until it is restarted.
    pub bound: bool,
    /// Credential policy client ID the proxy presents to the gateway.
    pub client_id: String,
    /// Why the bind failed, when `bound` is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DaemonScope {
    User,
    System,
}

/// Connection metadata only; URL credentials, query, and fragment are omitted.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DaemonControllerInfo {
    pub address: String,
    /// Lossy UTF-8 display path; never used to load the certificate.
    pub ca_certificate_path: Option<String>,
    #[serde(with = "humantime_serde")]
    pub heartbeat_interval: Duration,
}

/// The configured programs the device user wants Agentdesktop to manage.
/// Honored only while the policy sets `programSelection.allowUserChoice`.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "mode")]
pub enum ProgramChoice {
    /// Every configured program, including programs configured later.
    #[default]
    All,
    /// No program: Agentdesktop removes the settings it wrote for each one.
    None,
    /// Only these programs, by provider ID (for example `claude-code`).
    /// Programs configured later stay unmanaged until chosen.
    Selected { programs: BTreeSet<String> },
}

impl ProgramChoice {
    /// Whether this choice keeps `program` (a provider ID) managed.
    pub fn includes(&self, program: &str) -> bool {
        match self {
            Self::All => true,
            Self::None => false,
            Self::Selected { programs } => programs.contains(program),
        }
    }
}

/// Which configured programs are managed on this device, and whether the
/// device user may change that.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProgramManagement {
    /// Whether the policy lets the device user choose
    /// (`programSelection.allowUserChoice`).
    pub user_choice_allowed: bool,
    /// The device user's stored choice; ignored while `user_choice_allowed`
    /// is `false`.
    pub choice: ProgramChoice,
    /// The programs the policy configures, in reconcile order. Empty before
    /// the device has a configuration.
    pub programs: Vec<ManagedProgram>,
}

/// One configured program and its last reconcile outcome.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ManagedProgram {
    /// Provider ID, for example `claude-code`.
    pub program: String,
    /// Whether Agentdesktop manages this program: `false` when the device
    /// user opted out of it.
    pub managed: bool,
    /// Outcome of the last apply (`applied`, `unchanged`, `opted_out`, …);
    /// absent until the program has been applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentStatus {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_url: Option<String>,
    /// Code the user confirms at `authorization_url` during device
    /// authorization enrollment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmGatewayCredential {
    pub credential: String,
    pub expires_at_unix_seconds: u64,
}

/// LLM gateway sign-in state for OIDC device authorization.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct LlmGatewayLoginStatus {
    /// `signedIn` or `awaitingAuthentication`.
    pub status: String,
    /// Page where the user approves the sign-in from any device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verification_url: Option<String>,
    /// Code the user confirms at `verification_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_code: Option<String>,
}

/// Token and cost totals over a set of LLM gateway requests.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    pub requests: u64,
    pub errors: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// API-billed cost in US dollars at the gateway's catalog prices.
    pub cost_usd: f64,
    /// Subscription-billed requests priced at API rates. A subscription is
    /// not billed per token, so this is never part of `cost_usd`.
    pub subscription_cost_usd: f64,
    pub subscription_requests: u64,
    /// Requests for models the gateway has no price for.
    pub unpriced_requests: u64,
}

/// Usage totals for one value of a breakdown dimension.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageGroup {
    pub key: String,
    pub totals: UsageTotals,
}

/// Usage totals for one time bucket.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct UsageBucket {
    pub start_unix_seconds: u64,
    pub totals: UsageTotals,
}

/// LLM gateway usage recorded for this device by its controller, or by the
/// daemon itself when it has no controller.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DeviceUsage {
    /// False when the controller does not collect usage.
    pub enabled: bool,
    pub from_unix_seconds: u64,
    pub to_unix_seconds: u64,
    pub bucket_seconds: u64,
    pub totals: UsageTotals,
    pub buckets: Vec<UsageBucket>,
    pub by_tool: Vec<UsageGroup>,
    pub by_model: Vec<UsageGroup>,
    /// Configured tools whose traffic does not reach the gateway's usage
    /// records, such as VS Code with `copilotChat: githubModels`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmetered_tools: Vec<String>,
}

/// A timestamped telemetry observation emitted by a managed device.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryEvent {
    /// Time at which the daemon accepted the event, in Unix milliseconds.
    pub timestamp_unix_ms: u64,
    /// Typed telemetry payload.
    #[serde(flatten)]
    pub event: TelemetryEventKind,
}

/// Extensible set of telemetry event payloads.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum TelemetryEventKind {
    /// A new developer-tool session.
    SessionNew {
        /// Developer client that emitted the event.
        client_id: String,
        /// Session identifier supplied by the developer client.
        session_id: String,
    },
    /// A tool invocation observed before execution.
    ToolUse {
        /// Developer client that emitted the event.
        client_id: String,
        /// Tool name supplied by the developer client.
        tool_name: String,
        /// Optional invocation identifier supplied by the developer client.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_use_id: Option<String>,
        /// Tool input exactly as supplied to the hook, when collection is enabled.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_input: Option<serde_json::Value>,
    },
}

/// Request to register a sandbox session.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionRequest {
    /// Credential policy client ID the session's requests present to the gateway.
    pub client_id: String,
    /// Human-readable name for the session, shown in listings.
    pub label: String,
    /// Seconds without a request after which the session expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
}

/// A newly registered sandbox session. The pairing is shown here and nowhere else.
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionCreated {
    /// Identifier used to list and revoke the session.
    pub id: String,
    /// Secret the session's client presents; never shown again.
    pub pairing: String,
    pub client_id: String,
    pub label: String,
    pub created_at_unix_seconds: u64,
    /// When the session expires if it receives no further request.
    pub expires_at_unix_seconds: u64,
    /// Base URL the session's client sends its requests to.
    pub base_url: String,
}

impl std::fmt::Debug for SessionCreated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionCreated")
            .field("id", &self.id)
            .field("pairing", &"<redacted>")
            .field("client_id", &self.client_id)
            .field("label", &self.label)
            .field("created_at_unix_seconds", &self.created_at_unix_seconds)
            .field("expires_at_unix_seconds", &self.expires_at_unix_seconds)
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// A registered sandbox session, without its pairing.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub client_id: String,
    pub label: String,
    pub created_at_unix_seconds: u64,
    /// When the session expires if it receives no further request.
    pub expires_at_unix_seconds: u64,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        ConsoleAttach, ContainmentInfo, ControllerConnectionError, ControllerConnectionStatus,
        Launch, LaunchConsole, LaunchRequest, LaunchStatus, ProgramChoice, ProgramPosture,
        SessionCreated, SessionInfo, SessionRequest,
    };

    #[test]
    fn controller_connection_error_is_a_closed_camel_case_set() {
        let status = ControllerConnectionStatus {
            connected: false,
            last_seen_unix_seconds: Some(1),
            last_error: Some(ControllerConnectionError::IdentityRejected),
        };
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            serde_json::json!({
                "connected": false,
                "lastSeenUnixSeconds": 1,
                "lastError": "identityRejected",
            })
        );

        let newer: ControllerConnectionStatus =
            serde_json::from_str(r#"{"connected":false,"lastError":"somethingNew"}"#).unwrap();
        assert_eq!(newer.last_error, Some(ControllerConnectionError::Unknown));
    }

    #[test]
    fn containment_info_is_camel_case_and_omits_an_absent_error() {
        let available = ContainmentInfo {
            executor: r"C:\mxc\x64\wxc-exec.exe".into(),
            engine: "mxc".to_owned(),
            available: true,
            error: None,
            managed_settings_source: None,
            managed_hooks_locked: false,
            managed_mcp_locked: false,
            managed_settings_error: None,
            programs: BTreeMap::new(),
        };
        assert_eq!(
            serde_json::to_value(&available).unwrap(),
            serde_json::json!({
                "executor": r"C:\mxc\x64\wxc-exec.exe",
                "engine": "mxc",
                "available": true,
                "managedSettingsSource": null,
                "managedHooksLocked": false,
                "managedMcpLocked": false
            })
        );

        let unavailable: ContainmentInfo = serde_json::from_str(
            r#"{"executor":"x","engine":"mxc","available":false,"error":"requires user mode"}"#,
        )
        .unwrap();
        assert_eq!(unavailable.error.as_deref(), Some("requires user mode"));
        assert!(!unavailable.available);
        let again: ContainmentInfo =
            serde_json::from_str(&serde_json::to_string(&unavailable).unwrap()).unwrap();
        assert_eq!(again, unavailable);
    }

    #[test]
    fn launch_request_is_camel_case_json() {
        let request = LaunchRequest {
            program: "claude-code".into(),
            cwd: "/work/app".into(),
            args: vec!["--model".into(), "x".into()],
            console: None,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({
                "program": "claude-code",
                "cwd": "/work/app",
                "args": ["--model", "x"],
            })
        );
        let parsed: LaunchRequest = serde_json::from_value(serde_json::json!({
            "program": "claude-code", "cwd": "/work/app", "args": []
        }))
        .unwrap();
        assert!(parsed.args.is_empty());
        assert_eq!(parsed.console, None, "an old client sends no console field");
    }

    #[test]
    fn launch_request_console_attach_round_trips() {
        let request = LaunchRequest {
            program: "claude-code".into(),
            cwd: "/work/app".into(),
            args: vec![],
            console: Some(ConsoleAttach { client_pid: 4242 }),
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["console"], serde_json::json!({ "clientPid": 4242 }));
        let parsed: LaunchRequest = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, request);
    }

    #[test]
    fn launch_without_console_decodes_as_window() {
        let mut value = serde_json::to_value(Launch {
            launch_id: "0123456789abcdef".into(),
            program: "claude-code".into(),
            cwd: "/work/app".into(),
            container_id: "agentdesktop-0123456789abcdef".into(),
            status: LaunchStatus::Running,
            exit_code: None,
            console: LaunchConsole::Attached,
            started_at_unix_seconds: 1_700_000_000,
            ended_at_unix_seconds: None,
            integrity_drift: false,
            managed_settings_source: None,
            managed_hooks_locked: false,
            managed_mcp_locked: false,
            session_id: String::new(),
        })
        .unwrap();
        value.as_object_mut().unwrap().remove("console");
        let parsed: Launch = serde_json::from_value(value).unwrap();
        assert_eq!(
            parsed.console,
            LaunchConsole::Window,
            "an old daemon's answer has no console field"
        );
    }

    #[test]
    fn launch_serializes_with_camel_case_keys_and_lowercase_status() {
        let running = Launch {
            launch_id: "0123456789abcdef".into(),
            program: "claude-code".into(),
            cwd: "/work/app".into(),
            container_id: "agentdesktop-0123456789abcdef".into(),
            status: LaunchStatus::Running,
            exit_code: None,
            started_at_unix_seconds: 1_700_000_000,
            ended_at_unix_seconds: None,
            session_id: "test-session-id".into(),
            integrity_drift: false,
            managed_settings_source: None,
            managed_hooks_locked: false,
            managed_mcp_locked: false,
            console: crate::model::LaunchConsole::Window,
        };
        let json = serde_json::to_value(&running).unwrap();
        assert_eq!(json["launchId"], "0123456789abcdef");
        assert_eq!(json["containerId"], "agentdesktop-0123456789abcdef");
        assert_eq!(json["status"], "running");
        assert_eq!(json["startedAtUnixSeconds"], 1_700_000_000u64);
        assert_eq!(json["sessionId"], "test-session-id");
        assert!(
            json.get("credentialExpiresAtUnixSeconds").is_none(),
            "credentialExpiresAtUnixSeconds is removed"
        );
        assert!(json.get("integrityDrift").is_none());
        assert_eq!(json["managedSettingsSource"], serde_json::Value::Null);
        assert_eq!(json["managedHooksLocked"], false);
        assert_eq!(json["managedMcpLocked"], false);
        let again: Launch = serde_json::from_value(json).unwrap();
        assert_eq!(again, running);

        let exited = Launch {
            status: LaunchStatus::Exited,
            exit_code: Some(3),
            ended_at_unix_seconds: Some(1_700_000_100),
            ..running
        };
        let json = serde_json::to_value(&exited).unwrap();
        assert_eq!(json["status"], "exited");
        assert_eq!(json["exitCode"], 3);
        assert_eq!(json["endedAtUnixSeconds"], 1_700_000_100u64);
        assert_eq!(serde_json::from_value::<Launch>(json).unwrap(), exited);
    }

    #[test]
    fn launch_integrity_drift_round_trip() {
        let without_drift = Launch {
            launch_id: "0123456789abcdef".into(),
            program: "claude-code".into(),
            cwd: "/work/app".into(),
            container_id: "agentdesktop-0123456789abcdef".into(),
            status: LaunchStatus::Running,
            exit_code: None,
            started_at_unix_seconds: 1_700_000_000,
            ended_at_unix_seconds: None,
            session_id: "test-session-id".into(),
            integrity_drift: false,
            managed_settings_source: None,
            managed_hooks_locked: false,
            managed_mcp_locked: false,
            console: crate::model::LaunchConsole::Window,
        };
        let json = serde_json::to_value(&without_drift).unwrap();
        assert!(json.get("integrityDrift").is_none());
        let parsed: Launch = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, without_drift);
        assert!(!parsed.integrity_drift);

        let with_drift = Launch {
            integrity_drift: true,
            ..without_drift
        };
        let json = serde_json::to_value(&with_drift).unwrap();
        assert_eq!(json["integrityDrift"], true);
        let parsed: Launch = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, with_drift);
        assert!(parsed.integrity_drift);
    }

    #[test]
    fn launch_and_containment_info_round_trip_with_managed_settings_fields() {
        use super::ManagedSettingsSource;

        // Launch with managed-settings fields
        let launch_with_fields = Launch {
            launch_id: "0123456789abcdef".into(),
            program: "claude-code".into(),
            cwd: "/work/app".into(),
            container_id: "agentdesktop-0123456789abcdef".into(),
            status: LaunchStatus::Running,
            exit_code: None,
            started_at_unix_seconds: 1_700_000_000,
            ended_at_unix_seconds: None,
            session_id: "test-session-id".into(),
            managed_settings_source: Some(ManagedSettingsSource::File),
            managed_hooks_locked: true,
            managed_mcp_locked: false,
            integrity_drift: false,
            console: crate::model::LaunchConsole::Window,
        };
        let json = serde_json::to_value(&launch_with_fields).unwrap();
        assert_eq!(json["managedSettingsSource"], "file");
        assert_eq!(json["managedHooksLocked"], true);
        assert_eq!(json["managedMcpLocked"], false);
        let again: Launch = serde_json::from_value(json).unwrap();
        assert_eq!(again, launch_with_fields);

        // Launch without managed-settings fields (defaults)
        let launch_without_fields = Launch {
            launch_id: "fedcba9876543210".into(),
            program: "claude-code".into(),
            cwd: "/work/app".into(),
            container_id: "agentdesktop-fedcba9876543210".into(),
            status: LaunchStatus::Running,
            exit_code: None,
            started_at_unix_seconds: 1_700_000_000,
            ended_at_unix_seconds: None,
            session_id: "test-session-id".into(),
            managed_settings_source: None,
            managed_hooks_locked: false,
            managed_mcp_locked: false,
            integrity_drift: false,
            console: crate::model::LaunchConsole::Window,
        };
        let json = serde_json::to_value(&launch_without_fields).unwrap();
        assert_eq!(json["managedSettingsSource"], serde_json::Value::Null);
        assert_eq!(json["managedHooksLocked"], false);
        assert_eq!(json["managedMcpLocked"], false);
        let again: Launch = serde_json::from_value(json).unwrap();
        assert_eq!(again, launch_without_fields);

        // ContainmentInfo with managed-settings fields
        let info_with_fields = ContainmentInfo {
            executor: r"C:\mxc\x64\wxc-exec.exe".into(),
            engine: "mxc".to_owned(),
            available: true,
            error: None,
            managed_settings_source: Some(ManagedSettingsSource::Hklm),
            managed_hooks_locked: false,
            managed_mcp_locked: true,
            managed_settings_error: Some("cannot read file".into()),
            programs: BTreeMap::new(),
        };
        let json = serde_json::to_value(&info_with_fields).unwrap();
        assert_eq!(json["managedSettingsSource"], "hklm");
        assert_eq!(json["managedHooksLocked"], false);
        assert_eq!(json["managedMcpLocked"], true);
        assert_eq!(json["managedSettingsError"], "cannot read file");
        let again: ContainmentInfo = serde_json::from_value(json).unwrap();
        assert_eq!(again, info_with_fields);

        // ContainmentInfo without managed-settings fields
        let info_without_fields = ContainmentInfo {
            executor: "x".into(),
            engine: "mxc".to_owned(),
            available: false,
            error: Some("requires user mode".into()),
            managed_settings_source: None,
            managed_hooks_locked: false,
            managed_mcp_locked: false,
            managed_settings_error: None,
            programs: BTreeMap::new(),
        };
        let json = serde_json::to_value(&info_without_fields).unwrap();
        assert_eq!(json["managedSettingsSource"], serde_json::Value::Null);
        assert_eq!(json["managedHooksLocked"], false);
        assert_eq!(json["managedMcpLocked"], false);
        assert!(
            !json
                .as_object()
                .unwrap()
                .contains_key("managedSettingsError")
        );
        let again: ContainmentInfo = serde_json::from_value(json).unwrap();
        assert_eq!(again, info_without_fields);
    }

    // PR 6 (AC-2): ContainmentInfo.programs / ProgramPosture.

    fn containment_info_without_programs() -> ContainmentInfo {
        ContainmentInfo {
            executor: r"C:\mxc\x64\wxc-exec.exe".into(),
            engine: "mxc".to_owned(),
            available: true,
            error: None,
            managed_settings_source: None,
            managed_hooks_locked: false,
            managed_mcp_locked: false,
            managed_settings_error: None,
            programs: BTreeMap::new(),
        }
    }

    #[test]
    fn containment_info_omits_programs_when_empty() {
        let json = serde_json::to_value(containment_info_without_programs()).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("programs"),
            "{json}"
        );
    }

    #[test]
    fn program_posture_serializes_camel_case_with_the_frozen_field_names() {
        let posture = ProgramPosture {
            mode: crate::config::ContainmentMode::Wrap,
            executable: r"C:\Program Files\Anthropic\ClaudeCode\claude.exe".to_owned(),
            aliases_present: true,
            violations: 2,
            last_violation_unix_seconds: Some(1_700_000_000),
        };
        assert_eq!(
            serde_json::to_value(&posture).unwrap(),
            serde_json::json!({
                "mode": "wrap",
                "executable": r"C:\Program Files\Anthropic\ClaudeCode\claude.exe",
                "aliasesPresent": true,
                "violations": 2,
                "lastViolationUnixSeconds": 1_700_000_000u64,
            })
        );
    }

    #[test]
    fn program_posture_omits_last_violation_when_absent() {
        let posture = ProgramPosture {
            mode: crate::config::ContainmentMode::Native,
            executable: r"C:\x\claude.exe".to_owned(),
            aliases_present: false,
            violations: 0,
            last_violation_unix_seconds: None,
        };
        let json = serde_json::to_value(&posture).unwrap();
        assert!(
            !json
                .as_object()
                .unwrap()
                .contains_key("lastViolationUnixSeconds"),
            "{json}"
        );
        assert_eq!(json["mode"], "native");
    }

    #[test]
    fn containment_info_programs_round_trips_keyed_by_launch_id() {
        let mut info = containment_info_without_programs();
        info.programs.insert(
            "claude-code".to_owned(),
            ProgramPosture {
                mode: crate::config::ContainmentMode::Wrap,
                executable: r"C:\Program Files\Anthropic\ClaudeCode\claude.exe".to_owned(),
                aliases_present: true,
                violations: 0,
                last_violation_unix_seconds: None,
            },
        );
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["programs"]["claude-code"]["mode"], "wrap");
        let again: ContainmentInfo = serde_json::from_value(json).unwrap();
        assert_eq!(again, info);
    }

    #[test]
    fn session_request_uses_camel_case_and_ttl_seconds_is_optional() {
        let request = SessionRequest {
            client_id: "claude-code".into(),
            label: "lab".into(),
            ttl_seconds: None,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({"clientId": "claude-code", "label": "lab"})
        );
        let with_ttl = SessionRequest {
            ttl_seconds: Some(120),
            ..request.clone()
        };
        assert_eq!(
            serde_json::to_value(&with_ttl).unwrap(),
            serde_json::json!({"clientId": "claude-code", "label": "lab", "ttlSeconds": 120})
        );

        let parsed: SessionRequest =
            serde_json::from_str(r#"{"clientId":"claude-code","label":"lab"}"#).unwrap();
        assert_eq!(parsed, request);
        let parsed: SessionRequest =
            serde_json::from_str(r#"{"clientId":"claude-code","label":"lab","ttlSeconds":120}"#)
                .unwrap();
        assert_eq!(parsed, with_ttl);
        // A newer client may send fields this build does not know.
        let parsed: SessionRequest = serde_json::from_str(
            r#"{"clientId":"claude-code","label":"lab","somethingNew":{"a":1}}"#,
        )
        .unwrap();
        assert_eq!(parsed, request);
        // The wire names are camelCase only.
        assert!(
            serde_json::from_str::<SessionRequest>(r#"{"client_id":"claude-code","label":"lab"}"#)
                .is_err()
        );
    }

    #[test]
    fn session_created_round_trips_with_camel_case_field_names() {
        let created = SessionCreated {
            id: "0123456789abcdef".into(),
            pairing: "p".repeat(43),
            client_id: "claude-code".into(),
            label: "lab".into(),
            created_at_unix_seconds: 1_000,
            expires_at_unix_seconds: 29_800,
            base_url: "http://127.0.0.1:4000/sandbox".into(),
        };
        let json = serde_json::to_value(&created).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "id": "0123456789abcdef",
                "pairing": "p".repeat(43),
                "clientId": "claude-code",
                "label": "lab",
                "createdAtUnixSeconds": 1_000,
                "expiresAtUnixSeconds": 29_800,
                "baseUrl": "http://127.0.0.1:4000/sandbox",
            })
        );
        assert_eq!(
            serde_json::from_value::<SessionCreated>(json).unwrap(),
            created
        );
    }

    #[test]
    fn session_created_debug_redacts_the_pairing() {
        let created = SessionCreated {
            id: "0123456789abcdef".into(),
            pairing: "secret-pairing-value".into(),
            client_id: "claude-code".into(),
            label: "lab".into(),
            created_at_unix_seconds: 1_000,
            expires_at_unix_seconds: 29_800,
            base_url: "http://127.0.0.1:4000/sandbox".into(),
        };
        let debug = format!("{created:?}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(!debug.contains("secret-pairing-value"), "{debug}");
    }

    #[test]
    fn session_info_has_no_pairing_and_uses_camel_case() {
        let info = SessionInfo {
            id: "0123456789abcdef".into(),
            client_id: "claude-code".into(),
            label: "lab".into(),
            created_at_unix_seconds: 1_000,
            expires_at_unix_seconds: 29_800,
        };
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "id": "0123456789abcdef",
                "clientId": "claude-code",
                "label": "lab",
                "createdAtUnixSeconds": 1_000,
                "expiresAtUnixSeconds": 29_800,
            })
        );
        assert_eq!(serde_json::from_value::<SessionInfo>(json).unwrap(), info);
        assert_eq!(info.clone(), info);
    }

    #[test]
    fn program_choice_is_tagged_by_mode() {
        let selected = ProgramChoice::Selected {
            programs: ["codex".to_owned()].into(),
        };
        assert_eq!(
            serde_json::to_value(&selected).unwrap(),
            serde_json::json!({ "mode": "selected", "programs": ["codex"] })
        );
        assert_eq!(
            serde_json::from_str::<ProgramChoice>(r#"{"mode":"none"}"#).unwrap(),
            ProgramChoice::None
        );
        assert!(ProgramChoice::default().includes("claude-code"));
        assert!(!ProgramChoice::None.includes("claude-code"));
        assert!(selected.includes("codex"));
        assert!(!selected.includes("claude-code"));
    }
}
