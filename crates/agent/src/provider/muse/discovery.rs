use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};
use serde_json::Value;

use crate::provider::metadata;

use super::Muse;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_executable("muse", executable_candidates())?;
    Some(Agent {
        version: discover_version(&executable),
        executable,
        kind: Muse::ID.to_owned(),
        mcp_servers: discover_mcp_servers(),
        skills: metadata::discover_skills(skill_roots()),
    })
}

fn executable_candidates() -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    // The installer puts its launcher script, and the release it downloads,
    // in the user's local bin directory.
    for home in metadata::user_home_dirs() {
        candidates.insert(home.join(".local/bin/muse"));
    }
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/muse"),
        PathBuf::from("/usr/local/bin/muse"),
    ]);
    candidates.into_iter().collect()
}

/// Muse Code's configuration roots: `$XDG_CONFIG_HOME/muse` for the daemon's
/// own environment, and `~/.config/muse` for every user home.
fn config_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    roots.extend(
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(|config_home| PathBuf::from(config_home).join("muse")),
    );
    roots.extend(
        metadata::user_home_dirs()
            .into_iter()
            .map(|home| home.join(".config/muse")),
    );
    roots.into_iter().collect()
}

/// The launcher records the active release in `.muse-version` next to
/// itself and runs `muse-bin-<version>` from the same directory, so the
/// version is read without executing anything.
fn discover_version(executable: &Path) -> Option<String> {
    let mut candidates = BTreeSet::new();
    for executable in [
        Some(executable.to_path_buf()),
        executable.canonicalize().ok(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(directory) = executable.parent() {
            candidates.insert(directory.join(".muse-version"));
        }
    }
    candidates
        .into_iter()
        .find_map(|path| version_from_state(&fs::read_to_string(path).ok()?))
}

/// `1.4.23-R8977.1` (release, then a build revision) reports `1.4.23`, the
/// version `muse --version` leads with.
fn version_from_state(contents: &str) -> Option<String> {
    let state = contents.trim();
    let version = state.split_once("-R").map_or(state, |(version, _)| version);
    let numeric = !version.is_empty()
        && version
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()));
    numeric.then(|| version.to_owned())
}

fn discover_mcp_servers() -> Vec<McpServer> {
    mcp_config_paths()
        .into_iter()
        .flat_map(|path| mcp_servers_from_json(&path))
        .collect()
}

/// User servers live under `mcpServers` in `settings.json`; project servers
/// in the repository root's `.mcp.json`.
fn mcp_config_paths() -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for root in config_roots() {
        paths.insert(root.join("settings.json"));
    }
    paths.extend(metadata::current_dir_ancestors(Path::new(".mcp.json")));
    paths.into_iter().collect()
}

fn skill_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for root in config_roots() {
        roots.insert(root.join("skills"));
    }
    // Muse Code also loads the user skills of other agents.
    for home in metadata::user_home_dirs() {
        roots.insert(home.join(".agents/skills"));
        roots.insert(home.join(".claude/skills"));
        roots.insert(home.join(".codex/skills"));
    }
    roots.extend(metadata::current_dir_ancestors(Path::new(".agents/skills")));
    roots.extend(metadata::current_dir_ancestors(Path::new(".claude/skills")));
    roots.into_iter().collect()
}

fn mcp_servers_from_json(path: &Path) -> Vec<McpServer> {
    let Ok(contents) = fs::read(path) else {
        return Vec::new();
    };
    let Ok(document) = serde_json::from_slice::<Value>(&contents) else {
        return Vec::new();
    };
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
                enabled: server
                    .get("enabled")
                    .and_then(Value::as_bool)
                    .unwrap_or(true),
                source: path.to_path_buf(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{discover_version, mcp_servers_from_json, version_from_state};

    #[test]
    fn reads_user_servers_without_secrets() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("settings.json");
        fs::write(
            &path,
            r#"{
  "schema_version": 1,
  "mcpServers": {
    "docs": {
      "type": "streamable-http",
      "url": "https://example.com/mcp",
      "headers": {"Authorization": "Bearer secret"}
    },
    "local": {
      "type": "stdio",
      "command": "npx",
      "args": ["-y", "secret-package"],
      "env": {"TOKEN": "secret"}
    },
    "off": {"command": "uvx", "enabled": false},
    "untyped": {"url": "https://example.com/other"},
    "broken": {"args": []}
  }
}"#,
        )
        .unwrap();

        let servers = mcp_servers_from_json(&path);
        assert_eq!(servers.len(), 4);
        assert_eq!(servers[0].name, "docs");
        assert_eq!(servers[0].transport, "http");
        assert_eq!(servers[0].url.as_deref(), Some("https://example.com/mcp"));
        assert!(servers[0].enabled);
        assert_eq!(servers[1].name, "local");
        assert_eq!(servers[1].transport, "stdio");
        assert_eq!(servers[1].command.as_deref(), Some("npx"));
        assert_eq!(servers[2].name, "off");
        assert_eq!(servers[2].transport, "stdio");
        assert!(!servers[2].enabled);
        assert_eq!(servers[3].name, "untyped");
        assert_eq!(servers[3].transport, "http");
        assert!(
            !serde_json::to_string(&servers).unwrap().contains("secret"),
            "arguments, environment and headers are not inventoried"
        );
    }

    #[test]
    fn ignores_missing_and_malformed_files() {
        let root = tempfile::tempdir().unwrap();
        assert!(mcp_servers_from_json(&root.path().join("absent.json")).is_empty());
        let path = root.path().join("settings.json");
        fs::write(&path, "{ not json").unwrap();
        assert!(mcp_servers_from_json(&path).is_empty());
        fs::write(&path, r#"{"schema_version":1}"#).unwrap();
        assert!(mcp_servers_from_json(&path).is_empty());
    }

    #[test]
    fn reads_version_from_launcher_state() {
        assert_eq!(
            version_from_state("1.4.23-R8977.1\n").as_deref(),
            Some("1.4.23")
        );
        assert_eq!(version_from_state("1.5.0-R9001").as_deref(), Some("1.5.0"));
        assert_eq!(version_from_state("2.0.1").as_deref(), Some("2.0.1"));
        assert_eq!(version_from_state(""), None);
        assert_eq!(version_from_state("-R8977"), None);
        assert_eq!(version_from_state("not a version"), None);
    }

    #[test]
    fn finds_the_version_next_to_the_launcher() {
        let root = tempfile::tempdir().unwrap();
        let launcher = root.path().join("muse");
        fs::write(&launcher, "#!/usr/bin/env bash\n").unwrap();
        assert_eq!(discover_version(&launcher), None);
        fs::write(root.path().join(".muse-version"), "1.4.23-R8977.1\n").unwrap();
        assert_eq!(discover_version(&launcher).as_deref(), Some("1.4.23"));
    }
}
