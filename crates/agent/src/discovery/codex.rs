use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};

use super::metadata;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_in_path("codex")?;
    let version = metadata::version_after_component(&executable, "releases").and_then(|release| {
        let target_marker = format!("-{}-", std::env::consts::ARCH);
        release
            .split_once(&target_marker)
            .map(|(version, _)| version.to_owned())
    });
    Some(Agent {
        version,
        executable,
        kind: "codex".to_owned(),
        mcp_servers: discover_mcp_servers(),
        skills: metadata::discover_skills(skill_roots()),
    })
}

fn discover_mcp_servers() -> Vec<McpServer> {
    config_paths()
        .into_iter()
        .flat_map(|path| mcp_servers_from_toml(&path))
        .collect()
}

fn config_paths() -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    paths.insert(PathBuf::from("/etc/codex/config.toml"));
    paths.insert(PathBuf::from("/etc/codex/managed_config.toml"));
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

fn skill_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    roots.insert(PathBuf::from("/etc/codex/skills"));
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

    use super::mcp_servers_from_toml;

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
