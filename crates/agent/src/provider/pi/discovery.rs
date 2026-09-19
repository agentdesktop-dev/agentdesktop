use std::{
    collections::BTreeSet,
    io::Read,
    path::{Path, PathBuf},
};

use agentdesktop_core::model::{Agent, McpServer};

use crate::provider::{metadata, vscode::discovery as vscode};

use super::{Pi, user_pi_agent_dir};

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_all_in_path("pi")
        .into_iter()
        .chain(
            executable_candidates()
                .into_iter()
                .filter(|candidate| candidate.is_file()),
        )
        .find(|candidate| is_pi(candidate))?;
    Some(Agent {
        version: package_version(&executable),
        executable,
        kind: Pi::ID.to_owned(),
        mcp_servers: discover_mcp_servers(),
        skills: metadata::discover_skills(skill_roots()),
    })
}

fn executable_candidates() -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        candidates.insert(home.join(".local/bin/pi"));
        candidates.insert(home.join(".npm-global/bin/pi"));
        #[cfg(windows)]
        {
            candidates.insert(home.join(".local/bin/pi.exe"));
            candidates.insert(home.join("AppData/Roaming/npm/pi.cmd"));
            candidates.insert(home.join("AppData/Roaming/npm/pi.exe"));
            candidates.insert(home.join("AppData/Local/pnpm/pi.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/pi"),
        PathBuf::from("/usr/local/bin/pi"),
    ]);
    #[cfg(target_os = "linux")]
    candidates.extend([
        PathBuf::from("/usr/bin/pi"),
        PathBuf::from("/usr/local/bin/pi"),
    ]);
    candidates.into_iter().collect()
}

fn is_pi(executable: &Path) -> bool {
    package_manifest(executable).is_some()
}

fn package_version(executable: &Path) -> Option<String> {
    let manifest = package_manifest(executable)?;
    metadata::json_package_version(&manifest, Pi::PACKAGE_NAME)
}

fn package_manifest(executable: &Path) -> Option<PathBuf> {
    let mut seen = BTreeSet::new();
    for start in [
        Some(executable.to_path_buf()),
        executable.canonicalize().ok(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(manifest) = npm_shim_manifest(&start) {
            return Some(manifest);
        }
        let mut directory = start.parent()?.to_path_buf();
        for _ in 0..8 {
            let manifest = directory.join("package.json");
            if seen.insert(manifest.clone())
                && metadata::json_package_version(&manifest, Pi::PACKAGE_NAME).is_some()
            {
                return Some(manifest);
            }
            if !directory.pop() {
                break;
            }
        }
    }
    None
}

fn npm_shim_manifest(executable: &Path) -> Option<PathBuf> {
    let manifest = executable
        .parent()?
        .join("node_modules")
        .join(Pi::PACKAGE_NAME)
        .join("package.json");
    metadata::json_package_version(&manifest, Pi::PACKAGE_NAME)?;

    // npm's Windows global launchers point to a sibling package. Verify that
    // relationship from the small launcher script without executing it.
    const MAX_SHIM_BYTES: usize = 16 * 1024;
    let file = std::fs::File::open(executable).ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut source = String::new();
    file.take((MAX_SHIM_BYTES + 1) as u64)
        .read_to_string(&mut source)
        .ok()?;
    if source.len() > MAX_SHIM_BYTES {
        return None;
    }

    let target = format!("node_modules/{}/dist/bundle/cli.js\"", Pi::PACKAGE_NAME);
    let basedir_target = format!("\"$basedir/{target}");
    let dp0_target = format!("\"%dp0%/{target}");
    source
        .lines()
        .any(|line| {
            let line = line.trim_start().replace('\\', "/");
            ((line.starts_with("exec ")
                || line.starts_with("& ")
                || line.starts_with("$input | & "))
                && line.contains(&basedir_target))
                || (line.starts_with("endLocal & ")
                    && line.contains("\"%_prog%\"")
                    && line.contains(&dp0_target))
        })
        .then_some(manifest)
}

fn discover_mcp_servers() -> Vec<McpServer> {
    mcp_config_paths()
        .into_iter()
        .flat_map(|path| vscode::mcp_servers_from_json(&path))
        .collect()
}

/// Files `pi-mcp-adapter` loads as normal MCP config.
///
/// Host-specific Cursor/Claude/Codex files are not included: the adapter only
/// imports those after `/mcp setup` writes them into a Pi-owned file.
fn mcp_config_paths() -> Vec<PathBuf> {
    let mut paths = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        let agent_dir = user_pi_agent_dir(&home);
        paths.insert(agent_dir.join("mcp.json"));
        paths.insert(home.join(".config/mcp/mcp.json"));
        paths.insert(home.join(".agents/mcp.json"));
        paths.insert(home.join(".agents/mcp/mcp.json"));
    }
    paths.extend(metadata::current_dir_ancestors(Path::new(".mcp.json")));
    paths.extend(metadata::current_dir_ancestors(Path::new(".pi/mcp.json")));
    paths.into_iter().collect()
}

