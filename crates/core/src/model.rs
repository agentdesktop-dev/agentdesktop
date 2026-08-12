use std::{collections::BTreeMap, path::PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Discovery {
    pub agents: Vec<Agent>,
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
pub struct InferenceGatewayCredential {
    pub credential: String,
    pub expires_at_unix_seconds: u64,
}
