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
    /// Proxy auto-config (PAC) set by browser policy, one entry per browser
    /// that is installed or has policy, as read from machine policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub browser_pacs: Vec<BrowserPac>,
    /// One fetch per distinct PAC URL in `browser_pacs`: the identity of the
    /// PAC this device gets (SHA-256 of the body), never its content.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pac_fetches: Vec<PacFetch>,
}

/// The result of fetching one PAC URL, as a browser on this device would.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PacFetch {
    pub url: String,
    /// Lowercase hex SHA-256 of the response body, when the fetch succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Body size in bytes, when the fetch succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Why the fetch failed, at most 1024 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The PAC a browser's policy sets, as read from machine policy.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BrowserPac {
    /// `chrome`, `edge`, `chromium` or `firefox`.
    pub browser: String,
    /// How the browser is installed (`system`, `snap`, `flatpak`), or empty
    /// when only policy was found.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub packaging: String,
    /// The PAC URL, when policy selects PAC mode with a non-empty URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pac_url: Option<String>,
    /// The value came from a location the user can override
    /// (`policies/recommended`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recommended: bool,
    /// Firefox `Proxy.Locked`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub locked: bool,
    /// The policy file the value came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PathBuf>,
    /// A read error or conflicting policy files, at most 1024 bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One PAC value per device, derived from its browser rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DevicePac {
    /// No browser's policy sets a PAC.
    Absent,
    /// Every browser row names this PAC URL.
    Found(String),
    /// Browsers disagree: different URLs, or some have a PAC and some don't.
    Differs,
}

impl DevicePac {
    /// Derives the device value. No browser rows at all is `Absent`.
    pub fn from_browsers(browsers: &[BrowserPac]) -> Self {
        let mut urls = browsers.iter().map(|browser| browser.pac_url.as_deref());
        let Some(first) = urls.next() else {
            return Self::Absent;
        };
        if urls.any(|url| url != first) {
            return Self::Differs;
        }
        match first {
            Some(url) => Self::Found(url.to_owned()),
            None => Self::Absent,
        }
    }
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

#[cfg(test)]
mod tests {
    use super::{
        BrowserPac, ControllerConnectionError, ControllerConnectionStatus, DevicePac, ProgramChoice,
    };

    fn browser(name: &str, pac_url: Option<&str>) -> BrowserPac {
        BrowserPac {
            browser: name.to_owned(),
            pac_url: pac_url.map(str::to_owned),
            ..BrowserPac::default()
        }
    }

    #[test]
    fn device_pac_is_one_value_when_browsers_agree() {
        let pac = "https://pac.corp.example/ai.pac";
        assert_eq!(DevicePac::from_browsers(&[]), DevicePac::Absent);
        assert_eq!(
            DevicePac::from_browsers(&[browser("chrome", None), browser("firefox", None)]),
            DevicePac::Absent
        );
        assert_eq!(
            DevicePac::from_browsers(&[
                browser("chrome", Some(pac)),
                browser("firefox", Some(pac))
            ]),
            DevicePac::Found(pac.to_owned())
        );
        assert_eq!(
            DevicePac::from_browsers(&[browser("chrome", Some(pac)), browser("firefox", None)]),
            DevicePac::Differs
        );
        assert_eq!(
            DevicePac::from_browsers(&[
                browser("chrome", Some(pac)),
                browser("edge", Some("https://other.example/ai.pac")),
            ]),
            DevicePac::Differs
        );
    }

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
