use agentdesktop_core::model::Discovery;

use super::Provider;

mod discovery;

pub struct Grok;

impl Grok {
    pub const ID: &'static str = "grok";
    pub const DISPLAY_NAME: &'static str = "Grok Build";
}

#[async_trait::async_trait]
impl Provider for Grok {
    async fn discover(&self) -> Discovery {
        Discovery {
            agents: discovery::discover().into_iter().collect(),
            model_runtimes: Vec::new(),
        }
    }
}
