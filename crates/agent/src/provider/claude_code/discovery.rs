use super::ClaudeCode;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};
use serde_json::Value;

use crate::provider::metadata;

/// `settings_path` is the managed settings file the reconciler writes, so a
/// `daemon.claudeCode.config` override is inventoried from the same place.
pub(super) fn discover(settings_path: &Path) -> Option<Agent> {
    let executable = metadata::find_executable(PATH_SEARCH_NAME, executable_candidates())?;
    Some(Agent {
        version: executable_version(&executable, managed_install_executable().as_deref()),
        executable,
        kind: ClaudeCode::ID.to_owned(),
        mcp_servers: discover_mcp_servers(settings_path),
        skills: metadata::discover_skills(skill_roots()),
    })
}

/// On Windows, discovery considers only `.exe` candidates: a bare-name PATH
/// search would find the managed package's own extensionless `claude`
/// launcher script (prepended to the machine PATH) before any real install,
/// and would report that script, with no version, as the Claude Code
/// executable. Searching the extension-qualified name keeps the PATH walk to
/// the real binary; the launcher scripts (extensionless sh and `.cmd`) are
/// never reported.
const PATH_SEARCH_NAME: &str = if cfg!(windows) {
    "claude.exe"
} else {
    "claude"
};

fn executable_candidates() -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        candidates.extend(home_candidates(&home, cfg!(windows)));
    }
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/claude"),
        PathBuf::from("/usr/local/bin/claude"),
    ]);
    with_managed_candidate(candidates, managed_install_executable())
}

/// The per-home candidates. On Windows only the `.exe` form is considered
/// (see [`PATH_SEARCH_NAME`]): script launchers such as npm's `claude.cmd`
/// are not reported as the executable.
fn home_candidates(home: &Path, windows: bool) -> Vec<PathBuf> {
    if windows {
        vec![home.join(".local/bin/claude.exe")]
    } else {
        vec![
            home.join(".local/bin/claude"),
            home.join(".npm-global/bin/claude"),
        ]
    }
}

/// The managed package's install path of the binary,
/// `%ProgramFiles%\Anthropic\ClaudeCode\claude.exe`. Distinct from
/// `managed_root()` (`%ProgramFiles%\ClaudeCode`), which holds managed
/// settings and is granted read-only to Claude boxes. `None` off Windows:
/// the managed package is Windows-only.
fn managed_install_executable() -> Option<PathBuf> {
    #[cfg(windows)]
    return metadata::env_path("ProgramFiles")
        .map(|path| path.join("Anthropic").join("ClaudeCode").join("claude.exe"));
    #[cfg(not(windows))]
    None
}

/// The managed install is tried AFTER every existing candidate, so a
/// pre-existing user copy keeps winning discovery unchanged. Appended to the
/// sorted set rather than inserted into it, because the managed path sorts
/// lexically before the user-profile candidates.
fn with_managed_candidate(candidates: BTreeSet<PathBuf>, managed: Option<PathBuf>) -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = candidates.into_iter().collect();
    candidates.extend(managed);
    candidates
}

/// When the found executable is the managed install, its version comes from
/// the `agentdesktop-package.json` the managed package's build writes beside
/// the binary — discovery never runs a tool, and the managed install has no
/// version-named path component to read. Every other install keeps the
/// path-derived version.
fn executable_version(executable: &Path, managed: Option<&Path>) -> Option<String> {
    if managed == Some(executable) {
        return metadata::json_package_version(
            &executable.with_file_name("agentdesktop-package.json"),
            "claude-code",
        );
    }
    metadata::version_after_component(executable, "versions")
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

pub(super) fn managed_root() -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    return Some(PathBuf::from("/etc/claude-code"));
    #[cfg(target_os = "macos")]
    return Some(PathBuf::from("/Library/Application Support/ClaudeCode"));
    #[cfg(windows)]
    return metadata::env_path("ProgramFiles").map(|path| path.join("ClaudeCode"));
}

