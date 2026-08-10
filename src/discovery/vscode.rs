use super::{Agent, command};

pub(super) async fn discover() -> Option<Agent> {
    let executable = command::find_in_path("code")?;
    let version = command::version(&executable)
        .await
        .and_then(|output| output.lines().next().map(str::to_owned));
    Some(Agent {
        version,
        executable,
        kind: "vscode".to_owned(),
    })
}
