use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};

use crate::provider::{metadata, vscode::discovery as vscode};

use super::Pi;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_all_in_path("pi")
        .into_iter()
        .chain(
            executable_candidates()
                .into_iter()
                .filter(|candidate| candidate.is_file()),
        )
        .find(|candidate| is_pi(candidate))?;
    Some(Agent {
        version: package_version(&executable),
        executable,
        kind: Pi::ID.to_owned(),
        mcp_servers: discover_mcp_servers(),
        skills: metadata::discover_skills(skill_roots()),
    })
}

fn executable_candidates() -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        candidates.insert(home.join(".local/bin/pi"));
        candidates.insert(home.join(".npm-global/bin/pi"));
        #[cfg(windows)]
        {
            candidates.insert(home.join(".local/bin/pi.exe"));
            candidates.insert(home.join("AppData/Roaming/npm/pi.cmd"));
            candidates.insert(home.join("AppData/Roaming/npm/pi.exe"));
            candidates.insert(home.join("AppData/Local/pnpm/pi.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/pi"),
        PathBuf::from("/usr/local/bin/pi"),
    ]);
    #[cfg(target_os = "linux")]
    candidates.extend([
        PathBuf::from("/usr/bin/pi"),
        PathBuf::from("/usr/local/bin/pi"),
    ]);
    candidates.into_iter().collect()
}

fn is_pi(executable: &Path) -> bool {
    package_manifest(executable).is_some()
}

fn package_version(executable: &Path) -> Option<String> {
    let manifest = package_manifest(executable)?;
    metadata::json_package_version(&manifest, Pi::PACKAGE_NAME)
}

fn package_manifest(executable: &Path) -> Option<PathBuf> {
    let mut seen = BTreeSet::new();
    for start in [
        Some(executable.to_path_buf()),
        executable.canonicalize().ok(),
    ]
    .into_iter()
    .flatten()
    {
        let mut directory = start.parent()?.to_path_buf();
        for _ in 0..8 {
            let manifest = directory.join("package.json");
            if seen.insert(manifest.clone())
                && metadata::json_package_version(&manifest, Pi::PACKAGE_NAME).is_some()
            {
                return Some(manifest);
            }
            if !directory.pop() {
                break;
            }
        }
    }
    None
}

fn discover_mcp_servers() -> Vec<McpServer> {
    mcp_config_paths()
        .into_iter()
        .flat_map(|path| vscode::mcp_servers_from_json(&path))
        .collect()
}

/// Files `pi-mcp-adapter` loads as normal MCP config.
///
/// Host-specific Cursor/Claude/Codex files are not included: the adapter only
/// imports those after `/mcp setup` writes them into a Pi-owned file.
fn mcp_config_paths() -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        let agent_dir = std::env::var_os("PI_CODING_AGENT_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".pi/agent"));
        paths.insert(agent_dir.join("mcp.json"));
        paths.insert(home.join(".config/mcp/mcp.json"));
        paths.insert(home.join(".agents/mcp.json"));
        paths.insert(home.join(".agents/mcp/mcp.json"));
    }
    paths.extend(metadata::current_dir_ancestors(Path::new(".mcp.json")));
    paths.extend(metadata::current_dir_ancestors(Path::new(".pi/mcp.json")));
    paths.into_iter().collect()
}

fn skill_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        let agent_dir = std::env::var_os("PI_CODING_AGENT_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".pi/agent"));
        roots.insert(agent_dir.join("skills"));
        roots.insert(home.join(".agents/skills"));
    }
    roots.extend(metadata::current_dir_ancestors(Path::new(".pi/skills")));
    roots.extend(metadata::current_dir_ancestors(Path::new(".agents/skills")));
    roots.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use serde_json::json;

    use super::{is_pi, mcp_config_paths, package_version};
    use crate::provider::vscode::discovery::mcp_servers_from_value;

    #[test]
    fn accepts_earendil_pi_package_and_reads_version() {
        let root = temporary("pi-package");
        let package = root.join("node_modules/@earendil-works/pi-coding-agent");
        let bundle = package.join("dist/bundle");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name":"@earendil-works/pi-coding-agent","version":"0.85.1"}"#,
        )
        .unwrap();
        let executable = bundle.join("cli.js");
        fs::write(&executable, "#!/usr/bin/env node\n").unwrap();

        assert!(is_pi(&executable));
        assert_eq!(package_version(&executable).as_deref(), Some("0.85.1"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_an_unrelated_pi_binary() {
        let root = temporary("pi-unrelated");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"name":"raspberry-pi-tools","version":"1.0.0"}"#,
        )
        .unwrap();
        let executable = root.join("pi");
        fs::write(&executable, "#!/bin/sh\n").unwrap();

        assert!(!is_pi(&executable));
        assert_eq!(package_version(&executable), None);

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reads_adapter_servers_without_secrets_or_arguments() {
        let servers = mcp_servers_from_value(
            &json!({
                "mcpServers": {
                    "github": {
                        "url": "https://api.githubcopilot.com/mcp",
                        "headers": { "Authorization": "secret" }
                    },
                    "chrome": {
                        "command": "npx",
                        "args": ["-y", "chrome-devtools-mcp@1.6.0"],
                        "env": { "TOKEN": "secret" }
                    },
                    "skipped": {
                        "command": "npx",
                        "disabled": true
                    }
                }
            }),
            Path::new("/home/tester/.pi/agent/mcp.json"),
        );

        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0].name, "chrome");
        assert_eq!(servers[0].transport, "stdio");
        assert_eq!(servers[0].command.as_deref(), Some("npx"));
        assert!(servers[0].enabled);
        assert_eq!(servers[1].name, "github");
        assert_eq!(servers[1].transport, "http");
        assert_eq!(
            servers[1].url.as_deref(),
            Some("https://api.githubcopilot.com/mcp")
        );
        assert!(!servers[2].enabled);
        assert!(
            servers
                .iter()
                .all(|server| server.source.ends_with("mcp.json"))
        );
    }

    #[test]
    fn scans_pi_owned_and_shared_mcp_files() {
        let paths = mcp_config_paths();
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with(std::path::Path::new(".pi/agent/mcp.json")))
        );
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with(std::path::Path::new(".config/mcp/mcp.json")))
        );
        assert!(paths.iter().any(|path| path.ends_with(".mcp.json")));
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with(std::path::Path::new(".pi/mcp.json")))
        );
    }

    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agentdesktop-{name}-{}", std::process::id()))
    }
}