fn skill_roots() -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        let agent_dir = user_pi_agent_dir(&home);
        roots.insert(agent_dir.join("skills"));
        roots.insert(home.join(".agents/skills"));
    }
    roots.extend(metadata::current_dir_ancestors(Path::new(".pi/skills")));
    roots.extend(metadata::current_dir_ancestors(Path::new(".agents/skills")));
    roots.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use serde_json::json;

    use super::{is_pi, mcp_config_paths, package_version, skill_roots};
    use crate::provider::pi::user_pi_agent_dir;
    use crate::provider::vscode::discovery::mcp_servers_from_value;

    #[test]
    fn accepts_earendil_pi_package_and_reads_version() {
        let root = temporary("pi-package");
        let package = root.join("node_modules/@earendil-works/pi-coding-agent");
        let bundle = package.join("dist/bundle");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name":"@earendil-works/pi-coding-agent","version":"0.85.1"}"#,
        )
        .unwrap();
        let executable = bundle.join("cli.js");
        fs::write(&executable, "#!/usr/bin/env node\n").unwrap();

        assert!(is_pi(&executable));
        assert_eq!(package_version(&executable).as_deref(), Some("0.85.1"));

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_an_unrelated_pi_binary() {
        let root = temporary("pi-unrelated");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"name":"raspberry-pi-tools","version":"1.0.0"}"#,
        )
        .unwrap();
        let executable = root.join("pi");
        fs::write(&executable, "#!/bin/sh\n").unwrap();

        assert!(!is_pi(&executable));
        assert_eq!(package_version(&executable), None);

        let _ = fs::remove_dir_all(root);
    }

    // npm-generated Windows global shims: package.json is beside these launchers
    // under node_modules, rather than in an ancestor of the launcher itself.
    const NPM_CMD_SHIM: &str = r#"@ECHO off
GOTO start
:find_dp0
SET dp0=%~dp0
EXIT /b
:start
SETLOCAL
CALL :find_dp0

IF EXIST "%dp0%\node.exe" (
  SET "_prog=%dp0%\node.exe"
) ELSE (
  SET "_prog=node"
  SET PATHEXT=%PATHEXT:;.JS;=;%
)

endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & "%_prog%"  "%dp0%\node_modules\@earendil-works\pi-coding-agent\dist\bundle\cli.js" %*
"#;

    const NPM_POWERSHELL_SHIM: &str = r#"#!/usr/bin/env pwsh
$basedir=Split-Path $MyInvocation.MyCommand.Definition -Parent

$exe=""
if ($PSVersionTable.PSVersion -lt "6.0" -or $IsWindows) {
  # Fix case when both the Windows and Linux builds of Node
  # are installed in the same directory
  $exe=".exe"
}
$ret=0
if (Test-Path "$basedir/node$exe") {
  # Support pipeline input
  if ($MyInvocation.ExpectingInput) {
    $input | & "$basedir/node$exe"  "$basedir/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js" $args
  } else {
    & "$basedir/node$exe"  "$basedir/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js" $args
  }
  $ret=$LASTEXITCODE
} else {
  # Support pipeline input
  if ($MyInvocation.ExpectingInput) {
    $input | & "node$exe"  "$basedir/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js" $args
  } else {
    & "node$exe"  "$basedir/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js" $args
  }
  $ret=$LASTEXITCODE
}
exit $ret
"#;

    const NPM_SHELL_SHIM: &str = r#"#!/bin/sh
