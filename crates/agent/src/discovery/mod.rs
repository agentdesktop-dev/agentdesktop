mod claude_code;
mod claude_desktop;
mod codex;
mod context;
mod files;
mod mcp;
mod metadata;
mod ollama;
mod opencode;
mod vscode;

use agentdesktop_core::model::{Agent, Discovery};

use context::ScanContext;

type HarnessDiscovery = fn(&ScanContext) -> Option<Agent>;

// Adapters own native semantics; the registry preserves inventory order.
const HARNESSES: &[HarnessDiscovery] = &[
    codex::discover,
    opencode::discover,
    claude_code::discover,
    claude_desktop::discover,
    vscode::discover,
];

pub async fn discover() -> Discovery {
    let context = ScanContext::capture();
    let ollama = ollama::discover().await;

    Discovery {
        agents: discover_harnesses(&context),
        model_runtimes: ollama.into_iter().collect(),
    }
}

fn discover_harnesses(context: &ScanContext) -> Vec<Agent> {
    HARNESSES
        .iter()
        .filter_map(|discover| discover(context))
        .collect()
}

#[cfg(test)]
mod tests;
