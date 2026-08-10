use agentplane_core::model::Agent;

use super::metadata;

pub(super) fn discover() -> Option<Agent> {
    let executable = metadata::find_in_path("codex")?;
    let version = metadata::version_after_component(&executable, "releases").and_then(|release| {
        let target_marker = format!("-{}-", std::env::consts::ARCH);
        release
            .split_once(&target_marker)
            .map(|(version, _)| version.to_owned())
    });
    Some(Agent {
        version,
        executable,
        kind: "codex".to_owned(),
    })
}
