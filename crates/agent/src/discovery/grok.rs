use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};
use serde::Deserialize;

use super::metadata;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_executable("grok", executable_candidates())?;
    Some(Agent {
        version: discover_version(&executable),
        executable,
        kind: "grok".to_owned(),
        mcp_servers: discover_mcp_servers(),
        skills: metadata::discover_skills(skill_roots()),
    })
}

fn executable_candidates() -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    if let Some(home) = grok_home_from_env() {
        candidates.insert(home.join("bin/grok"));
        #[cfg(windows)]
        candidates.insert(home.join("bin/grok.exe"));
    }
    for home in metadata::user_home_dirs() {
        candidates.insert(home.join(".grok/bin/grok"));
        #[cfg(windows)]
        {
            candidates.insert(home.join(".grok/bin/grok.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/grok"),
        PathBuf::from("/usr/local/bin/grok"),
    ]);
    candidates.into_iter().collect()
}

fn grok_home_from_env() -> Option<PathBuf> {
    std::env::var_os("GROK_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn grok_homes() -> Vec<PathBuf> {
    let mut homes = BTreeSet::new();
    homes.extend(grok_home_from_env());
    homes.extend(
        metadata::user_home_dirs()
            .into_iter()
            .map(|home| home.join(".grok")),
    );
    homes.into_iter().collect()
}

fn discover_version(executable: &Path) -> Option<String> {
    version_candidates(executable)
        .into_iter()
        .find_map(|path| version_from_json(&path))
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
            candidates.insert(directory.join("version.json"));
            candidates.insert(directory.join("../version.json"));
        }
    }
    for home in grok_homes() {
        candidates.insert(home.join("version.json"));
    }
    candidates.into_iter().collect()
}

fn version_from_json(path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct VersionFile {
        version: String,
    }

    let contents = fs::read(path).ok()?;
    let metadata: VersionFile = serde_json::from_slice(&contents).ok()?;
    (!metadata.version.is_empty()).then_some(metadata.version)
}

fn discover_mcp_servers() -> Vec<McpServer> {
    config_paths()
        .into_iter()
        .flat_map(|path| mcp_servers_from_toml(&path))
        .collect()
}

fn config_paths() -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    #[cfg(unix)]
    {
        paths.insert(PathBuf::from("/etc/grok/config.toml"));
        paths.insert(PathBuf::from("/etc/grok/managed_config.toml"));
    }
    for home in grok_homes() {
        paths.insert(home.join("config.toml"));
        paths.insert(home.join("managed_config.toml"));
    }
    paths.extend(metadata::current_dir_ancestors(Path::new(
        ".grok/config.toml",
    )));
    paths.into_iter().collect()
}

fn skill_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for home in grok_homes() {
        roots.insert(home.join("skills"));
    }
    for home in metadata::user_home_dirs() {
        roots.insert(home.join(".agents/skills"));
    }
    roots.extend(metadata::current_dir_ancestors(Path::new(".grok/skills")));
    roots.extend(metadata::current_dir_ancestors(Path::new(".agents/skills")));
    roots.into_iter().collect()
}

fn mcp_servers_from_toml(path: &Path) -> Vec<McpServer> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(document) = toml::from_str::<toml::Value>(&contents) else {
        return Vec::new();
    };
    mcp_servers_from_value(&document, path)
}

fn mcp_servers_from_value(document: &toml::Value, source: &Path) -> Vec<McpServer> {
    let disabled = document
        .get("disabled_mcp_servers")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .collect::<BTreeSet<_>>();
    let Some(servers) = document.get("mcp_servers").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(name, value)| {
            let server = value.as_table()?;
            let command = server
                .get("command")
                .and_then(toml::Value::as_str)
                .map(str::to_owned);
            let url = server
                .get("url")
                .and_then(toml::Value::as_str)
                .map(str::to_owned);
            let transport = server
                .get("type")
                .and_then(toml::Value::as_str)
                .map(|transport| match transport {
                    "streamable-http" => "http",
                    other => other,
                })
                .or_else(|| url.as_ref().map(|_| "http"))
                .or_else(|| command.as_ref().map(|_| "stdio"))?;
            let enabled = server
                .get("enabled")
                .and_then(toml::Value::as_bool)
                .unwrap_or(true)
                && !disabled.contains(name.as_str());
            Some(McpServer {
                name: name.clone(),
                transport: transport.to_owned(),
                command,
                url,
                enabled,
                source: source.to_path_buf(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::{mcp_servers_from_toml, version_from_json};

    #[test]
    fn reads_stdio_http_and_sse_servers_without_secrets() {
        let path = temporary("grok-mcp.toml");
        fs::write(
            &path,
            r#"
disabled_mcp_servers = ["skipped"]

[mcp_servers.docs]
url = "https://example.com/mcp"
headers = { Authorization = "secret" }
enabled = false

[mcp_servers.local]
command = "npx"
args = ["-y", "secret-package"]
[mcp_servers.local.env]
TOKEN = "secret"

[mcp_servers.events]
type = "sse"
url = "https://example.com/events"

[mcp_servers.gateway]
type = "streamable-http"
url = "https://example.com/mcp"

[mcp_servers.skipped]
command = "npx"
"#,
        )
        .unwrap();
        let servers = mcp_servers_from_toml(&path);
        let _ = fs::remove_file(&path);

        assert_eq!(servers.len(), 5);
        assert_eq!(servers[0].name, "docs");
        assert_eq!(servers[0].transport, "http");
        assert_eq!(servers[0].url.as_deref(), Some("https://example.com/mcp"));
        assert!(!servers[0].enabled);
        assert_eq!(servers[1].name, "events");
        assert_eq!(servers[1].transport, "sse");
        assert_eq!(servers[2].transport, "http");
        assert_eq!(servers[3].command.as_deref(), Some("npx"));
        assert!(servers[3].enabled);
        assert_eq!(servers[4].name, "skipped");
        assert!(!servers[4].enabled);
    }

    #[test]
    fn reads_version_from_install_metadata() {
        let path = temporary("grok-version.json");
        fs::write(
            &path,
            r#"{"version":"1.0.13","stable_version":"1.0.13","checked_at":"2026-09-04T01:41:44.871077Z"}"#,
        )
        .unwrap();

        assert_eq!(version_from_json(&path).as_deref(), Some("1.0.13"));

        let _ = fs::remove_file(&path);
    }

    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agentdesktop-{}-{name}", std::process::id()))
    }
}
