use agentdesktop_core::model::Agent;

use super::metadata;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_in_path("opencode")?;
    Some(Agent {
        version: metadata::pacman_version_for_file(&executable),
        executable,
        kind: "opencode".to_owned(),
    })
}
