mod claude_code;
mod codex;
mod command;
mod opencode;
mod vscode;

use agentplane_core::model::Discovery;

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
