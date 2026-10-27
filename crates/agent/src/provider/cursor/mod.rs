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
    fn id(&self) -> &'static str {
        Self::ID
    }

    async fn discover(&self) -> Discovery {
        Discovery {
            agents: discovery::discover().into_iter().collect(),
            model_runtimes: Vec::new(),
        }
    }
}
