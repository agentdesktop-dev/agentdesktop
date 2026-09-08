use agentdesktop_core::model::Discovery;

use super::Provider;

pub(super) mod discovery;

pub struct Ollama;

impl Ollama {
    pub const ID: &'static str = "ollama";
    pub const DISPLAY_NAME: &'static str = "Ollama";
}

#[async_trait::async_trait]
impl Provider for Ollama {
    async fn discover(&self) -> Discovery {
        Discovery {
            agents: Vec::new(),
            model_runtimes: discovery::discover().await.into_iter().collect(),
        }
    }
}
