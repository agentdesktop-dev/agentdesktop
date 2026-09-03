use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};
use memchr::memmem;

use super::metadata;

const VERSION_MARKER: &[u8] = b"goose/";
const MAX_VERSION_LENGTH: usize = 64;
const MAX_BINARY_SIZE: u64 = 512 * 1024 * 1024;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_executable("goose", executable_candidates())?;
    Some(Agent {
        version: embedded_version(&executable),
        executable,
        kind: "goose".to_owned(),
        mcp_servers: discover_mcp_servers(),
        skills: metadata::discover_skills(skill_roots()),
    })
}

fn executable_candidates() -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        candidates.insert(home.join(".local/bin/goose"));
        #[cfg(windows)]
        {
            candidates.insert(home.join(".local/bin/goose.exe"));
            candidates.insert(home.join("goose/goose.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/goose"),
        PathBuf::from("/usr/local/bin/goose"),
    ]);
    candidates.into_iter().collect()
}

fn config_paths() -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    #[cfg(unix)]
    paths.insert(PathBuf::from("/etc/goose/config.yaml"));
    for home in metadata::user_home_dirs() {
        #[cfg(not(windows))]
        paths.insert(home.join(".config/goose/config.yaml"));
        #[cfg(windows)]
        paths.insert(home.join("AppData/Roaming/Block/goose/config/config.yaml"));
    }
    paths.into_iter().collect()
}

fn discover_mcp_servers() -> Vec<McpServer> {
    config_paths()
        .into_iter()
        .flat_map(|path| mcp_servers_from_yaml(&path))
        .collect()
}

fn mcp_servers_from_yaml(path: &Path) -> Vec<McpServer> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(document) = serde_yaml::from_str::<serde_yaml::Value>(&contents) else {
        return Vec::new();
    };
    let Some(extensions) = document
        .get("extensions")
        .and_then(serde_yaml::Value::as_mapping)
    else {
        return Vec::new();
    };

    extensions
        .iter()
        .filter_map(|(name, value)| {
            let name = name.as_str()?.to_owned();
            let extension = value.as_mapping()?;
            let kind = extension.get("type").and_then(serde_yaml::Value::as_str)?;
            let enabled = extension
                .get("enabled")
                .and_then(serde_yaml::Value::as_bool)
                .unwrap_or(false);
            let (transport, command, url) = match kind {
                "stdio" => (
                    "stdio",
                    extension
                        .get("cmd")
                        .and_then(serde_yaml::Value::as_str)
                        .map(str::to_owned),
                    None,
                ),
                "streamable_http" => (
                    "http",
                    None,
                    extension
                        .get("uri")
                        .and_then(serde_yaml::Value::as_str)
                        .map(str::to_owned),
                ),
                _ => return None,
            };
            if command.is_none() && url.is_none() {
                return None;
            }
            Some(McpServer {
                name,
                transport: transport.to_owned(),
                command,
                url,
                enabled,
                source: path.to_path_buf(),
            })
        })
        .collect()
}

fn skill_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        roots.insert(home.join(".agents/skills"));
        roots.insert(home.join(".claude/skills"));
        #[cfg(not(windows))]
        {
            roots.insert(home.join(".config/agents/skills"));
            roots.insert(home.join(".config/goose/skills"));
        }
        #[cfg(windows)]
        roots.insert(home.join("AppData/Roaming/Block/goose/config/skills"));
    }
    for relative in [
        Path::new(".agents/skills"),
        Path::new(".claude/skills"),
        Path::new(".goose/skills"),
    ] {
        roots.extend(metadata::current_dir_ancestors(relative));
    }
    roots.into_iter().collect()
}

/// Reads Goose's embedded user-agent marker without executing the binary.
fn embedded_version(executable: &Path) -> Option<String> {
    let file = File::open(executable).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_BINARY_SIZE {
        return None;
    }
    embedded_version_from_reader(file, 64 * 1024)
}

fn embedded_version_from_reader(mut reader: impl Read, chunk_size: usize) -> Option<String> {
    let chunk_size = chunk_size.max(1);
    let retained_length = VERSION_MARKER.len() + MAX_VERSION_LENGTH;
    let mut chunk = vec![0; chunk_size];
    let mut retained = Vec::new();

    loop {
        let read = reader.read(&mut chunk).ok()?;
        let finished = read == 0;
        retained.extend_from_slice(&chunk[..read]);
        if let Some(version) = find_version(&retained, finished) {
            return Some(version);
        }
        if finished {
            return None;
        }
        if retained.len() > retained_length {
            retained.drain(..retained.len() - retained_length);
        }
    }
}

fn find_version(bytes: &[u8], finished: bool) -> Option<String> {
    for start in memmem::find_iter(bytes, VERSION_MARKER) {
        let value = &bytes[start + VERSION_MARKER.len()..];
        let length = value
            .iter()
            .take(MAX_VERSION_LENGTH)
            .take_while(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
            .count();
        if length == value.len() && !finished {
            continue;
        }
        let Ok(version) = std::str::from_utf8(&value[..length]) else {
            continue;
        };
        if valid_version(version) {
            return Some(version.to_owned());
        }
    }
    None
}

fn valid_version(version: &str) -> bool {
    let core = version
        .split_once(['-', '+'])
        .map_or(version, |(core, _)| core);
    let components: Vec<_> = core.split('.').collect();
    components.len() == 3
        && components.iter().all(|component| {
            !component.is_empty() && component.bytes().all(|byte| byte.is_ascii_digit())
        })
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Cursor, path::PathBuf};

    use super::{embedded_version_from_reader, mcp_servers_from_yaml};

    #[test]
    fn reads_version_across_chunk_boundaries() {
        let binary = b"prefix goose/1.49.0 suffix";
        assert_eq!(
            embedded_version_from_reader(Cursor::new(binary), 5).as_deref(),
            Some("1.49.0")
        );
    }

    #[test]
    fn reads_stdio_and_http_extensions_without_secrets_or_arguments() {
        let path = temporary("goose-extensions.yaml");
        fs::write(
            &path,
            r#"
extensions:
  local:
    type: stdio
    enabled: true
    cmd: npx
    args: ["-y", "secret-package"]
    envs:
      TOKEN: secret
  docs:
    type: streamable_http
    enabled: false
    uri: https://example.com/mcp
    headers:
      Authorization: secret
  developer:
    type: platform
    enabled: true
"#,
        )
        .unwrap();

        let servers = mcp_servers_from_yaml(&path);
        let _ = fs::remove_file(&path);

        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "local");
        assert_eq!(servers[0].command.as_deref(), Some("npx"));
        assert!(servers[0].enabled);
        assert_eq!(servers[1].name, "docs");
        assert_eq!(servers[1].url.as_deref(), Some("https://example.com/mcp"));
        assert!(!servers[1].enabled);
    }

    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agentdesktop-{}-{name}", std::process::id()))
    }
}
