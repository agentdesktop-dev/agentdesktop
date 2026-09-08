use agentdesktop_core::model::Discovery;

use super::Provider;

pub(super) mod discovery;

pub struct VsCode;
impl Provider for VsCode {
    fn id(&self) -> &'static str {
        "vscode"
    }
    fn display_name(&self) -> &'static str {
        "VS Code"
    }
    async fn discover(&self) -> Discovery {
        Discovery {
            agents: discovery::discover().into_iter().collect(),
            model_runtimes: Vec::new(),
        }
    }
}
