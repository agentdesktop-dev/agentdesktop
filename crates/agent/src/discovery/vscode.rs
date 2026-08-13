use agentdesktop_core::model::Agent;

use std::{collections::BTreeSet, path::PathBuf};

use super::metadata;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_executable("code", executable_candidates())?;
    let version = version_candidates(&executable)
        .into_iter()
        .find_map(|path| metadata::json_version(&path));
    Some(Agent {
        version,
        executable,
        kind: "vscode".to_owned(),
        mcp_servers: Vec::new(),
        skills: Vec::new(),
    })
}

fn executable_candidates() -> Vec<PathBuf> {
    let candidates = BTreeSet::new();

    #[cfg(target_os = "macos")]
    let candidates = {
        let mut candidates = candidates;
        candidates.insert(PathBuf::from(
            "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
        ));
        for home in metadata::user_home_dirs() {
            candidates.insert(
                home.join("Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"),
            );
        }
        candidates
    };

    #[cfg(windows)]
    let candidates = {
        let mut candidates = candidates;
        for home in metadata::user_home_dirs() {
            candidates.insert(home.join("AppData/Local/Programs/Microsoft VS Code/bin/code.cmd"));
            candidates.insert(home.join("AppData/Local/Programs/Microsoft VS Code/bin/code.exe"));
        }
        for root in [
            metadata::env_path("ProgramFiles"),
            metadata::env_path("ProgramFiles(x86)"),
        ]
        .into_iter()
        .flatten()
        {
            candidates.insert(root.join("Microsoft VS Code/bin/code.cmd"));
            candidates.insert(root.join("Microsoft VS Code/bin/code.exe"));
        }
        candidates
    };

    candidates.into_iter().collect()
}

fn version_candidates(executable: &std::path::Path) -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    for executable in [
        Some(executable.to_path_buf()),
        executable.canonicalize().ok(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(directory) = executable.parent() {
            candidates.insert(directory.join("resources/app/package.json"));
            candidates.insert(directory.join("../resources/app/package.json"));
            candidates.insert(directory.join("../../package.json"));
        }
    }
    candidates.extend([
        PathBuf::from("/usr/share/code/resources/app/package.json"),
        PathBuf::from("/usr/lib/code/resources/app/package.json"),
    ]);
    for home in metadata::user_home_dirs() {
        candidates.insert(
            home.join("Applications/Visual Studio Code.app/Contents/Resources/app/package.json"),
        );
    }
    candidates.insert(PathBuf::from(
        "/Applications/Visual Studio Code.app/Contents/Resources/app/package.json",
    ));
    candidates.into_iter().collect()
}
