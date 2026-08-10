use agentplane_core::model::Agent;

use super::command;

pub(super) async fn discover() -> Option<Agent> {
    let executable = command::find_in_path("codex")?;
    Some(Agent {
        version: command::version(&executable).await,
        executable,
        kind: "codex".to_owned(),
    })
}
