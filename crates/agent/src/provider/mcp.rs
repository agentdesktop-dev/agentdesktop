use std::path::Path;

use agentdesktop_core::model::McpServer;
use serde_json::{Map, Value};
use url::Url;

/// Projects the shared JSON entry shape; each adapter supplies its native enablement rule.
pub(crate) fn from_json_map(
    servers: &Map<String, Value>,
    source: &Path,
    is_enabled: impl Fn(&Map<String, Value>) -> bool,
) -> Vec<McpServer> {
    servers
        .iter()
        .filter_map(|(name, value)| {
            let entry = value.as_object()?;
            let command = entry.get("command").and_then(Value::as_str);
            let url = entry.get("url").and_then(Value::as_str);
            let transport = entry
                .get("type")
                .and_then(Value::as_str)
                .map(|kind| match kind {
                    "streamable-http" => "http",
                    other => other,
                })
                .or_else(|| url.map(|_| "http"))
                .or_else(|| command.map(|_| "stdio"))?;
            Some(server(
                name,
                transport,
                command,
                url,
                is_enabled(entry),
                source,
            ))
        })
        .collect()
}

/// Retains a native registration even when its endpoint cannot be safely disclosed.
pub(crate) fn server(
    name: &str,
    transport: &str,
    command: Option<&str>,
    url: Option<&str>,
    enabled: bool,
    source: &Path,
) -> McpServer {
    McpServer {
        name: name.to_owned(),
        transport: transport.to_owned(),
        command: command.map(str::to_owned),
        url: url.and_then(disclosed_origin),
        enabled,
        source: source.to_path_buf(),
    }
}

fn disclosed_origin(raw: &str) -> Option<String> {
    let (_, authority) = raw.split_once("://")?;
    if authority.is_empty()
        || authority.starts_with(['/', '?', '#'])
        || raw
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '\\' | '$' | '{' | '}'))
    {
        return None;
    }
    let url = Url::parse(raw).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    Some(format!("{}/", url.origin().ascii_serialization()))
}

/// Reads the common `{"mcpServers": {...}}` JSON layout used by Claude Code, Claude Desktop, and others.
pub(crate) fn from_mcp_servers_file(
    path: &Path,
    is_enabled: impl Fn(&Map<String, Value>) -> bool,
) -> Vec<McpServer> {
    super::files::read_json::<Value>(path)
        .and_then(|document| {
            document
                .get("mcpServers")
                .and_then(Value::as_object)
                .map(|servers| from_json_map(servers, path, is_enabled))
        })
        .unwrap_or_default()
}
