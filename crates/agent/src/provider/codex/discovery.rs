use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};

use super::Codex;
use crate::provider::{context::ScanContext, files, mcp, metadata};

pub(super) fn discover() -> Option<Agent> {
    let context = ScanContext::capture();
    discover_with(&context)
}

fn discover_with(context: &ScanContext) -> Option<Agent> {
    let executable = context.find_executable(Codex::ID, executable_candidates(context))?;
    let version = standalone_version(&executable).or_else(|| npm_version(&executable));
    Some(Agent {
        version,
        executable,
        kind: Codex::ID.to_owned(),
        mcp_servers: discover_mcp_servers(context),
        skills: metadata::discover_skills(skill_roots(context)),
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

fn executable_candidates(context: &ScanContext) -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    for home in context.homes() {
        candidates.insert(home.join(".local/bin/codex"));
        candidates.insert(home.join(".npm-global/bin/codex"));
        #[cfg(windows)]
        {
            candidates.insert(home.join(".local/bin/codex.exe"));
            candidates.insert(home.join("AppData/Roaming/npm/codex.cmd"));
        }
    }
    #[cfg(target_os = "macos")]
    candidates.extend(
        ["/opt/homebrew/bin/codex", "/usr/local/bin/codex"]
            .into_iter()
            .filter_map(|path| context.system_path(path)),
    );
    candidates.into_iter().collect()
}

fn discover_mcp_servers(context: &ScanContext) -> Vec<McpServer> {
    config_paths(context)
        .into_iter()
        .flat_map(|path| mcp_servers_from_toml(&path))
        .collect()
}

fn config_paths(context: &ScanContext) -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    if let Some(root) = system_root(context) {
        paths.extend([root.join("config.toml"), root.join("managed_config.toml")]);
    }
    if let Some(codex_home) = codex_home(context) {
        paths.insert(codex_home.join("config.toml"));
    }
    for home in context.homes() {
        paths.insert(home.join(".codex/config.toml"));
    }
    paths.extend(context.current_dir_ancestors(Path::new(".codex/config.toml")));
    paths.into_iter().collect()
}

fn codex_home(context: &ScanContext) -> Option<PathBuf> {
    context
        .env_path("CODEX_HOME")
        .or_else(|| context.home().map(|home| home.join(".codex")))
}

fn system_root(context: &ScanContext) -> Option<PathBuf> {
    if cfg!(unix) {
        context.system_path("/etc/codex")
    } else {
        None
    }
}

fn skill_roots(context: &ScanContext) -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    roots.extend(system_root(context).map(|root| root.join("skills")));
    if let Some(codex_home) = codex_home(context) {
        roots.insert(codex_home.join("skills"));
    }
    for home in context.homes() {
        roots.insert(home.join(".agents/skills"));
        roots.insert(home.join(".codex/skills"));
    }
    roots.extend(context.current_dir_ancestors(Path::new(".agents/skills")));
    roots.into_iter().collect()
}

fn mcp_servers_from_toml(path: &Path) -> Vec<McpServer> {
    let Some(document) = files::read_toml::<toml::Value>(path) else {
        return Vec::new();
    };
    let Some(servers) = document.get("mcp_servers").and_then(toml::Value::as_table) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(name, value)| {
            let server = value.as_table()?;
            let command = server.get("command").and_then(toml::Value::as_str);
            let url = server.get("url").and_then(toml::Value::as_str);
            let transport = if url.is_some() {
                "http"
            } else if command.is_some() {
                "stdio"
            } else {
                return None;
            };
            Some(mcp::server(
                name,
                transport,
                command,
                url,
                server
                    .get("enabled")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(true),
                path,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::PathBuf};

    use super::{
        ScanContext, config_paths, executable_candidates, mcp_servers_from_toml, npm_version,
        skill_roots, system_root,
    };

    #[cfg(unix)]
    #[test]
    fn native_system_configurations_and_skills_are_included_in_sorted_paths() {
        // Inspect candidate paths only; do not read any host configuration or skill files.
        let context = ScanContext::capture();
        let configs = config_paths(&context);
        let skills = skill_roots(&context);
        let root = PathBuf::from("/etc/codex");
        for name in ["config.toml", "managed_config.toml"] {
            assert!(configs.contains(&root.join(name)));
        }
        assert!(skills.contains(&root.join("skills")));
        assert!(configs.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(skills.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn isolated_paths_preserve_scanned_defaults_and_codex_home_overrides() {
        let root = temporary("codex-paths");
        let home = root.join("home");
        let project = root.join("project");
        let cwd = project.join("nested");

        for override_path in [None, Some(root.join("custom-codex")), Some(PathBuf::new())] {
            let mut context = ScanContext::isolated(home.clone(), cwd.clone());
            let mut configs = BTreeSet::from([
                home.join(".codex/config.toml"),
                cwd.join(".codex/config.toml"),
                project.join(".codex/config.toml"),
                root.join(".codex/config.toml"),
            ]);
            let mut skills = BTreeSet::from([
                home.join(".codex/skills"),
                home.join(".agents/skills"),
                cwd.join(".agents/skills"),
                project.join(".agents/skills"),
                root.join(".agents/skills"),
            ]);
            if let Some(override_path) = override_path {
                if !override_path.as_os_str().is_empty() {
                    configs.insert(override_path.join("config.toml"));
                    skills.insert(override_path.join("skills"));
                }
                context = context.with_override("CODEX_HOME", override_path);
            }

            assert_eq!(config_paths(&context), Vec::from_iter(configs));
            assert_eq!(skill_roots(&context), Vec::from_iter(skills));
            assert!(system_root(&context).is_none());
            let executables = executable_candidates(&context);
            assert!(executables.contains(&home.join(".local/bin/codex")));
            assert!(executables.iter().all(|path| path.starts_with(&home)));
        }
    }

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
url = "https://USER_SENTINEL:PASS_SENTINEL@example.com/PATH_SENTINEL/mcp?key=QUERY_SENTINEL#FRAGMENT_SENTINEL"
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
        assert_eq!(servers[0].url.as_deref(), Some("https://example.com/"));
        assert!(!servers[0].enabled);
        assert_eq!(servers[1].command.as_deref(), Some("npx"));
        assert_eq!(servers[1].transport, "stdio");
        assert!(servers[1].enabled);
        assert!(servers.iter().all(|server| server.source == path));
        let serialized = serde_json::to_string(&servers).unwrap();
        assert!(!serialized.contains("SENTINEL"));
        assert!(!serialized.contains("SECRET"));
        assert!(!serialized.contains("secret"));
    }

    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agentdesktop-{}-{name}", std::process::id()))
    }
}
