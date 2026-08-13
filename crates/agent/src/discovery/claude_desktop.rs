use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};

use super::{claude_code, metadata};

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_executable("claude-desktop", executable_candidates())?;
    Some(Agent {
        version: discover_version(&executable),
        executable,
        kind: "claude-desktop".to_owned(),
        mcp_servers: discover_mcp_servers(),
        skills: Vec::new(),
    })
}

fn executable_candidates() -> Vec<PathBuf> {
    let candidates = BTreeSet::new();

    #[cfg(target_os = "macos")]
    let candidates = {
        let mut candidates = candidates;
        candidates.insert(PathBuf::from(
            "/Applications/Claude.app/Contents/MacOS/Claude",
        ));
        for home in metadata::user_home_dirs() {
            candidates.insert(home.join("Applications/Claude.app/Contents/MacOS/Claude"));
        }
        candidates
    };

    #[cfg(windows)]
    let candidates = {
        let mut candidates = candidates;
        for home in metadata::user_home_dirs() {
            let local = home.join("AppData/Local");
            candidates.insert(local.join("AnthropicClaude/claude.exe"));
            candidates.insert(local.join("Programs/Claude/Claude.exe"));
            let install_root = local.join("AnthropicClaude");
            if let Ok(entries) = std::fs::read_dir(install_root) {
                candidates.extend(
                    entries
                        .flatten()
                        .map(|entry| entry.path().join("claude.exe")),
                );
            }
        }
        for root in [
            metadata::env_path("ProgramFiles"),
            metadata::env_path("ProgramFiles(x86)"),
        ]
        .into_iter()
        .flatten()
        {
            candidates.insert(root.join("Claude/Claude.exe"));
        }
        candidates
    };

    candidates.into_iter().collect()
}

fn discover_version(executable: &Path) -> Option<String> {
    let mut archives = BTreeSet::new();
    if let Some(directory) = executable.parent() {
        archives.insert(directory.join("resources/app.asar"));
        archives.insert(directory.join("../Resources/app.asar"));
    }
    if let Ok(executable) = executable.canonicalize()
        && let Some(directory) = executable.parent()
    {
        archives.insert(directory.join("resources/app.asar"));
        archives.insert(directory.join("../Resources/app.asar"));
    }
    archives.extend([
        "/usr/lib/claude-desktop/resources/app.asar".into(),
        "/usr/lib/claude-desktop-bin/resources/app.asar".into(),
        "/opt/Claude/resources/app.asar".into(),
        "/opt/claude-desktop/resources/app.asar".into(),
    ]);
    archives
        .into_iter()
        .find_map(|archive| metadata::electron_asar_version(&archive, "Claude"))
}

fn discover_mcp_servers() -> Vec<McpServer> {
    let mut paths = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        paths.insert(home.join(".config/Claude/claude_desktop_config.json"));
        paths.insert(home.join(".config/Claude-3p/claude_desktop_config.json"));
        paths.insert(home.join("Library/Application Support/Claude/claude_desktop_config.json"));
        paths.insert(home.join("AppData/Roaming/Claude/claude_desktop_config.json"));
    }
    paths
        .into_iter()
        .flat_map(|path| claude_code::mcp_servers_from_json(&path))
        .collect()
}
