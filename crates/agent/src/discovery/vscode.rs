use agentplane_core::model::Agent;

use std::path::Path;

use super::metadata;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_in_path("code")?;
    let version = [
        "/usr/share/code/resources/app/package.json",
        "/usr/lib/code/resources/app/package.json",
    ]
    .into_iter()
    .find_map(|path| metadata::json_version(Path::new(path)));
    Some(Agent {
        version,
        executable,
        kind: "vscode".to_owned(),
    })
}
