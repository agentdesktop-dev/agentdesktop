mod claude_code;
mod claude_desktop;
mod codex;
mod metadata;
mod opencode;
mod vscode;

use agentdesktop_core::model::Discovery;

pub async fn discover() -> Discovery {
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
    }
}
