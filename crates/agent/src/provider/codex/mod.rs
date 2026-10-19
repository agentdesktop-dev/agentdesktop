use std::path::PathBuf;

use agentdesktop_core::config::{DaemonConfig, ProgramAuthentication};
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
        if !ctx.merge_user_settings
            && config
                .programs
                .codex
                .as_ref()
                .is_some_and(|program| program.auth == Some(ProgramAuthentication::Subscription))
        {
            anyhow::bail!(
                "programs.codex.auth subscription requires --user (or daemon.user: true)"
            );
        }
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
            ctx.llm_proxy
                .as_ref()
                .map(|proxy| (proxy.address, &*proxy.pairing)),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_requires_a_user_mode_daemon() {
        let config = agentdesktop_core::config::parse_daemon(
            "llmGateway:\n  url: https://gateway.example.com\n  proxyUrl: https://gateway.example.com/codex\n  authentication:\n    type: controllerJwt\n    audience: gateway\n    allowedClientIds: [codex]\nprograms:\n  codex:\n    auth: subscription\n",
        )
        .unwrap();
        let context = ReconcileContext {
            merge_user_settings: false,
            credential_helper: PathBuf::from("agentdesktop"),
            socket: PathBuf::from("socket"),
            llm_proxy: None,
        };
        let error = Codex::default().plan(&context, &config).err().unwrap();
        assert!(error.to_string().contains("requires --user"));
    }
}
