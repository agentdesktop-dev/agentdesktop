use agentplane_core::model::Agent;

use super::command;

pub(super) async fn discover() -> Option<Agent> {
    let executable = command::find_in_path("claude")?;
    Some(Agent {
        version: command::version(&executable).await,
        executable,
        kind: "claude-code".to_owned(),
    })
}
