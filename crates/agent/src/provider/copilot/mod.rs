use std::path::PathBuf;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::Discovery;

use super::{Provider, ReconcileContext};
use crate::reconcile::ReconcilePlan;

pub(super) mod discovery;
pub(crate) mod reconcile;

#[cfg(test)]
mod tests;

/// Display name used in reconcile-plan reporting (dry-run output, conflict
/// and error messages).
pub(super) const DISPLAY_NAME: &str = "GitHub Copilot CLI";
/// Provider name for the OpenAI-shaped route (`/copilot-cli/v1`).
pub(super) const PROVIDER_OPENAI: &str = agentdesktop_core::config::CopilotConfig::PROVIDER_OPENAI;
/// Provider name for the Anthropic-shaped route (`/copilot-cli`).
pub(super) const PROVIDER_ANTHROPIC: &str =
    agentdesktop_core::config::CopilotConfig::PROVIDER_ANTHROPIC;

/// GitHub Copilot CLI: points the user's `providers.json` at the loopback LLM
/// proxy's `/copilot-cli` route. User mode only; `providers_path` is `None`
/// for a system daemon, which has no user file to manage.
pub struct Copilot {
    pub providers_path: Option<PathBuf>,
}

impl Copilot {
    pub const ID: &'static str = "copilot";
}

/// The user's `providers.json` from the daemon's environment
/// (`COPILOT_PROVIDERS_CONFIG`, `COPILOT_HOME`, then the home directory).
pub fn default_copilot_providers_path() -> anyhow::Result<PathBuf> {
    reconcile::providers_path(&|name| std::env::var_os(name))
}

#[async_trait::async_trait]
impl Provider for Copilot {
    fn id(&self) -> &'static str {
        Self::ID
    }

    async fn discover(&self) -> Discovery {
        Discovery {
            agents: discovery::discover().into_iter().collect(),
            model_runtimes: Vec::new(),
        }
    }

    fn plan(&self, ctx: &ReconcileContext, config: &DaemonConfig) -> anyhow::Result<ReconcilePlan> {
        let configured = config.programs.copilot.as_ref().map(|program| {
            let gateway = config
                .llm_gateway
                .as_ref()
                .filter(|_| program.use_llm_gateway);
            (program, gateway)
        });
        let proxy = ctx
            .llm_proxy
            .as_ref()
            .map(|proxy| (proxy.address, &*proxy.pairing));
        let plan = ReconcilePlan::default();
        // The inverse of Grok's check: the file lives in the user's home, so a
        // system daemon has no providers path and rejects the program before
        // any provider writes.
        let Some(path) = self
            .providers_path
            .as_deref()
            .filter(|_| ctx.merge_user_settings)
        else {
            if configured.is_some() {
                anyhow::bail!(
                    "{DISPLAY_NAME} reads its providers from the user's own Copilot directory; remove programs.copilot or run agentdesktop with --user (daemon.user: true) so it can manage that file"
                );
            }
            return Ok(plan);
        };
        reconcile::plan(path, proxy, configured, &plan)?;
        Ok(plan)
    }
}
