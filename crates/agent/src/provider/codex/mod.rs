use std::path::PathBuf;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::Discovery;

use super::{Provider, ReconcileContext};
use crate::reconcile::ReconcilePlan;

pub(super) mod discovery;
mod reconcile;

#[derive(Clone)]
pub struct Codex {
    pub managed_config_path: PathBuf,
}

impl Default for Codex {
    fn default() -> Self {
        Self {
            managed_config_path: default_codex_managed_config_path(),
        }
    }
}

impl Codex {
    pub const ID: &'static str = "codex";
    pub const DISPLAY_NAME: &'static str = "Codex";
}

#[async_trait::async_trait]
impl Provider for Codex {
    async fn discover(&self) -> Discovery {
        Discovery {
            agents: discovery::discover().into_iter().collect(),
            model_runtimes: Vec::new(),
        }
    }

    fn plan(&self, ctx: &ReconcileContext, config: &DaemonConfig) -> anyhow::Result<ReconcilePlan> {
        let configured = config.programs.codex.as_ref().map(|provider| {
            let gateway = config
                .llm_gateway
                .as_ref()
                .filter(|_| provider.use_llm_gateway);
            (provider, gateway)
        });
        let plan = ReconcilePlan::default();
        reconcile::plan(
            &self.managed_config_path,
            &ctx.credential_helper,
            &ctx.socket,
            config.sandbox.as_ref(),
            configured,
            &plan,
        )?;
        Ok(plan)
    }
}

/// Returns the system-wide Codex managed configuration path.
pub fn default_codex_managed_config_path() -> PathBuf {
    PathBuf::from("/etc/codex/managed_config.toml")
}