/// `managed-settings.json` followed by the `managed-settings.d/*.json` drop-ins in the
/// lexical order Claude Code merges them.
pub(super) fn managed_settings_files(root: &Path) -> Vec<PathBuf> {
    let mut files = vec![root.join("managed-settings.json")];
    if let Ok(entries) = fs::read_dir(root.join("managed-settings.d")) {
        let mut drop_ins: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "json")
                    && path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| !name.starts_with('.'))
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
    use std::{
        collections::BTreeSet,
        fs,
        path::{Path, PathBuf},
    };

    use serde_json::json;

    use super::{
        PATH_SEARCH_NAME, executable_version, home_candidates, managed_mcp_servers_from_value,
        managed_settings_sources, mcp_servers_from_value, merged_managed_mcp_servers,
        with_managed_candidate,
    };
    use crate::provider::metadata;

    /// DV6-4 (adversarial/maintainer M2): discovery considers only `.exe`
    /// candidates on Windows. The PATH search is extension-qualified, so the
    /// managed package's extensionless `claude` launcher on the prepended
    /// machine PATH (and any `claude.cmd`) is never reported, and the home
    /// candidates carry no script paths.
    #[test]
    fn discovery_considers_only_exe_candidates_on_windows() {
        if cfg!(windows) {
            assert_eq!(PATH_SEARCH_NAME, "claude.exe");
        } else {
            assert_eq!(PATH_SEARCH_NAME, "claude");
        }
        let windows = home_candidates(Path::new(r"C:\Users\dev"), true);
        assert!(!windows.is_empty());
        for candidate in &windows {
            assert!(
                candidate.extension().is_some_and(|ext| ext == "exe"),
                "{candidate:?}"
            );
        }
        // The npm shim is a .cmd script: not a candidate on Windows.
        assert!(
            !windows
                .iter()
                .any(|candidate| { candidate.to_string_lossy().to_lowercase().contains("npm") })
        );
        // Off Windows the extensionless form is the executable itself.
        let unix = home_candidates(Path::new("/home/dev"), false);
        assert!(unix.iter().all(|candidate| candidate.extension().is_none()));
    }

    /// DV6-4: a directory holding the launcher scripts yields no discovery
    /// hit, because only the `.exe` form is a candidate; a real binary
    /// beside the scripts is found, and the scripts never are.
    #[test]
    fn a_launcher_directory_yields_no_hit_without_a_real_exe_beside_the_scripts() {
        let launchers = tempfile::tempdir().unwrap();
        fs::write(launchers.path().join("claude"), b"#!/bin/sh\n").unwrap();
        fs::write(launchers.path().join("claude.cmd"), b"@echo off\r\n").unwrap();
        let exe_candidate = launchers.path().join("claude.exe");
        // The probe name is on no PATH (as in the sibling tests), so only
        // the candidate list decides; the scripts exist but are no candidate.
        let found =
            metadata::find_executable("agentdesktop-test-no-such-tool", [exe_candidate.clone()]);
        assert_eq!(found, None);

        fs::write(&exe_candidate, b"not a program").unwrap();
        let found =
            metadata::find_executable("agentdesktop-test-no-such-tool", [exe_candidate.clone()]);
        assert_eq!(found, Some(exe_candidate));
    }

    /// D9: the managed install candidate comes AFTER every existing one, even
    /// though `C:\Program Files\...` sorts lexically before the user-profile
    /// paths a plain set insert would have put it ahead of.
    #[test]
    fn the_managed_install_candidate_comes_after_every_existing_candidate() {
        let existing: BTreeSet<PathBuf> = [
            PathBuf::from(r"C:\Users\dev\.local\bin\claude.exe"),
            PathBuf::from(r"C:\Users\dev\AppData\Roaming\npm\claude.cmd"),
        ]
        .into_iter()
        .collect();
        let managed = PathBuf::from(r"C:\Program Files\Anthropic\ClaudeCode\claude.exe");

        let candidates = with_managed_candidate(existing.clone(), Some(managed.clone()));

        assert_eq!(candidates.len(), existing.len() + 1);
        assert_eq!(candidates.last(), Some(&managed));
        assert!(
            candidates[..existing.len()]
                .iter()
                .all(|candidate| *candidate != managed)
        );

        let unchanged = with_managed_candidate(existing.clone(), None);
        assert_eq!(unchanged, existing.into_iter().collect::<Vec<_>>());
    }

    /// AC-2 (N9): with the managed install present and no match earlier,
    /// discovery reports the managed path; with a `~\.local\bin` copy also
    /// present it reports the existing first match, unchanged order.
    #[test]
    fn a_managed_install_is_found_only_when_no_existing_candidate_matches() {
        let home = tempfile::tempdir().unwrap();
        let install = tempfile::tempdir().unwrap();
        let local_bin = home.path().join(".local/bin/claude.exe");
        let managed = install.path().join("claude.exe");
        fs::create_dir_all(local_bin.parent().unwrap()).unwrap();
        fs::write(&managed, b"not a program").unwrap();
        let candidates: BTreeSet<PathBuf> = [local_bin.clone()].into_iter().collect();

        // The probe name is on no PATH, so resolution falls through to the
        // candidate list, like a Windows host without `claude.exe` on PATH.
        let found = metadata::find_executable(
            "agentdesktop-test-no-such-tool",
            with_managed_candidate(candidates.clone(), Some(managed.clone())),
        );
        assert_eq!(found, Some(managed.clone()));

        fs::write(&local_bin, b"not a program").unwrap();
        let found = metadata::find_executable(
            "agentdesktop-test-no-such-tool",
            with_managed_candidate(candidates, Some(managed)),
        );
        assert_eq!(found, Some(local_bin));
    }

    /// D9 (N9): the managed install's version comes from the
    /// `agentdesktop-package.json` beside it, never from running it — the
    /// file on disk is not a runnable program, so executing it instead of
    /// reading the manifest could not produce this version.
    #[test]
    fn a_managed_install_reports_the_manifest_version_without_running_the_binary() {
        let install = tempfile::tempdir().unwrap();
        let executable = install.path().join("claude.exe");
        fs::write(&executable, b"not a program").unwrap();
        fs::write(
            install.path().join("agentdesktop-package.json"),
            br#"{ "name": "claude-code", "version": "2.1.98" }"#,
        )
        .unwrap();

        assert_eq!(
            executable_version(&executable, Some(&executable)),
            Some("2.1.98".to_owned())
        );
    }

    /// D9: a managed install whose manifest is missing or names another
    /// package has no version; nothing falls back to executing the binary.
    #[test]
    fn a_managed_install_without_its_manifest_has_no_version() {
        let install = tempfile::tempdir().unwrap();
        let executable = install.path().join("claude.exe");
        fs::write(&executable, b"not a program").unwrap();
        assert_eq!(executable_version(&executable, Some(&executable)), None);

        fs::write(
            install.path().join("agentdesktop-package.json"),
            br#"{ "name": "other-tool", "version": "1.0.0" }"#,
        )
        .unwrap();
        assert_eq!(executable_version(&executable, Some(&executable)), None);
    }

    /// D9: every non-managed install keeps the existing path-derived version.
    #[test]
    fn a_non_managed_executable_keeps_the_path_derived_version() {
        let root = tempfile::tempdir().unwrap();
        let versions = root.path().join("versions/2.0.14");
        fs::create_dir_all(&versions).unwrap();
        let executable = versions.join("claude");
        fs::write(&executable, b"not a program").unwrap();

        let managed = PathBuf::from(r"C:\Program Files\Anthropic\ClaudeCode\claude.exe");
        assert_eq!(
            executable_version(&executable, Some(&managed)),
            Some("2.0.14".to_owned())
        );
        assert_eq!(
            executable_version(&executable, None),
            Some("2.0.14".to_owned())
        );
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

    #[test]
    fn hidden_dropin_managed_mcp_servers_are_not_inventoried() {
        let root = tempfile::tempdir().unwrap();
        let drop_in = root.path().join("managed-settings.d/50-agentdesktop.json");
        let hidden_drop_in = root.path().join("managed-settings.d/.50-agentdesktop.json");

        write_managed_servers(&drop_in, json!({ "visible": { "command": "npx" } }));
        write_managed_servers(
            &hidden_drop_in,
            json!({ "hidden": { "command": "secret" } }),
        );

        let servers =
            merged_managed_mcp_servers(&managed_settings_sources(Some(root.path()), &drop_in));

        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "visible");
        // Hidden drop-in servers should not be in the inventory
        assert!(!servers.iter().any(|s| s.name == "hidden"));
    }
}
