use std::path::PathBuf;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::Discovery;

use super::{Provider, ReconcileContext};
use crate::reconcile::ReconcilePlan;

pub(super) mod discovery;
mod reconcile;

pub struct VsCode {
    pub settings_path: PathBuf,
}

impl Default for VsCode {
    fn default() -> Self {
        Self {
            settings_path: default_vscode_settings_path(),
        }
    }
}

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

    fn plan(
        &self,
        _ctx: &ReconcileContext,
        config: &DaemonConfig,
    ) -> anyhow::Result<ReconcilePlan> {
        let configured = config
            .programs
            .vscode
            .as_ref()
            .filter(|vscode| vscode.use_llm_gateway);
        let plan = ReconcilePlan::default();
        reconcile::plan(&self.settings_path, configured, &plan)?;
        Ok(plan)
    }
}

/// Returns the placeholder VS Code settings path used when running without `--user`.
pub fn default_vscode_settings_path() -> PathBuf {
    PathBuf::from("/etc/agentdesktop/vscode-settings.json")
}
