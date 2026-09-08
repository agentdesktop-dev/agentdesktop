use crate::provider::{
    Provider, claude_code::ClaudeCode, claude_desktop::ClaudeDesktop, codex::Codex, ollama::Ollama,
    opencode::OpenCode, vscode::VsCode,
};
use agentdesktop_core::model::Discovery;

pub async fn discover() -> Discovery {
    let mut discovery = Ollama.discover().await;
    for found in [
        Codex::default().discover().await,
        OpenCode::default().discover().await,
        ClaudeCode::default().discover().await,
        ClaudeDesktop::default().discover().await,
        VsCode.discover().await,
    ] {
        discovery.agents.extend(found.agents);
        discovery.model_runtimes.extend(found.model_runtimes);
    }
    discovery
}
