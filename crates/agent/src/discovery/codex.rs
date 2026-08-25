use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};

use super::metadata;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_executable("codex", executable_candidates())?;
    let version = standalone_version(&executable).or_else(|| npm_version(&executable));
    Some(Agent {
        version,
        executable,
        kind: "codex".to_owned(),
        mcp_servers: discover_mcp_servers(),
        skills: metadata::discover_skills(skill_roots()),
    })
}

fn standalone_version(executable: &Path) -> Option<String> {
    metadata::version_after_component(executable, "releases").and_then(|release| {
        let target_marker = format!("-{}-", std::env::consts::ARCH);
        release
            .split_once(&target_marker)
            .map(|(version, _)| version.to_owned())
    })
}

fn npm_version(executable: &Path) -> Option<String> {
    let mut candidates = BTreeSet::new();
    for executable in [
        Some(executable.to_path_buf()),
        executable.canonicalize().ok(),
    ]
    .into_iter()
    .flatten()
    {
        for directory in executable.parent()?.ancestors().take(4) {
            candidates.insert(directory.join("package.json"));
            candidates.insert(directory.join("node_modules/@openai/codex/package.json"));
        }
    }
    candidates
        .into_iter()
        .find_map(|path| metadata::json_package_version(&path, "@openai/codex"))
}

fn executable_candidates() -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        candidates.insert(home.join(".local/bin/codex"));
        candidates.insert(home.join(".npm-global/bin/codex"));
        #[cfg(windows)]
        {
            candidates.insert(home.join(".local/bin/codex.exe"));
            candidates.insert(home.join("AppData/Roaming/npm/codex.cmd"));
        }
    }
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
    ]);
    candidates.into_iter().collect()
}

fn discover_mcp_servers() -> Vec<McpServer> {
    config_paths()
        .into_iter()
        .flat_map(|path| mcp_servers_from_toml(&path))
        .collect()
}

fn config_paths() -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    paths.extend(system_config_paths());
    if let Some(home) = metadata::home_dir() {
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        paths.insert(codex_home.join("config.toml"));
    }
    for home in metadata::user_home_dirs() {
        paths.insert(home.join(".codex/config.toml"));
    }
    paths.extend(metadata::current_dir_ancestors(Path::new(
        ".codex/config.toml",
    )));
    paths.into_iter().collect()
}

fn system_config_paths() -> Vec<PathBuf> {
    #[cfg(unix)]
    let root = Some(PathBuf::from("/etc/codex"));
    #[cfg(windows)]
    let root: Option<PathBuf> = None;

    root.into_iter()
        .flat_map(|root| [root.join("config.toml"), root.join("managed_config.toml")])
        .collect()
}

fn skill_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    roots.extend(system_config_paths().into_iter().filter_map(|path| {
        (path.file_name().is_some_and(|name| name == "config.toml"))
            .then(|| path.parent().map(|parent| parent.join("skills")))
            .flatten()
    }));
    if let Some(home) = metadata::home_dir() {
        let codex_home = std::env::var_os("CODEX_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".codex"));
        roots.insert(codex_home.join("skills"));
    }
    for home in metadata::user_home_dirs() {
        roots.insert(home.join(".agents/skills"));
        roots.insert(home.join(".codex/skills"));
    }
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
            let transport = if url.is_some() {
                "http"
            } else if command.is_some() {
                "stdio"
            } else {
                return None;
            };
            Some(McpServer {
                name: name.clone(),
                transport: transport.to_owned(),
                command,
                url,
                enabled: server
                    .get("enabled")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(true),
                source: path.to_path_buf(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::{mcp_servers_from_toml, npm_version};

    #[test]
    fn reads_version_from_npm_package() {
        let root = temporary("codex-npm-version");
        let package = root.join("node_modules/@openai/codex");
        let executable = package.join("bin/codex.js");
        fs::create_dir_all(executable.parent().unwrap()).unwrap();
        fs::write(&executable, "#!/usr/bin/env node\n").unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name":"@openai/codex","version":"0.129.0"}"#,
        )
        .unwrap();

        assert_eq!(npm_version(&executable).as_deref(), Some("0.129.0"));

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_stdio_and_http_servers_without_secrets() {
        let path = temporary("codex-mcp.toml");
        fs::write(
            &path,
            r#"
[mcp_servers.docs]
url = "https://example.com/mcp"
bearer_token_env_var = "SECRET"
enabled = false

[mcp_servers.local]
command = "npx"
args = ["-y", "secret-package"]
[mcp_servers.local.env]
TOKEN = "secret"
"#,
        )
        .unwrap();
        let servers = mcp_servers_from_toml(&path);
        let _ = fs::remove_file(&path);

        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "docs");
        assert_eq!(servers[0].transport, "http");
        assert!(!servers[0].enabled);
        assert_eq!(servers[1].command.as_deref(), Some("npx"));
    }

    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agentdesktop-{}-{name}", std::process::id()))
    }
}
