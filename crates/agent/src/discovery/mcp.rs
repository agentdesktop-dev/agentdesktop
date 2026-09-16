use std::path::Path;

use agentdesktop_core::model::McpServer;
use serde_json::{Map, Value};
use url::Url;

/// Projects the shared JSON entry shape; each adapter supplies its native enablement rule.
pub(super) fn from_json_map(
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
pub(super) fn server(
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
    // Never expand templates or accept forms the parser would repair into a different endpoint.
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
pub(super) fn from_mcp_servers_file(
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serialized_inventory_omits_secrets_and_preserves_native_fields() {
        let source = Path::new("native").join("mcp.json");
        let document = json!({
            " native name ": {
                "command": " native command ",
                "args": ["ARG_SENTINEL"],
                "env": {"TOKEN": "ENV_SENTINEL"}
            },
            "remote": {
                "type": "streamable-http",
                "url": "https://USER_SENTINEL:PASS_SENTINEL@example.com/PATH_SENTINEL/mcp?key=QUERY_SENTINEL#FRAGMENT_SENTINEL",
                "headers": {"Authorization": "HEADER_SENTINEL"}
            }
        });
        let servers = from_json_map(document.as_object().unwrap(), &source, |_| true);
        assert_eq!(servers.len(), 2);
        let local = servers.iter().find(|s| s.name == " native name ").unwrap();
        assert_eq!(local.command.as_deref(), Some(" native command "));
        assert_eq!(local.transport, "stdio");
        let remote = servers.iter().find(|s| s.name == "remote").unwrap();
        assert_eq!(remote.transport, "http");
        assert_eq!(remote.url.as_deref(), Some("https://example.com/"));
        assert!(servers.iter().all(|s| s.source == source && s.enabled));
        let serialized = serde_json::to_string(&servers).unwrap();
        assert!(!serialized.contains("SENTINEL"));
        for field in ["\"args\"", "\"env\"", "\"headers\""] {
            assert!(!serialized.contains(field));
        }
    }

    #[test]
    fn server_discloses_localhost_origin_without_changing_other_fields() {
        let source = Path::new("native").join("servers.toml");
        let item = server(
            " name ",
            "sse",
            Some(" command "),
            Some(
                "http://USER_SENTINEL:PASS_SENTINEL@localhost:8080/PATH_SENTINEL/mcp?auth=QUERY_SENTINEL#FRAGMENT_SENTINEL",
            ),
            false,
            &source,
        );
        assert_eq!(item.name, " name ");
        assert_eq!(item.transport, "sse");
        assert_eq!(item.command.as_deref(), Some(" command "));
        assert_eq!(item.url.as_deref(), Some("http://localhost:8080/"));
        assert_eq!(item.source, source);
        assert!(!item.enabled);
        assert!(!serde_json::to_string(&item).unwrap().contains("SENTINEL"));
    }

    #[test]
    fn undisclosable_urls_keep_registrations_and_valid_siblings() {
        for raw in [
            "",
            "/mcp",
            "//example.com/mcp",
            "file:///tmp/mcp",
            "javascript:alert(1)",
            " https://example.com",
            "https://example.com ",
            "https://exam\nple.com",
            "https://example.com/a b",
            "https://example.com/\u{a0}",
            "https://example.com/\0",
            "http://",
            "https:example.com",
            "https:/example.com",
            "https:///example.com",
            "https:////example.com",
            "https://?query",
            "https://#fragment",
            "https://user:pass@",
            "https://example.com:bad",
            r"https:\\example.com",
            r"https://example.com\@other.com/mcp",
            r"https://user:pass@\example.com",
            "https://exam%5Cple.com/mcp",
            "unix:///tmp/SOCKET_SENTINEL.sock",
            "pipe:///PIPE_SENTINEL",
            r"\\.\pipe\PIPE_SENTINEL",
            "${env:URL_SENTINEL}",
            "${input:URL_SENTINEL}",
            "https://${env:HOST_SENTINEL}/mcp",
            "https://example.com/${env:PATH_SENTINEL}",
            "https://example.com/${TOKEN_SENTINEL:-fallback}",
            "https://example.com/{env:TOKEN_SENTINEL}",
            "https://example.com/mcp?token=${input:QUERY_SENTINEL}",
        ] {
            let source = Path::new("mcp.json");
            let item = server("native", "stdio", Some("cmd"), Some(raw), false, source);
            assert_eq!(item.name, "native");
            assert_eq!(item.transport, "stdio");
            assert_eq!(item.command.as_deref(), Some("cmd"));
            assert_eq!(item.url, None, "must not disclose {raw:?}");
            assert_eq!(item.source, source);
            assert!(!item.enabled);
            let document = json!({
                "native": {"command": "cmd", "url": raw},
                "endpoint-only": {"url": raw},
                "good": {"command": "ok"},
                "malformed": null
            });
            let servers = from_json_map(document.as_object().unwrap(), source, |_| true);
            assert_eq!(servers.len(), 3);
            assert!(
                servers
                    .iter()
                    .all(|server| server.url.is_none() && server.source == source)
            );
            for name in ["native", "endpoint-only"] {
                let server = servers.iter().find(|server| server.name == name).unwrap();
                assert_eq!(server.transport, "http");
                assert_eq!(
                    server.command.as_deref(),
                    (name == "native").then_some("cmd")
                );
                assert!(server.enabled);
            }
            assert_eq!(
                servers
                    .iter()
                    .find(|server| server.name == "good")
                    .unwrap()
                    .command
                    .as_deref(),
                Some("ok")
            );
            let serialized = serde_json::to_string(&servers).unwrap();
            assert!(!serialized.contains("\"url\""));
            assert!(!serialized.contains("SENTINEL"));
        }
    }

    #[test]
    fn json_projection_uses_adapter_enablement() {
        let document = json!({
            "on": {"command": "cmd", "native-active": true, "disabled": true},
            "off": {"command": "cmd", "native-active": false, "enabled": true}
        });
        let servers = from_json_map(
            document.as_object().unwrap(),
            Path::new("native"),
            |entry| entry.get("native-active").and_then(Value::as_bool) == Some(true),
        );
        assert_eq!(servers.len(), 2);
        for server in servers {
            assert_eq!(server.enabled, server.name == "on");
        }
    }

    #[test]
    fn malformed_entries_do_not_hide_siblings_or_change_transport_decoding() {
        let document = json!({
            "null": null, "array": [], "scalar": "cmd", "empty": {},
            "wrong": {"command": 1, "url": false, "type": []},
            "sse": {"type": "sse", "url": "https://example.com/mcp"},
            "inferred": {"command": "cmd", "url": "http://localhost/mcp"},
            "typed": {"type": "native", "command": 1, "url": false},
            "empty strings": {"command": "", "type": ""}
        });
        let source = Path::new("native").join("config.json");
        let servers = from_json_map(document.as_object().unwrap(), &source, |_| true);
        assert_eq!(servers.len(), 4);
        for (name, transport) in [
            ("sse", "sse"),
            ("inferred", "http"),
            ("typed", "native"),
            ("empty strings", ""),
        ] {
            let item = servers.iter().find(|s| s.name == name).unwrap();
            assert_eq!(item.transport, transport);
            assert_eq!(item.source, source);
        }
        let typed = servers.iter().find(|s| s.name == "typed").unwrap();
        assert_eq!(typed.command, None);
        assert_eq!(typed.url, None);
    }
}
