mod claude_code;
mod codex;
mod command;
mod opencode;
mod vscode;

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

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
    let (codex, opencode, claude_code, vscode) = tokio::join!(
        codex::discover(),
        opencode::discover(),
        claude_code::discover(),
        vscode::discover(),
    );

    Discovery {
        agents: [codex, opencode, claude_code, vscode]
            .into_iter()
            .flatten()
            .collect(),
    }
}
