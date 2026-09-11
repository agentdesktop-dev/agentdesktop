use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};
use crate::provider::{claude_code::discovery as claude_code, context::ScanContext, metadata};

use super::ClaudeDesktop;

pub(super) fn discover() -> Option<Agent> {
    let context = ScanContext::capture();
    discover_with(&context)
}

fn discover_with(context: &ScanContext) -> Option<Agent> {
    let executable = context.find_executable(ClaudeDesktop::ID, executable_candidates(context))?;
    Some(Agent {
        version: discover_version(context, &executable),
        executable,
        kind: ClaudeDesktop::ID.to_owned(),
        mcp_servers: discover_mcp_servers(context),
        skills: Vec::new(),
    })
}

fn executable_candidates(context: &ScanContext) -> Vec<PathBuf> {
    let candidates = BTreeSet::new();
    #[cfg(target_os = "linux")]
    let _ = context;

    #[cfg(target_os = "macos")]
    let candidates = {
        let mut candidates = candidates;
        candidates.extend(context.system_path("/Applications/Claude.app/Contents/MacOS/Claude"));
        for home in context.homes() {
            candidates.insert(home.join("Applications/Claude.app/Contents/MacOS/Claude"));
        }
        candidates
    };

    #[cfg(windows)]
    let candidates = {
        let mut candidates = candidates;
        for home in context.homes() {
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
            context.env_path("ProgramFiles"),
            context.env_path("ProgramFiles(x86)"),
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

fn discover_version(context: &ScanContext, executable: &Path) -> Option<String> {
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
    archives.extend(
        [
            "/usr/lib/claude-desktop/resources/app.asar",
            "/usr/lib/claude-desktop-bin/resources/app.asar",
            "/opt/Claude/resources/app.asar",
            "/opt/claude-desktop/resources/app.asar",
        ]
        .into_iter()
        .filter_map(|path| context.system_path(path)),
    );
    archives
        .into_iter()
        .find_map(|archive| metadata::electron_asar_version(&archive, "Claude"))
}

fn discover_mcp_servers(context: &ScanContext) -> Vec<McpServer> {
    let mut paths = BTreeSet::new();
    for home in context.homes() {
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
