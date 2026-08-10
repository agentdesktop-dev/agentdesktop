use std::{env, path::PathBuf};

use serde::{Deserialize, Serialize};
use tokio::process::Command;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Discovery {
    pub agents: Vec<Agent>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Agent {
    pub kind: String,
    pub executable: PathBuf,
    pub version: Option<String>,
}

pub async fn discover() -> Discovery {
    let agents = match find_in_path("codex") {
        Some(executable) => vec![Agent {
            version: version(&executable).await,
            executable,
            kind: "codex".to_owned(),
        }],
        None => Vec::new(),
    };

    Discovery { agents }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
}

async fn version(executable: &PathBuf) -> Option<String> {
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
