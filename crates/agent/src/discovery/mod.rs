mod claude_code;
mod claude_desktop;
mod codex;
pub(crate) mod metadata;
mod ollama;
mod opencode;
mod vscode;

use agentdesktop_core::model::Discovery;

pub async fn discover() -> Discovery {
    let ollama = ollama::discover().await;
    let (codex, opencode, claude_code, claude_desktop, vscode) = (
        codex::discover(),
        opencode::discover(),
        claude_code::discover(),
        claude_desktop::discover(),
        vscode::discover(),
    );

    Discovery {
        agents: [codex, opencode, claude_code, claude_desktop, vscode]
            .into_iter()
            .flatten()
            .collect(),
        model_runtimes: ollama.into_iter().collect(),
    }
}
