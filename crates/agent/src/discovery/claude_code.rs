use agentdesktop_core::model::Agent;

use super::metadata;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_in_path("claude")?;
    Some(Agent {
        version: metadata::version_after_component(&executable, "versions"),
        executable,
        kind: "claude-code".to_owned(),
    })
}
