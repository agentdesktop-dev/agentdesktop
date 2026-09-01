use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Serialize)]
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

/// A point-in-time, secret-free local audit of developer-tool access.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccessReport {
    pub generated_at_unix_ms: u64,
    pub status: AccessReportStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub agents: Vec<AgentAccessReport>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AccessReportStatus {
    Ready,
    Unavailable,
}

/// Local access audit for one discovered developer-tool installation.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAccessReport {
    pub kind: String,
    pub executable: PathBuf,
    pub version: Option<String>,
    /// Home directory whose configuration and history produced this report.
    pub user_home: PathBuf,
    /// Access declared by configuration or implied by a configured capability.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<AccessCapability>,
    /// Resources found in structured local session records.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<AccessObservation>,
    /// Actionable findings derived from current configuration and defaults.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<AccessFinding>,
    /// Which evidence sources were inspected and how complete each scan was.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coverage: Vec<AccessCoverage>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct AccessCapability {
    pub category: AccessCategory,
    pub resource: String,
    pub operations: Vec<AccessOperation>,
    pub decision: AccessDecision,
    pub enforcement: AccessEnforcement,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    pub source: AccessSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct AccessObservation {
    pub category: AccessCategory,
    pub resource: String,
    pub operation: AccessOperation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    pub count: u64,
    /// Latest modification time among history files containing this observation.
    /// This is evidence freshness, not necessarily the event's execution time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_updated_at_unix_ms: Option<u64>,
    pub confidence: AccessConfidence,
    pub source: AccessSource,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccessFinding {
    pub severity: AccessSeverity,
    pub title: String,
    pub detail: String,
    pub category: AccessCategory,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<PathBuf>,
    /// Configuration source that can be reviewed to remediate this finding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<AccessSource>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AccessCoverage {
    pub source: AccessSourceKind,
    pub status: AccessCoverageStatus,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub struct AccessSource {
    pub kind: AccessSourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum AccessCategory {
    Filesystem,
    Network,
    Execution,
    ExternalService,
    Credential,
    Browser,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum AccessOperation {
    Read,
    Write,
    Execute,
    Connect,
    Use,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum AccessDecision {
    Allow,
    Ask,
    Deny,
    AutoReview,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum AccessEnforcement {
    Sandbox,
    Harness,
    None,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum AccessSourceKind {
    Configuration,
    Default,
    Mcp,
    History,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum AccessConfidence {
    High,
    Heuristic,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase")]
pub enum AccessSeverity {
    Notice,
    Warning,
    Critical,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AccessCoverageStatus {
    Complete,
    Partial,
    Unavailable,
    Unsupported,
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
pub struct Health {
    pub status: String,
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
