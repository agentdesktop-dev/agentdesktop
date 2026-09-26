use super::ClaudeCode;

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};
use serde_json::Value;

use crate::provider::metadata;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_executable("claude", executable_candidates())?;
    Some(Agent {
        version: metadata::version_after_component(&executable, "versions"),
        executable,
        kind: ClaudeCode::ID.to_owned(),
        mcp_servers: discover_mcp_servers(),
        skills: metadata::discover_skills(skill_roots()),
    })
}

fn executable_candidates() -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        candidates.insert(home.join(".local/bin/claude"));
        candidates.insert(home.join(".npm-global/bin/claude"));
        #[cfg(windows)]
        {
            candidates.insert(home.join(".local/bin/claude.exe"));
            candidates.insert(home.join("AppData/Roaming/npm/claude.cmd"));
        }
    }
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude"),
    ]);
    candidates.into_iter().collect()
}

fn discover_mcp_servers() -> Vec<McpServer> {
    let mut servers = Vec::new();
    if let Some(root) = managed_root() {
        servers.extend(mcp_servers_from_json(&root.join("managed-mcp.json")));
        // Servers pushed through the `managedMcpServers` managed setting — including the
        // ones Agentdesktop itself writes to `managed-settings.d/50-agentdesktop.json`.
        for path in managed_settings_files(&root) {
            servers.extend(managed_mcp_servers_from_json(&path));
        }
    }

    for home in metadata::user_home_dirs() {
        let user = home.join(".claude.json");
        servers.extend(mcp_servers_from_json(&user));
    }
    servers
}

fn skill_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    roots.extend(managed_root().map(|root| root.join("skills")));
    roots.extend(metadata::current_dir_ancestors(Path::new(".claude/skills")));
    for home in metadata::user_home_dirs() {
        roots.insert(home.join(".claude/skills"));
        for root in installed_plugin_roots(&home) {
            roots.insert(root);
        }
    }
    roots.into_iter().collect()
}

fn managed_root() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    return Some(PathBuf::from("/etc/claude-code"));
    #[cfg(target_os = "macos")]
    return Some(PathBuf::from("/Library/Application Support/ClaudeCode"));
    #[cfg(windows)]
    return metadata::env_path("ProgramFiles").map(|path| path.join("ClaudeCode"));
}

/// `managed-settings.json` followed by the `managed-settings.d/*.json` drop-ins in the
/// lexical order Claude Code merges them.
fn managed_settings_files(root: &Path) -> Vec<PathBuf> {
    let mut files = vec![root.join("managed-settings.json")];
    if let Ok(entries) = fs::read_dir(root.join("managed-settings.d")) {
        let mut drop_ins: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
            })
            .collect();
        drop_ins.sort();
        files.extend(drop_ins);
    }
    files
}

fn installed_plugin_roots(home: &Path) -> Vec<PathBuf> {
    let path = home.join(".claude/plugins/installed_plugins.json");
    let Ok(contents) = fs::read(&path) else {
        return Vec::new();
    };
    let Ok(document) = serde_json::from_slice::<Value>(&contents) else {
        return Vec::new();
    };
    document
        .get("plugins")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|plugins| plugins.values())
        .filter_map(Value::as_array)
        .flatten()
        .filter_map(|install| install.get("installPath").and_then(Value::as_str))
        .map(PathBuf::from)
        .collect()
}

pub(in crate::provider) fn mcp_servers_from_json(path: &Path) -> Vec<McpServer> {
    let Ok(contents) = fs::read(path) else {
        return Vec::new();
    };
    let Ok(document) = serde_json::from_slice::<Value>(&contents) else {
        return Vec::new();
    };
    mcp_servers_from_value(&document, path)
}

fn mcp_servers_from_value(document: &Value, source: &Path) -> Vec<McpServer> {
    let Some(servers) = document.get("mcpServers").and_then(Value::as_object) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(name, value)| mcp_server_from_entry(name, value, source))
        .collect()
}

fn managed_mcp_servers_from_json(path: &Path) -> Vec<McpServer> {
    let Ok(contents) = fs::read(path) else {
        return Vec::new();
    };
    let Ok(document) = serde_json::from_slice::<Value>(&contents) else {
        return Vec::new();
    };
    document
        .get("managedMcpServers")
        .map(|servers| managed_mcp_servers_from_value(servers, path))
        .unwrap_or_default()
}

