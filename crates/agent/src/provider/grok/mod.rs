use std::path::PathBuf;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::Discovery;

use super::{Provider, ReconcileContext};
use crate::reconcile::ReconcilePlan;

pub(super) mod discovery;
mod reconcile;

#[derive(Clone)]
pub struct Grok {
    pub managed_config_path: PathBuf,
}

impl Default for Grok {
    fn default() -> Self {
        Self {
            managed_config_path: default_grok_managed_config_path(),
        }
    }
}

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

    fn plan(&self, ctx: &ReconcileContext, config: &DaemonConfig) -> anyhow::Result<ReconcilePlan> {
        let configured = config.programs.grok.as_ref().map(|provider| {
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
            configured,
            &plan,
        )?;
        Ok(plan)
    }
}

/// Returns the system-wide Grok Build managed configuration path.
pub fn default_grok_managed_config_path() -> PathBuf {
    PathBuf::from("/etc/grok/managed_config.toml")
}
