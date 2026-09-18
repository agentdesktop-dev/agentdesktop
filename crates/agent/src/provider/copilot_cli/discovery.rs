use std::{
    collections::{BTreeSet, VecDeque},
    env, fs,
    io::Read,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer, Skill};
use semver::Version;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use super::CopilotCli;
use crate::provider::metadata;

const MAX_METADATA_BYTES: u64 = 1024 * 1024;

/// Explicit scan inputs keep tests independent of the user's HOME and PATH.
/// The optional boundary applies to symlinks and package probes, not just homes.
struct ScanContext {
    executables: Vec<PathBuf>,
    config_roots: Vec<PathBuf>,
    boundary: Option<PathBuf>,
}

impl ScanContext {
    fn capture() -> Self {
        let home = metadata::home_dir();
        let homes = metadata::user_home_dirs();
        let copilot_home = env::var_os("COPILOT_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let mut executables = path_executables();
        for home in &homes {
            executables.extend([
                home.join(".local/bin/copilot"),
                home.join(".npm-global/bin/copilot"),
            ]);
            #[cfg(windows)]
            executables.extend([
                home.join(".local/bin/copilot.exe"),
                home.join("AppData/Roaming/npm/copilot.cmd"),
            ]);
        }
        Self {
            executables,
            config_roots: config_roots(home.as_deref(), homes, copilot_home),
            boundary: None,
        }
    }
}

fn config_roots(
    current_home: Option<&Path>,
    homes: Vec<PathBuf>,
    copilot_home: Option<PathBuf>,
) -> Vec<PathBuf> {
    let overridden = copilot_home.is_some();
    let mut roots = BTreeSet::new();
    roots.extend(copilot_home);
    for home in homes {
        if !overridden || Some(home.as_path()) != current_home {
            roots.insert(home.join(".copilot"));
        }
    }
    roots.into_iter().collect()
}

pub(super) fn discover() -> Option<Agent> {
    discover_in(&ScanContext::capture())
}

fn discover_in(context: &ScanContext) -> Option<Agent> {
    let executable = context
        .executables
        .iter()
        .find(|candidate| {
            is_executable(candidate) && confined(candidate, context.boundary.as_deref()).is_some()
        })?
        .clone();
    let version = package_version(&executable, context.boundary.as_deref())
        .map(|version| version.to_string());
    let mut mcp_servers = Vec::new();
    let mut skills = Vec::new();
    for root in &context.config_roots {
        let Some(root) = confined(root, context.boundary.as_deref()) else {
            continue;
        };
        // Never open config.json (login/auth), providers.json, sessions, or logs.
        mcp_servers.extend(mcp_servers_from_file(&root.join("mcp-config.json"), &root));
        skills.extend(discover_skills(&root.join("skills"), &root));
    }
    Some(Agent {
        kind: CopilotCli::ID.to_owned(),
        executable,
        version,
        mcp_servers,
        skills,
    })
}

/// The explicit launcher uses the current user's PATH, never another user's install.
pub(super) fn find_launcher() -> Option<PathBuf> {
    path_executables()
        .into_iter()
        .find(|candidate| is_executable(candidate))
        .and_then(|candidate| std::path::absolute(candidate).ok())
}

/// Resolve the official npm shim to its package-declared JavaScript entrypoint.
/// Executing Node directly avoids cmd.exe re-expanding literal native arguments.
#[cfg(any(windows, test))]
pub(super) fn npm_entrypoint(shim: &Path) -> Option<PathBuf> {
    let root = shim
        .parent()?
        .join("node_modules/@github/copilot")
        .canonicalize()
        .ok()?;
    let manifest: Value = serde_json::from_str(&read_metadata(&root.join("package.json"))?).ok()?;
    if manifest.get("name")?.as_str()? != "@github/copilot" {
        return None;
    }
    let bin = manifest.get("bin")?;
    let relative = bin.as_str().or_else(|| bin.get("copilot")?.as_str())?;
    if Path::new(relative).is_absolute() {
        return None;
    }
    let script = confined(&root.join(relative), Some(&root))?;
    let extension = script.extension()?.to_str()?;
    (script.is_file() && matches!(extension, "js" | "cjs" | "mjs")).then_some(script)
}

fn path_executables() -> Vec<PathBuf> {
    let Some(value) = env::var_os("PATH") else {
        return Vec::new();
    };
    #[cfg(windows)]
    let names = [
        "copilot.exe",
        "copilot.cmd",
        "copilot.bat",
        "copilot.com",
        "copilot",
    ];
    #[cfg(not(windows))]
    let names = ["copilot"];
    env::split_paths(&value)
        .flat_map(|directory| names.iter().map(move |name| directory.join(name)))
        .collect()
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        true
    }
}

