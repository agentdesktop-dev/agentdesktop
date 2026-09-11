use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};
use serde_json::Value;

use super::{context::ScanContext, files, mcp, metadata};

pub(super) fn discover(context: &ScanContext) -> Option<Agent> {
    let executable = context.find_executable("claude", executable_candidates(context))?;
    Some(Agent {
        version: metadata::version_after_component(&executable, "versions"),
        executable,
        kind: "claude-code".to_owned(),
        mcp_servers: discover_mcp_servers(context),
        skills: metadata::discover_skills(skill_roots(context)),
    })
}

fn executable_candidates(context: &ScanContext) -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    for home in context.homes() {
        candidates.insert(home.join(".local/bin/claude"));
        candidates.insert(home.join(".npm-global/bin/claude"));
        #[cfg(windows)]
        {
            candidates.insert(home.join(".local/bin/claude.exe"));
            candidates.insert(home.join("AppData/Roaming/npm/claude.cmd"));
        }
    }
    #[cfg(target_os = "macos")]
    candidates.extend(
        ["/opt/homebrew/bin/claude", "/usr/local/bin/claude"]
            .into_iter()
            .filter_map(|path| context.system_path(path)),
    );
    candidates.into_iter().collect()
}

fn discover_mcp_servers(context: &ScanContext) -> Vec<McpServer> {
    let mut servers = Vec::new();
    if let Some(managed) = managed_root(context).map(|root| root.join("managed-mcp.json")) {
        servers.extend(mcp::from_mcp_servers_file(&managed, mcp_enabled));
    }

    for home in context.homes() {
        let user = home.join(".claude.json");
        servers.extend(mcp::from_mcp_servers_file(&user, mcp_enabled));
    }
    servers
}

fn mcp_enabled(entry: &serde_json::Map<String, Value>) -> bool {
    entry.get("disabled").and_then(Value::as_bool) != Some(true)
}

fn skill_roots(context: &ScanContext) -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    roots.extend(managed_root(context).map(|root| root.join("skills")));
    roots.extend(context.current_dir_ancestors(Path::new(".claude/skills")));
    for home in context.homes() {
        roots.insert(home.join(".claude/skills"));
        for root in installed_plugin_roots(home) {
            roots.insert(root);
        }
    }
    roots.into_iter().collect()
}

fn managed_root(context: &ScanContext) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    return context.system_path("/etc/claude-code");
    #[cfg(target_os = "macos")]
    return context.system_path("/Library/Application Support/ClaudeCode");
    #[cfg(windows)]
    return context
        .env_path("ProgramFiles")
        .map(|path| path.join("ClaudeCode"));
}

fn installed_plugin_roots(home: &Path) -> Vec<PathBuf> {
    let path = home.join(".claude/plugins/installed_plugins.json");
    let Some(document) = files::read_json::<Value>(&path) else {
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
