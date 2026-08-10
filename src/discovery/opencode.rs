use super::{Agent, command};

pub(super) async fn discover() -> Option<Agent> {
    let executable = command::find_in_path("opencode")?;
    Some(Agent {
        version: command::version(&executable).await,
        executable,
        kind: "opencode".to_owned(),
    })
}
