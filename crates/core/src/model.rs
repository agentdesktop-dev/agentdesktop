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

/// Estimated LLM usage reported by the configured gateway.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmUsageSummary {
    pub from: String,
    pub to: String,
    /// ISO 4217 code for every `estimated_cost` in this report.
    pub currency: String,
    pub requests: u64,
    pub total_tokens: u64,
    pub estimated_cost: f64,
    #[serde(default)]
    pub breakdown: Vec<LlmUsageBreakdown>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LlmUsageRange {
    Hour,
    #[default]
    Day,
    Week,
    Month,
}

impl LlmUsageRange {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hour => "hour",
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
        }
    }

    pub fn duration(self) -> Duration {
        match self {
            Self::Hour => Duration::from_secs(60 * 60),
            Self::Day => Duration::from_secs(24 * 60 * 60),
            Self::Week => Duration::from_secs(7 * 24 * 60 * 60),
            Self::Month => Duration::from_secs(30 * 24 * 60 * 60),
        }
    }
}

impl std::str::FromStr for LlmUsageRange {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "hour" => Ok(Self::Hour),
            "day" => Ok(Self::Day),
            "week" => Ok(Self::Week),
            "month" => Ok(Self::Month),
            _ => Err("invalid usage range"),
        }
    }
}

/// Estimated LLM usage for one model and client agent combination.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmUsageBreakdown {
    pub model: String,
    pub agent: String,
    pub requests: u64,
    pub total_tokens: u64,
    pub estimated_cost: f64,
}

/// Estimated LLM usage across a fleet, grouped by the enrolled device that sent each request.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmFleetUsageSummary {
    pub from: String,
    pub to: String,
    /// ISO 4217 code for every `estimated_cost` in this report.
    pub currency: String,
    pub requests: u64,
    pub total_tokens: u64,
    pub estimated_cost: f64,
    #[serde(default)]
    pub devices: Vec<LlmDeviceUsage>,
}

/// Estimated LLM usage attributed to one device. `device_id` is `None` for
/// requests whose gateway credential carried no device identity.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmDeviceUsage {
    pub device_id: Option<String>,
    pub hostname: Option<String>,
    pub requests: u64,
    pub total_tokens: u64,
    pub estimated_cost: f64,
}

/// Metadata-only LLM requests for one model and client agent combination.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmUsageInteractions {
    /// ISO 4217 code for every `estimated_cost` in this page.
    pub currency: String,
    pub interactions: Vec<LlmUsageInteraction>,
    pub next_cursor: Option<String>,
}

/// Metadata captured for one LLM request. Prompt and response content are excluded.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LlmUsageInteraction {
    pub id: String,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub duration_ms: Option<u64>,
    pub http_status: Option<u16>,
    pub failed: bool,
    pub operation: Option<String>,
    pub provider: Option<String>,
    pub request_model: String,
    pub response_model: Option<String>,
    pub agent: String,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub estimated_cost: Option<f64>,
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
