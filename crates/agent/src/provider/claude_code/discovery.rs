use super::ClaudeCode;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};
use serde_json::Value;

use crate::provider::metadata;

const NPM_PACKAGE: &str = "@anthropic-ai/claude-code";

/// `settings_path` is the managed settings file the reconciler writes, so a
/// `daemon.claudeCode.config` override is inventoried from the same place.
pub(super) fn discover(settings_path: &Path) -> Option<Agent> {
    let executable = metadata::find_executable("claude", executable_candidates())?;
    Some(Agent {
        version: version(&executable),
        executable,
        kind: ClaudeCode::ID.to_owned(),
        mcp_servers: discover_mcp_servers(settings_path),
        skills: metadata::discover_skills(skill_roots()),
    })
}

/// The native installer keeps each build under `versions/<version>`; an npm
/// install has the version only in the `@anthropic-ai/claude-code` manifest.
///
/// An npm install resolves into `node_modules`. Under a Node version manager
/// that path has a `versions` component of its own, which names the Node
/// release, so the native layout is read only outside `node_modules`.
fn version(executable: &Path) -> Option<String> {
    let npm_install = executable.canonicalize().is_ok_and(|resolved| {
        resolved
            .components()
            .any(|component| component.as_os_str() == "node_modules")
    });
    if npm_install {
        return metadata::npm_package_version(executable, NPM_PACKAGE);
    }
    metadata::version_after_component(executable, "versions")
        .or_else(|| metadata::npm_package_version(executable, NPM_PACKAGE))
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

fn discover_mcp_servers(settings_path: &Path) -> Vec<McpServer> {
    let root = managed_root();
    let mut servers = Vec::new();
    if let Some(root) = &root {
        servers.extend(mcp_servers_from_json(&root.join("managed-mcp.json")));
    }
    // Servers pushed through the `managedMcpServers` managed setting — including the
    // ones Agentdesktop itself writes to its configured settings file.
    servers.extend(merged_managed_mcp_servers(&managed_settings_sources(
        root.as_deref(),
        settings_path,
    )));

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

/// The managed settings files under `root`, plus Agentdesktop's own settings file when
/// it has been configured outside that root. The configured file is merged last because
/// it is the one Agentdesktop reconciles.
fn managed_settings_sources(root: Option<&Path>, settings_path: &Path) -> Vec<PathBuf> {
    let mut files = root.map(managed_settings_files).unwrap_or_default();
    if !files.iter().any(|path| path == settings_path) {
        files.push(settings_path.to_path_buf());
    }
    files
}

/// Merges `managedMcpServers` across settings files by server name, with later files
/// overriding earlier ones, so each effective server is reported once.
fn merged_managed_mcp_servers(files: &[PathBuf]) -> Vec<McpServer> {
    let mut merged = BTreeMap::new();
    for path in files {
        for server in managed_mcp_servers_from_json(path) {
            merged.insert(server.name.clone(), server);
        }
    }
    merged.into_values().collect()
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
    use std::{fs, path::Path};

    use serde_json::json;

    use super::{
        managed_mcp_servers_from_value, managed_settings_sources, mcp_servers_from_value,
        merged_managed_mcp_servers, version,
    };

    #[test]
    fn version_comes_from_the_native_installer_layout() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("share/claude/versions/2.1.327");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, "").unwrap();

        assert_eq!(version(&executable).as_deref(), Some("2.1.327"));
    }

    #[test]
    fn version_comes_from_the_npm_manifest_when_installed_with_npm() {
        let prefix = tempfile::tempdir().unwrap();
        let package = prefix
            .path()
            .join("lib/node_modules/@anthropic-ai/claude-code");
        fs::create_dir_all(package.join("bin")).unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name":"@anthropic-ai/claude-code","version":"2.1.327"}"#,
        )
        .unwrap();
        fs::write(package.join("bin/claude.exe"), "").unwrap();
        fs::create_dir_all(prefix.path().join("bin")).unwrap();
        let launcher = prefix.path().join("bin/claude");
        #[cfg(unix)]
        std::os::unix::fs::symlink(package.join("bin/claude.exe"), &launcher).unwrap();
        #[cfg(not(unix))]
        fs::copy(package.join("bin/claude.exe"), &launcher).unwrap();

        assert_eq!(version(&launcher).as_deref(), Some("2.1.327"));
    }

    /// nvm and nodenv keep Node itself under `versions/<node release>`.
    #[cfg(unix)]
    #[test]
    fn version_ignores_the_node_release_of_a_node_version_manager() {
        let home = tempfile::tempdir().unwrap();
        for node in [".nvm/versions/node/v22.11.0", ".nodenv/versions/22.11.0"] {
            let prefix = home.path().join(node);
            let package = prefix.join("lib/node_modules/@anthropic-ai/claude-code");
            fs::create_dir_all(package.join("bin")).unwrap();
            fs::write(
                package.join("package.json"),
                r#"{"name":"@anthropic-ai/claude-code","version":"2.1.327"}"#,
            )
            .unwrap();
            fs::write(package.join("bin/claude.exe"), "").unwrap();
            fs::create_dir_all(prefix.join("bin")).unwrap();
            let launcher = prefix.join("bin/claude");
            std::os::unix::fs::symlink(package.join("bin/claude.exe"), &launcher).unwrap();

            assert_eq!(version(&launcher).as_deref(), Some("2.1.327"), "{node}");
        }
    }

    #[test]
    fn version_is_none_without_a_versioned_path_or_manifest() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("claude");
        fs::write(&executable, "").unwrap();

        assert_eq!(version(&executable), None);
    }

    fn write_managed_servers(path: &Path, servers: serde_json::Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            serde_json::to_vec(&json!({ "managedMcpServers": servers })).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn later_managed_settings_files_override_servers_by_name() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("managed-settings.json");
        let drop_in = root.path().join("managed-settings.d/50-agentdesktop.json");
        write_managed_servers(
            &base,
            json!({
                "github": { "type": "http", "url": "https://old.example.com/mcp" },
                "local": { "command": "server" }
            }),
        );
        write_managed_servers(
            &drop_in,
            json!({ "github": { "type": "http", "url": "https://new.example.com/mcp" } }),
        );

        let servers =
            merged_managed_mcp_servers(&managed_settings_sources(Some(root.path()), &drop_in));

        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "github");
        assert_eq!(
            servers[0].url.as_deref(),
            Some("https://new.example.com/mcp")
        );
        assert_eq!(servers[0].source, drop_in);
        assert_eq!(servers[1].name, "local");
        assert_eq!(servers[1].source, base);
    }

    #[test]
    fn configured_settings_path_outside_the_managed_root_is_inventoried_last() {
        let root = tempfile::tempdir().unwrap();
        let custom = tempfile::tempdir().unwrap();
        let drop_in = root.path().join("managed-settings.d/10-other.json");
        let configured = custom.path().join("agentdesktop.json");
        write_managed_servers(&drop_in, json!({ "github": { "command": "stale" } }));
        write_managed_servers(
            &configured,
            json!({ "github": { "type": "http", "url": "https://gateway.example.com/mcp" } }),
        );

        let sources = managed_settings_sources(Some(root.path()), &configured);
        assert_eq!(sources.last(), Some(&configured));
        assert_eq!(
            sources.iter().filter(|path| **path == configured).count(),
            1
        );

        let servers = merged_managed_mcp_servers(&sources);
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].transport, "http");
        assert_eq!(servers[0].source, configured);
    }

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
