use agentdesktop_core::model::Discovery;

use super::Provider;

mod discovery;

pub struct Cursor;

impl Cursor {
    pub const ID: &'static str = "cursor";
    pub const DISPLAY_NAME: &'static str = "Cursor";
}

#[async_trait::async_trait]
impl Provider for Cursor {
    async fn discover(&self) -> Discovery {
        Discovery {
            agents: discovery::discover().into_iter().collect(),
            model_runtimes: Vec::new(),
        }
    }
}
