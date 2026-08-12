use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};
use serde_json::Value;

use super::metadata;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_in_path("claude")?;
    Some(Agent {
        version: metadata::version_after_component(&executable, "versions"),
        executable,
        kind: "claude-code".to_owned(),
        mcp_servers: discover_mcp_servers(),
        skills: metadata::discover_skills(skill_roots()),
    })
}

fn discover_mcp_servers() -> Vec<McpServer> {
    let mut servers = Vec::new();
    let managed = PathBuf::from("/etc/claude-code/managed-mcp.json");
    servers.extend(mcp_servers_from_json(&managed));

    for home in metadata::user_home_dirs() {
        let user = home.join(".claude.json");
        servers.extend(mcp_servers_from_json(&user));
    }
    servers
}

fn skill_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    roots.insert(PathBuf::from("/etc/claude-code/skills"));
    roots.extend(metadata::current_dir_ancestors(Path::new(".claude/skills")));
    for home in metadata::user_home_dirs() {
        roots.insert(home.join(".claude/skills"));
        for root in installed_plugin_roots(&home) {
            roots.insert(root);
        }
    }
    roots.into_iter().collect()
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

pub(super) fn mcp_servers_from_json(path: &Path) -> Vec<McpServer> {
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
        .filter_map(|(name, value)| {
            let server = value.as_object()?;
            let command = server
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let url = server.get("url").and_then(Value::as_str).map(str::to_owned);
            let transport = server
                .get("type")
                .and_then(Value::as_str)
                .map(|transport| match transport {
                    "streamable-http" => "http",
                    other => other,
                })
                .or_else(|| url.as_ref().map(|_| "http"))
                .or_else(|| command.as_ref().map(|_| "stdio"))?;
            Some(McpServer {
                name: name.clone(),
                transport: transport.to_owned(),
                command,
                url,
                enabled: server.get("disabled").and_then(Value::as_bool) != Some(true),
                source: source.to_path_buf(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::mcp_servers_from_value;

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
}
