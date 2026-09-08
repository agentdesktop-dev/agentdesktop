use std::path::PathBuf;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::Discovery;

use super::{Provider, Reconcile, ReconcileContext};
use crate::reconcile::ReconcilePlan;

pub(super) mod discovery;
mod reconcile;

#[derive(Clone)]
pub struct OpenCode {
    pub managed_config_path: PathBuf,
    pub plugin_path: PathBuf,
}

impl Default for OpenCode {
    fn default() -> Self {
        Self {
            managed_config_path: default_open_code_managed_config_path(),
            plugin_path: default_open_code_plugin_path(),
        }
    }
}

impl Provider for OpenCode {
    fn id(&self) -> &'static str {
        "opencode"
    }
    fn display_name(&self) -> &'static str {
        "OpenCode"
    }
    async fn discover(&self) -> Discovery {
        Discovery {
            agents: discovery::discover().into_iter().collect(),
            model_runtimes: Vec::new(),
        }
    }
}

impl Reconcile for OpenCode {
    fn plan(&self, ctx: &ReconcileContext, config: &DaemonConfig) -> anyhow::Result<ReconcilePlan> {
        let configured = config.programs.open_code.as_ref().map(|provider| {
            let gateway = config
                .llm_gateway
                .as_ref()
                .filter(|_| provider.use_llm_gateway);
            (provider, gateway)
        });
        let plan = ReconcilePlan::default();
        reconcile::plan(
            &self.managed_config_path,
            &self.plugin_path,
            &ctx.credential_helper,
            &ctx.socket,
            configured,
            &plan,
        )?;
        Ok(plan)
    }
}

/// Returns the system-wide OpenCode managed configuration path.
pub fn default_open_code_managed_config_path() -> PathBuf {
    PathBuf::from("/etc/opencode/opencode.jsonc")
}

/// Returns the path of Agentdesktop's managed OpenCode credential plugin.
pub fn default_open_code_plugin_path() -> PathBuf {
    PathBuf::from("/etc/opencode/plugins/agentdesktop.js")
}
