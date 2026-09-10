use agentdesktop_core::model::Discovery;

use super::Provider;

pub(super) mod discovery;

pub struct VsCode;
impl VsCode {
    pub const ID: &'static str = "vscode";
    pub const DISPLAY_NAME: &'static str = "VS Code";
}

#[async_trait::async_trait]
impl Provider for VsCode {
    async fn discover(&self) -> Discovery {
        Discovery {
            agents: discovery::discover().into_iter().collect(),
            model_runtimes: Vec::new(),
        }
    }
}
