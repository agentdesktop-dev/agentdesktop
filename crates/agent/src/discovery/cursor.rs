use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};

use super::{metadata, vscode};

/// Product name recorded in Cursor's packaged `package.json`.
const PRODUCT_NAME: &str = "Cursor";

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_executable("cursor", executable_candidates())?;
    let version = metadata::packaged_manifest_candidates(&executable)
        .into_iter()
        .chain(well_known_manifests())
        .find_map(|path| metadata::json_package_version(&path, PRODUCT_NAME));
    Some(Agent {
        version,
        executable,
        kind: "cursor".to_owned(),
        mcp_servers: discover_mcp_servers(),
        skills: metadata::discover_skills(skill_roots()),
    })
}

/// Cursor is a Visual Studio Code fork and reads the same MCP configuration
/// schema, so the VS Code parser is reused against Cursor's own paths.
fn discover_mcp_servers() -> Vec<McpServer> {
    mcp_config_paths()
        .into_iter()
        .flat_map(|path| vscode::mcp_servers_from_json(&path))
        .collect()
}

fn mcp_config_paths() -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        paths.insert(home.join(".cursor/mcp.json"));
    }
    paths.extend(metadata::current_dir_ancestors(Path::new(
        ".cursor/mcp.json",
    )));
    paths.into_iter().collect()
}

/// Roots Cursor loads skills from, including the Claude Code and Codex
/// directories it reads for compatibility.
fn skill_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for relative in [
        ".cursor/skills",
        ".agents/skills",
        ".claude/skills",
        ".codex/skills",
    ] {
        roots.extend(metadata::current_dir_ancestors(Path::new(relative)));
    }
    for home in metadata::user_home_dirs() {
        roots.insert(home.join(".cursor/skills"));
        roots.insert(home.join(".agents/skills"));
        roots.insert(home.join(".claude/skills"));
        roots.insert(home.join(".codex/skills"));
    }
    roots.into_iter().collect()
}

fn executable_candidates() -> Vec<PathBuf> {
    #[allow(unused_mut)]
    let mut candidates = BTreeSet::new();

    #[cfg(target_os = "macos")]
    {
        candidates.insert(PathBuf::from(
            "/Applications/Cursor.app/Contents/Resources/app/bin/cursor",
        ));
        for home in metadata::user_home_dirs() {
            candidates
                .insert(home.join("Applications/Cursor.app/Contents/Resources/app/bin/cursor"));
        }
    }

    #[cfg(target_os = "linux")]
    {
        candidates.extend([
            PathBuf::from("/usr/share/cursor/bin/cursor"),
            PathBuf::from("/usr/lib/cursor/bin/cursor"),
            PathBuf::from("/opt/cursor/bin/cursor"),
            PathBuf::from("/opt/Cursor/bin/cursor"),
            PathBuf::from("/usr/local/bin/cursor"),
        ]);
        for home in metadata::user_home_dirs() {
            candidates.insert(home.join(".local/bin/cursor"));
        }
    }

    #[cfg(windows)]
    {
        for home in metadata::user_home_dirs() {
            candidates
                .insert(home.join("AppData/Local/Programs/Cursor/resources/app/bin/cursor.cmd"));
            candidates
                .insert(home.join("AppData/Local/Programs/Cursor/resources/app/bin/cursor.exe"));
        }
        for root in [
            metadata::env_path("ProgramFiles"),
            metadata::env_path("ProgramFiles(x86)"),
        ]
        .into_iter()
        .flatten()
        {
            candidates.insert(root.join("Cursor/resources/app/bin/cursor.cmd"));
            candidates.insert(root.join("Cursor/resources/app/bin/cursor.exe"));
        }
    }

    candidates.into_iter().collect()
}

/// Install roots checked when the executable does not sit next to its manifest,
/// such as a `cursor` symlink placed on `PATH`.
fn well_known_manifests() -> Vec<PathBuf> {
    #[allow(unused_mut)]
    let mut candidates = BTreeSet::new();

    #[cfg(target_os = "macos")]
    {
        candidates.insert(PathBuf::from(
            "/Applications/Cursor.app/Contents/Resources/app/package.json",
        ));
        for home in metadata::user_home_dirs() {
            candidates
                .insert(home.join("Applications/Cursor.app/Contents/Resources/app/package.json"));
        }
    }

    #[cfg(target_os = "linux")]
    candidates.extend([
        PathBuf::from("/usr/share/cursor/resources/app/package.json"),
        PathBuf::from("/usr/lib/cursor/resources/app/package.json"),
        PathBuf::from("/opt/cursor/resources/app/package.json"),
        PathBuf::from("/opt/Cursor/resources/app/package.json"),
    ]);

    #[cfg(windows)]
    for home in metadata::user_home_dirs() {
        candidates.insert(home.join("AppData/Local/Programs/Cursor/resources/app/package.json"));
    }

    candidates.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use serde_json::json;

    use super::{mcp_config_paths, skill_roots};
    use crate::discovery::vscode::mcp_servers_from_value;

    #[test]
    fn reads_cursor_servers_without_secrets_or_arguments() {
        let servers = mcp_servers_from_value(
            &json!({
                "mcpServers": {
                    "notion": {
                        "url": "https://mcp.notion.com/mcp",
                        "headers": { "Authorization": "secret" }
                    },
                    "local": {
                        "command": "npx",
                        "args": ["-y", "secret-package"],
                        "env": { "TOKEN": "secret" }
                    }
                }
            }),
            Path::new(".cursor/mcp.json"),
        );

        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].name, "local");
        assert_eq!(servers[0].transport, "stdio");
        assert_eq!(servers[0].command.as_deref(), Some("npx"));
        assert_eq!(servers[1].name, "notion");
        assert_eq!(servers[1].transport, "http");
        assert_eq!(
            servers[1].url.as_deref(),
            Some("https://mcp.notion.com/mcp")
        );
    }

    #[test]
    fn reads_the_user_level_mcp_configuration() {
        assert!(
            mcp_config_paths()
                .iter()
                .any(|path| path.ends_with(".cursor/mcp.json"))
        );
    }

    #[test]
    fn includes_the_compatibility_skill_roots_cursor_reads() {
        let roots = skill_roots();
        for relative in [
            ".cursor/skills",
            ".agents/skills",
            ".claude/skills",
            ".codex/skills",
        ] {
            assert!(
                roots.iter().any(|root| root.ends_with(relative)),
                "missing skill root {relative}"
            );
        }
    }

    #[test]
    fn ignores_cursors_bundled_skill_directory() {
        // Cursor ships its own skills in `~/.cursor/skills-cursor`, which is
        // not one of the skill roots Cursor documents. Reporting it would add
        // the same vendor defaults to every device in a fleet.
        assert!(
            !skill_roots()
                .iter()
                .any(|root: &PathBuf| root.ends_with(".cursor/skills-cursor"))
        );
    }
}
