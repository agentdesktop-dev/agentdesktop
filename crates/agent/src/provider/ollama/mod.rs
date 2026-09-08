use agentdesktop_core::model::Discovery;

use super::Provider;

pub(super) mod discovery;

pub struct Ollama;

impl Provider for Ollama {
    fn id(&self) -> &'static str {
        "ollama"
    }
    fn display_name(&self) -> &'static str {
        "Ollama"
    }
    async fn discover(&self) -> Discovery {
        Discovery {
            agents: Vec::new(),
            model_runtimes: discovery::discover().await.into_iter().collect(),
        }
    }
}
