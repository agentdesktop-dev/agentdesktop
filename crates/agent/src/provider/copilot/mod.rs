use std::path::PathBuf;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::{DaemonScope, Discovery};

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
/// proxy's `/copilot-cli` route. A user daemon manages its own user's file; a
/// system daemon manages one user's file only when `daemon.copilot.config`
/// names it (`manage_for_user`), and has no path otherwise.
pub struct Copilot {
    pub providers_path: Option<PathBuf>,
    /// Set for a system daemon configured with `daemon.copilot.config`: manage
    /// that one user's file instead of skipping Copilot CLI as user-only.
    pub manage_for_user: bool,
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
        // A system daemon manages Copilot CLI only for a user whose file is configured.
        let for_user = !ctx.merge_user_settings && self.manage_for_user;
        if !for_user
            && let Some(plan) = ctx.skip_unless(
                DaemonScope::User,
                config.programs.copilot.is_some(),
                DISPLAY_NAME,
            )
        {
            return Ok(plan);
        }

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
        let Some(path) = self.providers_path.as_deref() else {
            if configured.is_some() {
                anyhow::bail!("{DISPLAY_NAME} user configuration path is unavailable");
            }
            return Ok(plan);
        };
        if for_user {
            // The CLI reads providers.json as the user: hand what the plan
            // creates to whoever owns the user's Copilot directory.
            crate::reconcile::plan::plan_for_user(&plan, &[path])?;
        }
        reconcile::plan(path, proxy, configured, &plan)?;
        Ok(plan)
    }
}
