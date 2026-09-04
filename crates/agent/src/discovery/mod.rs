mod claude_code;
mod claude_desktop;
mod codex;
mod grok;
mod metadata;
mod ollama;
mod opencode;
mod vscode;

use agentdesktop_core::model::Discovery;

pub async fn discover() -> Discovery {
    let ollama = ollama::discover().await;
    let (codex, opencode, claude_code, claude_desktop, vscode, grok) = (
        codex::discover(),
        opencode::discover(),
        claude_code::discover(),
        claude_desktop::discover(),
        vscode::discover(),
        grok::discover(),
    );

    Discovery {
        agents: [codex, opencode, claude_code, claude_desktop, vscode, grok]
            .into_iter()
            .flatten()
            .collect(),
        model_runtimes: ollama.into_iter().collect(),
    }
}
