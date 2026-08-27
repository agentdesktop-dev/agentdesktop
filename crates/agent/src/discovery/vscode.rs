use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};
use serde_json::Value;

use super::metadata;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_executable("code", executable_candidates())?;
    let version = version_candidates(&executable)
        .into_iter()
        .find_map(|path| metadata::json_version(&path));
    Some(Agent {
        version,
        executable,
        kind: "vscode".to_owned(),
        mcp_servers: discover_mcp_servers(),
        skills: metadata::discover_skills(skill_roots()),
    })
}

fn discover_mcp_servers() -> Vec<McpServer> {
    mcp_config_paths()
        .into_iter()
        .flat_map(|path| mcp_servers_from_json(&path))
        .collect()
}

fn mcp_config_paths() -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        paths.insert(home.join(".copilot/mcp-config.json"));
        let user_root = user_profile_root(&home);
        paths.insert(user_root.join("mcp.json"));
        if let Ok(profiles) = fs::read_dir(user_root.join("profiles")) {
            paths.extend(
                profiles
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .map(|path| path.join("mcp.json")),
            );
        }
    }
    paths.extend(metadata::current_dir_ancestors(Path::new(
        ".vscode/mcp.json",
    )));
    paths.extend(metadata::current_dir_ancestors(Path::new(".mcp.json")));
    paths.into_iter().collect()
}

#[cfg(target_os = "macos")]
fn user_profile_root(home: &Path) -> PathBuf {
    home.join("Library/Application Support/Code/User")
}

#[cfg(target_os = "linux")]
fn user_profile_root(home: &Path) -> PathBuf {
    home.join(".config/Code/User")
}

#[cfg(windows)]
fn user_profile_root(home: &Path) -> PathBuf {
    home.join("AppData/Roaming/Code/User")
}

fn mcp_servers_from_json(path: &Path) -> Vec<McpServer> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(document) = json5::from_str::<Value>(&contents) else {
        return Vec::new();
    };
    mcp_servers_from_value(&document, path)
}

fn mcp_servers_from_value(document: &Value, source: &Path) -> Vec<McpServer> {
    let Some(servers) = document
        .get("servers")
        .or_else(|| document.get("mcpServers"))
        .and_then(Value::as_object)
    else {
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
                enabled: server.get("disabled").and_then(Value::as_bool) != Some(true)
                    && server.get("enabled").and_then(Value::as_bool) != Some(false),
                source: source.to_path_buf(),
            })
        })
        .collect()
}

fn skill_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for relative in [".github/skills", ".claude/skills", ".agents/skills"] {
        roots.extend(metadata::current_dir_ancestors(Path::new(relative)));
    }
    for home in metadata::user_home_dirs() {
        roots.insert(home.join(".copilot/skills"));
        roots.insert(home.join(".claude/skills"));
        roots.insert(home.join(".agents/skills"));
    }
    roots.into_iter().collect()
}

fn executable_candidates() -> Vec<PathBuf> {
    let candidates = BTreeSet::new();

    #[cfg(target_os = "macos")]
    let candidates = {
        let mut candidates = candidates;
        candidates.insert(PathBuf::from(
            "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
        ));
        for home in metadata::user_home_dirs() {
            candidates.insert(
                home.join("Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"),
            );
        }
        candidates
    };

    #[cfg(windows)]
    let candidates = {
        let mut candidates = candidates;
        for home in metadata::user_home_dirs() {
            candidates.insert(home.join("AppData/Local/Programs/Microsoft VS Code/bin/code.cmd"));
            candidates.insert(home.join("AppData/Local/Programs/Microsoft VS Code/bin/code.exe"));
        }
        for root in [
            metadata::env_path("ProgramFiles"),
            metadata::env_path("ProgramFiles(x86)"),
        ]
        .into_iter()
        .flatten()
        {
            candidates.insert(root.join("Microsoft VS Code/bin/code.cmd"));
            candidates.insert(root.join("Microsoft VS Code/bin/code.exe"));
        }
        candidates
    };

    candidates.into_iter().collect()
}

fn version_candidates(executable: &Path) -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    for executable in [
        Some(executable.to_path_buf()),
        executable.canonicalize().ok(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(directory) = executable.parent() {
            candidates.insert(directory.join("resources/app/package.json"));
            candidates.insert(directory.join("../resources/app/package.json"));
            candidates.insert(directory.join("../../package.json"));
        }
    }
    candidates.extend([
        PathBuf::from("/usr/share/code/resources/app/package.json"),
        PathBuf::from("/usr/lib/code/resources/app/package.json"),
    ]);
    for home in metadata::user_home_dirs() {
        candidates.insert(
            home.join("Applications/Visual Studio Code.app/Contents/Resources/app/package.json"),
        );
    }
    candidates.insert(PathBuf::from(
        "/Applications/Visual Studio Code.app/Contents/Resources/app/package.json",
    ));
    candidates.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use serde_json::json;

    use super::{mcp_servers_from_json, mcp_servers_from_value};

    #[cfg(target_os = "macos")]
    #[test]
    fn uses_macos_vscode_user_mcp_configuration() {
        assert_eq!(
            super::user_profile_root(Path::new("/Users/developer")).join("mcp.json"),
            Path::new("/Users/developer/Library/Application Support/Code/User/mcp.json")
        );
    }

    #[test]
    fn reads_vscode_servers_without_secrets_or_arguments() {
        let servers = mcp_servers_from_value(
            &json!({
                "servers": {
                    "docs": {
                        "type": "http",
                        "url": "https://example.com/mcp",
                        "headers": { "Authorization": "secret" }
                    },
                    "local": {
                        "command": "npx",
                        "args": ["-y", "secret-package"],
                        "env": { "TOKEN": "secret" }
                    },
                    "disabled": {
                        "type": "sse",
                        "url": "https://example.com/events",
                        "enabled": false
                    }
                }
            }),
            Path::new(".vscode/mcp.json"),
        );

        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0].name, "disabled");
        assert_eq!(servers[0].transport, "sse");
        assert!(!servers[0].enabled);
        assert_eq!(servers[1].name, "docs");
        assert_eq!(servers[1].transport, "http");
        assert_eq!(servers[1].url.as_deref(), Some("https://example.com/mcp"));
        assert_eq!(servers[2].name, "local");
        assert_eq!(servers[2].transport, "stdio");
        assert_eq!(servers[2].command.as_deref(), Some("npx"));
    }

    #[test]
    fn reads_portable_copilot_server_format() {
        let servers = mcp_servers_from_value(
            &json!({
                "mcpServers": {
                    "docs": {
                        "type": "streamable-http",
                        "url": "https://example.com/mcp"
                    }
                }
            }),
            Path::new(".copilot/mcp-config.json"),
        );

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].transport, "http");
        assert!(servers[0].enabled);
    }

    #[test]
    fn reads_json_with_comments_and_trailing_commas() {
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-vscode-mcp-{}.json",
            std::process::id()
        ));
        fs::write(
            &path,
            r#"{
                // VS Code configuration files use JSON with comments.
                "servers": {
                    "local": {
                        "type": "stdio",
                        "command": "npx",
                    },
                },
            }"#,
        )
        .unwrap();

        let servers = mcp_servers_from_json(&path);
        let _ = fs::remove_file(path);

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "local");
        assert_eq!(servers[0].command.as_deref(), Some("npx"));
    }
}
