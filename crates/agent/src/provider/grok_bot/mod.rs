use agentdesktop_core::model::Discovery;

use super::Provider;

mod discovery;

pub struct GrokBot;

impl GrokBot {
    pub const ID: &'static str = "grok-bot";
    pub const DISPLAY_NAME: &'static str = "Grok Bot";
    pub const PRODUCT_NAME: &'static str = "Grok Bot";
    pub const BUNDLE_IDENTIFIER: &'static str = "com.anysphere.sand";
}

#[async_trait::async_trait]
impl Provider for GrokBot {
    async fn discover(&self) -> Discovery {
        Discovery {
            agents: discovery::discover().into_iter().collect(),
            model_runtimes: Vec::new(),
        }
    }
}