basedir=$(dirname "$(echo "$0" | sed -e 's,\\,/,g')")

case `uname` in
    *CYGWIN*|*MINGW*|*MSYS*)
        if command -v cygpath > /dev/null 2>&1; then
            basedir=`cygpath -w "$basedir"`
        fi
    ;;
esac

if [ -x "$basedir/node" ]; then
  exec "$basedir/node"  "$basedir/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js" "$@"
else
  exec node  "$basedir/node_modules/@earendil-works/pi-coding-agent/dist/bundle/cli.js" "$@"
fi
"#;

    fn npm_shim_fixture(name: &str, source: &str) -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let package = root
            .path()
            .join("node_modules/@earendil-works/pi-coding-agent");
        fs::create_dir_all(package.join("dist/bundle")).unwrap();
        fs::write(
            package.join("package.json"),
            r#"{"name":"@earendil-works/pi-coding-agent","version":"0.85.1"}"#,
        )
        .unwrap();
        fs::write(package.join("dist/bundle/cli.js"), "#!/usr/bin/env node\n").unwrap();
        let shim = root.path().join(name);
        fs::write(&shim, source).unwrap();
        (root, shim)
    }

    #[test]
    fn accepts_npm_cmd_shim_and_reads_version() {
        let (_root, shim) = npm_shim_fixture("pi.cmd", NPM_CMD_SHIM);
        assert!(is_pi(&shim));
        assert_eq!(package_version(&shim).as_deref(), Some("0.85.1"));
    }

    #[test]
    fn accepts_npm_powershell_shim_and_reads_version() {
        let (_root, shim) = npm_shim_fixture("pi.ps1", NPM_POWERSHELL_SHIM);
        assert!(is_pi(&shim));
        assert_eq!(package_version(&shim).as_deref(), Some("0.85.1"));
    }

    #[test]
    fn accepts_npm_shell_shim_and_reads_version() {
        let (_root, shim) = npm_shim_fixture("pi", NPM_SHELL_SHIM);
        assert!(is_pi(&shim));
        assert_eq!(package_version(&shim).as_deref(), Some("0.85.1"));
    }

    #[test]
    fn rejects_unrelated_launchers_beside_pi_package() {
        for (name, source) in [
            ("pi.cmd", NPM_CMD_SHIM),
            ("pi.ps1", NPM_POWERSHELL_SHIM),
            ("pi", NPM_SHELL_SHIM),
        ] {
            let unrelated = source
                .replace("@earendil-works/pi-coding-agent", "unrelated-pi")
                .replace(r"@earendil-works\pi-coding-agent", "unrelated-pi");
            let (_root, shim) = npm_shim_fixture(name, &unrelated);
            assert!(!is_pi(&shim));
            assert_eq!(package_version(&shim), None);
        }
    }

    #[test]
    fn rejects_npm_shim_without_matching_package_metadata() {
        let (root, shim) = npm_shim_fixture("pi.cmd", NPM_CMD_SHIM);
        let manifest = root
            .path()
            .join("node_modules/@earendil-works/pi-coding-agent/package.json");
        for contents in [
            r#"{"name":"unrelated-pi","version":"0.85.1"}"#,
            r#"{"name":"@earendil-works/pi-coding-agent"}"#,
        ] {
            fs::write(&manifest, contents).unwrap();
            assert!(!is_pi(&shim));
            assert_eq!(package_version(&shim), None);
        }
    }

    #[test]
    fn rejects_commented_npm_shim_references() {
        let source = format!(
            "{}\nexec unrelated-pi\n",
            NPM_SHELL_SHIM
                .lines()
                .map(|line| format!("# {line}\n"))
                .collect::<String>()
        );
        let (_root, shim) = npm_shim_fixture("pi", &source);
        assert!(!is_pi(&shim));
    }

    #[test]
    fn rejects_oversized_npm_shims() {
        let source = format!("{NPM_CMD_SHIM}{}", " ".repeat(16 * 1024));
        let (_root, shim) = npm_shim_fixture("pi.cmd", &source);
        assert!(!is_pi(&shim));
    }

    #[test]
    fn reads_adapter_servers_without_secrets_or_arguments() {
        let servers = mcp_servers_from_value(
            &json!({
                "mcpServers": {
                    "github": {
                        "url": "https://api.githubcopilot.com/mcp",
                        "headers": { "Authorization": "secret" }
                    },
                    "chrome": {
                        "command": "npx",
                        "args": ["-y", "chrome-devtools-mcp@1.6.0"],
                        "env": { "TOKEN": "secret" }
                    },
                    "skipped": {
                        "command": "npx",
                        "disabled": true
                    }
                }
            }),
            Path::new("/home/tester/.pi/agent/mcp.json"),
        );

        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0].name, "chrome");
        assert_eq!(servers[0].transport, "stdio");
        assert_eq!(servers[0].command.as_deref(), Some("npx"));
        assert!(servers[0].enabled);
        assert_eq!(servers[1].name, "github");
        assert_eq!(servers[1].transport, "http");
        assert_eq!(
            servers[1].url.as_deref(),
            Some("https://api.githubcopilot.com/mcp")
        );
        assert!(!servers[2].enabled);
        assert!(
            servers
                .iter()
                .all(|server| server.source.ends_with("mcp.json"))
        );
    }

    #[test]
    fn scans_pi_owned_and_shared_mcp_files() {
        let paths = mcp_config_paths();
        for home in crate::provider::metadata::user_home_dirs() {
            assert!(paths.contains(&user_pi_agent_dir(&home).join("mcp.json")));
        }
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with(std::path::Path::new(".config/mcp/mcp.json")))
        );
        assert!(paths.iter().any(|path| path.ends_with(".mcp.json")));
        assert!(
            paths
                .iter()
                .any(|path| path.ends_with(std::path::Path::new(".pi/mcp.json")))
        );
    }

    #[test]
    fn pi_agent_dir_overrides_match_all_consumers() {
        const CHILD_HOME: &str = "AGENTDESKTOP_PI_PATH_TEST_HOME";
        const CHILD_EXPECTED: &str = "AGENTDESKTOP_PI_PATH_TEST_EXPECTED";
        if let Some(home) = std::env::var_os(CHILD_HOME) {
            let home = PathBuf::from(home);
            let expected = PathBuf::from(std::env::var_os(CHILD_EXPECTED).unwrap());
            assert_eq!(user_pi_agent_dir(&home), expected);
            assert!(mcp_config_paths().contains(&expected.join("mcp.json")));
            assert!(skill_roots().contains(&expected.join("skills")));
            return;
        }

        // Isolate environment overrides in a subprocess so parallel tests never
        // observe changes to HOME, USERPROFILE, or PI_CODING_AGENT_DIR.
        let home = tempfile::tempdir().unwrap();
        let absolute = home.path().join("absolute-agent");
        let cases = [
            (
                Some(Path::new("~/.pi/custom-agent")),
                home.path().join(".pi/custom-agent"),
            ),
            (Some(Path::new("~")), home.path().to_owned()),
            (
                Some(Path::new("relative-agent")),
                PathBuf::from("relative-agent"),
            ),
            (Some(absolute.as_path()), absolute.clone()),
            (Some(Path::new("")), home.path().join(".pi/agent")),
            (None, home.path().join(".pi/agent")),
            (
                Some(Path::new("~another-user/agent")),
                PathBuf::from("~another-user/agent"),
            ),
            #[cfg(windows)]
            (
                Some(Path::new(r"~\.pi\custom-agent")),
                home.path().join(".pi/custom-agent"),
            ),
            #[cfg(unix)]
            (
                Some(Path::new(r"~\.pi\custom-agent")),
                PathBuf::from(r"~\.pi\custom-agent"),
            ),
        ];
        for (configured, expected) in cases {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "provider::pi::discovery::tests::pi_agent_dir_overrides_match_all_consumers",
                    "--nocapture",
                ])
                .env(CHILD_HOME, home.path())
                .env(CHILD_EXPECTED, expected)
                .env("HOME", home.path())
                .env("USERPROFILE", home.path());
            if let Some(configured) = configured {
                command.env("PI_CODING_AGENT_DIR", configured);
            } else {
                command.env_remove("PI_CODING_AGENT_DIR");
            }
            let output = command.output().unwrap();
            assert!(
                output.status.success(),
                "PI_CODING_AGENT_DIR={configured:?}:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }

    fn temporary(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("agentdesktop-{name}-{}", std::process::id()))
    }
}
