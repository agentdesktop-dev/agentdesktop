use super::OpenCode;

use std::{collections::BTreeSet, path::PathBuf};

use agentdesktop_core::model::Agent;

use crate::provider::metadata;

// `opencode/<version>` before 2.0, `opencode/<channel>/<version>/cli` since.
const VERSION_MARKER: &[u8] = b"user-agent=opencode/";
const MAX_BINARY_SIZE: u64 = 512 * 1024 * 1024;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_executable(OpenCode::ID, executable_candidates())?;
    let version_source = metadata::resolve_exec_wrapper(&executable);
    Some(Agent {
        version: metadata::embedded_version(&version_source, VERSION_MARKER, MAX_BINARY_SIZE),
        executable,
        kind: OpenCode::ID.to_owned(),
        mcp_servers: Vec::new(),
        skills: Vec::new(),
    })
}

fn executable_candidates() -> Vec<PathBuf> {
    let mut candidates = BTreeSet::new();
    for home in metadata::user_home_dirs() {
        candidates.insert(home.join(".opencode/bin/opencode"));
        candidates.insert(home.join(".local/bin/opencode"));
        #[cfg(windows)]
        {
            candidates.insert(home.join(".opencode/bin/opencode.exe"));
            candidates.insert(home.join(".local/bin/opencode.exe"));
            candidates.insert(home.join("AppData/Roaming/npm/opencode.cmd"));
        }
    }
    #[cfg(target_os = "macos")]
    candidates.extend([
        PathBuf::from("/opt/homebrew/bin/opencode"),
        PathBuf::from("/usr/local/bin/opencode"),
    ]);
    candidates.into_iter().collect()
}