/// Reads a `managedMcpServers` value. Claude Code uses an object keyed by server name;
/// Claude Desktop uses an array of entries carrying a `name`, which MDM payloads encode as
/// a JSON string because property lists cannot hold arbitrary JSON.
pub(in crate::provider) fn managed_mcp_servers_from_value(
    servers: &Value,
    source: &Path,
) -> Vec<McpServer> {
    match servers {
        Value::String(encoded) => serde_json::from_str::<Value>(encoded)
            .ok()
            .filter(|decoded| !decoded.is_string())
            .map(|decoded| managed_mcp_servers_from_value(&decoded, source))
            .unwrap_or_default(),
        Value::Object(servers) => servers
            .iter()
            .filter_map(|(name, value)| mcp_server_from_entry(name, value, source))
            .collect(),
        Value::Array(servers) => servers
            .iter()
            .filter_map(|value| {
                let name = value.get("name").and_then(Value::as_str)?;
                mcp_server_from_entry(name, value, source)
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn mcp_server_from_entry(name: &str, value: &Value, source: &Path) -> Option<McpServer> {
    let server = value.as_object()?;
    let command = server
        .get("command")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let url = server.get("url").and_then(Value::as_str).map(str::to_owned);
    let transport = server
        .get("type")
        .or_else(|| server.get("transport"))
        .and_then(Value::as_str)
        .map(|transport| match transport {
            "streamable-http" => "http",
            other => other,
        })
        .or_else(|| url.as_ref().map(|_| "http"))
        .or_else(|| command.as_ref().map(|_| "stdio"))?;
    Some(McpServer {
        name: name.to_owned(),
        transport: transport.to_owned(),
        command,
        url,
        enabled: server.get("disabled").and_then(Value::as_bool) != Some(true),
        source: source.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::{managed_mcp_servers_from_value, mcp_servers_from_value};

    #[test]
    fn reads_claude_servers_without_credentials_or_arguments() {
        let servers = mcp_servers_from_value(
            &json!({
                "mcpServers": {
                    "remote": {
                        "type": "streamable-http",
                        "url": "https://example.com/mcp",
                        "headers": { "Authorization": "secret" }
                    },
                    "local": {
                        "command": "npx",
                        "args": ["secret"],
                        "env": { "TOKEN": "secret" }
                    }
                }
            }),
            Path::new(".mcp.json"),
        );

        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "local");
        assert_eq!(servers[0].transport, "stdio");
        assert_eq!(servers[1].transport, "http");
    }

    #[test]
    fn ignores_project_servers_in_claude_user_configuration() {
        let servers = mcp_servers_from_value(
            &json!({
                "mcpServers": {
                    "global": { "command": "global-server" }
                },
                "projects": {
                    "/workspace/one": {
                        "mcpServers": {
                            "local": { "command": "local-server" }
                        }
                    },
                    "/workspace/two": {
                        "mcpServers": {
                            "local": { "command": "local-server" }
                        }
                    }
                }
            }),
            Path::new(".claude.json"),
        );

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "global");
    }

    #[test]
    fn reads_claude_code_managed_servers_keyed_by_name() {
        let servers = managed_mcp_servers_from_value(
            &json!({
                "github": {
                    "type": "http",
                    "url": "https://gateway.example.com/github/mcp",
                    "headers": { "Authorization": "secret" }
                },
                "local": { "command": "server", "args": ["secret"] }
            }),
            Path::new("managed-settings.d/50-agentdesktop.json"),
        );

        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "github");
        assert_eq!(servers[0].transport, "http");
        assert_eq!(servers[1].name, "local");
        assert_eq!(servers[1].transport, "stdio");
    }

    #[test]
    fn reads_claude_desktop_managed_servers_encoded_as_a_json_string() {
        let encoded = json!([
            {
                "name": "github",
                "transport": "http",
                "url": "https://gateway.example.com/github/mcp",
                "oauth": true
            },
            { "transport": "http", "url": "https://unnamed.example.com/mcp" }
        ])
        .to_string();

        let servers = managed_mcp_servers_from_value(
            &json!(encoded),
            Path::new("com.anthropic.claudefordesktop.plist"),
        );

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "github");
        assert_eq!(servers[0].transport, "http");
        assert_eq!(
            servers[0].url.as_deref(),
            Some("https://gateway.example.com/github/mcp")
        );
    }

    #[test]
    fn ignores_malformed_managed_servers() {
        let source = Path::new("managed-settings.json");
        assert!(managed_mcp_servers_from_value(&json!("not json"), source).is_empty());
        assert!(managed_mcp_servers_from_value(&json!("\"nested\""), source).is_empty());
        assert!(managed_mcp_servers_from_value(&json!(42), source).is_empty());
    }
}
