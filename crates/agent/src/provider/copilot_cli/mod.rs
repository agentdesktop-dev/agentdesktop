//! Opt-in native GitHub Copilot CLI routing. No native configuration is reconciled.

use agentdesktop_core::model::Discovery;

use super::Provider;

mod discovery;
mod launch;

pub(crate) use launch::{credential, launch};

#[derive(Clone, Default)]
pub struct CopilotCli;

impl CopilotCli {
    pub const ID: &'static str = "copilot-cli";
    pub const DISPLAY_NAME: &'static str = "GitHub Copilot CLI";
}

#[async_trait::async_trait]
impl Provider for CopilotCli {
    async fn discover(&self) -> Discovery {
        // Inventory never executes Copilot, including login or inference commands.
        Discovery {
            agents: discovery::discover().into_iter().collect(),
            model_runtimes: Vec::new(),
        }
    }
}
