use std::{env, path::PathBuf};

use tokio::process::Command;

pub(super) fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

pub(super) async fn version(executable: &PathBuf) -> Option<String> {
    let output = Command::new(executable)
        .arg("--version")
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let version = String::from_utf8(output.stdout).ok()?;
    Some(version.trim().to_owned()).filter(|version| !version.is_empty())
}
