use std::{collections::BTreeMap, path::PathBuf, time::Duration};

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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnrollmentStatus {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_url: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmGatewayCredential {
    pub credential: String,
    pub expires_at_unix_seconds: u64,
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
    use super::{ControllerConnectionError, ControllerConnectionStatus};

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
}