/// Only known install-relative npm metadata is examined. Unlike generic ancestor
/// searches this cannot accidentally use an arbitrary parent project's version.
pub(super) fn package_version(executable: &Path, boundary: Option<&Path>) -> Option<Version> {
    #[derive(Deserialize)]
    struct Package {
        name: String,
        version: String,
    }

    let resolved = confined(executable, boundary)?;
    let mut candidates = BTreeSet::new();
    for executable in [executable, resolved.as_path()] {
        let directory = executable.parent()?;
        candidates.extend([
            directory.join("package.json"),
            directory.join("../package.json"),
            directory.join("node_modules/@github/copilot/package.json"),
            directory.join("../lib/node_modules/@github/copilot/package.json"),
        ]);
    }
    candidates.into_iter().find_map(|candidate| {
        let candidate = confined(&candidate, boundary)?;
        if candidate.file_name()? != "package.json" {
            return None;
        }
        let package: Package = serde_json::from_str(&read_metadata(&candidate)?).ok()?;
        if !matches!(
            package.name.as_str(),
            "@github/copilot"
                | "@github/copilot-darwin-arm64"
                | "@github/copilot-darwin-x64"
                | "@github/copilot-linux-arm64"
                | "@github/copilot-linux-x64"
                | "@github/copilot-win32-arm64"
                | "@github/copilot-win32-x64"
        ) {
            return None;
        }
        Version::parse(&package.version).ok()
    })
}

fn confined(path: &Path, boundary: Option<&Path>) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    if let Some(boundary) = boundary
        && !canonical.starts_with(boundary.canonicalize().ok()?)
    {
        return None;
    }
    Some(canonical)
}

fn read_metadata(path: &Path) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_METADATA_BYTES {
        return None;
    }
    let mut contents = String::new();
    fs::File::open(path)
        .ok()?
        .take(MAX_METADATA_BYTES + 1)
        .read_to_string(&mut contents)
        .ok()?;
    (contents.len() as u64 <= MAX_METADATA_BYTES).then_some(contents)
}

fn mcp_servers_from_file(path: &Path, root: &Path) -> Vec<McpServer> {
    // Reject file symlinks even within the root: mcp-config.json must not alias auth.
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Vec::new();
    }
    let Some(path) = confined(path, Some(root)) else {
        return Vec::new();
    };
    let Some(document) =
        read_metadata(&path).and_then(|contents| serde_json::from_str::<Value>(&contents).ok())
    else {
        return Vec::new();
    };
    let Some(servers) = document.get("mcpServers").and_then(Value::as_object) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(name, value)| {
            let command = value
                .get("command")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let endpoint = value.get("url").and_then(Value::as_str);
            let transport = match value.get("type").and_then(Value::as_str) {
                Some("local" | "stdio") => "stdio",
                Some("http" | "streamable-http") => "http",
                Some("sse") => "sse",
                _ if endpoint.is_some() => "http",
                _ if command.is_some() => "stdio",
                _ => return None,
            };
            // Origin-only inventory cannot expose userinfo, path tokens, query, or fragment.
            let url = endpoint
                .and_then(|endpoint| Url::parse(endpoint).ok())
                .filter(|url| matches!(url.scheme(), "https" | "http") && url.host().is_some())
                .map(|url| url.origin().ascii_serialization());
            Some(McpServer {
                name: name.clone(),
                transport: transport.to_owned(),
                command,
                url,
                enabled: value.get("disabled").and_then(Value::as_bool) != Some(true)
                    && value.get("enabled").and_then(Value::as_bool) != Some(false),
                source: path.clone(),
            })
        })
        .collect()
}

fn discover_skills(directory: &Path, root: &Path) -> Vec<Skill> {
    let Some(directory) = confined(directory, Some(root)) else {
        return Vec::new();
    };
    let mut pending = VecDeque::from([(directory.clone(), 0)]);
    let mut visited = BTreeSet::new();
    let mut skills = Vec::new();
    let mut budget: usize = 2048;
    while let Some((path, depth)) = pending.pop_front() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let Some(path) = confined(&path, Some(&directory)) else {
            continue;
        };
        if !visited.insert(path.clone()) {
            continue;
        }
        if path.is_dir() && depth < 16 {
            if let Ok(entries) = fs::read_dir(&path) {
                for entry in entries.flatten().take(budget.saturating_sub(pending.len())) {
                    pending.push_back((entry.path(), depth + 1));
                }
            }
        } else if path.file_name().is_some_and(|name| name == "SKILL.md")
            && let Some(front_matter) =
                read_metadata(&path).and_then(|contents| metadata::skill_front_matter(&contents))
        {
            skills.push(Skill { path, front_matter });
        }
    }
    skills.sort_by(|left, right| left.path.cmp(&right.path));
    skills
}

#[cfg(test)]
mod tests;
