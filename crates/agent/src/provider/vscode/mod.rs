use std::path::PathBuf;

use agentdesktop_core::config::{DaemonConfig, VsCodeCopilotChat};
use agentdesktop_core::model::Discovery;

use super::{Provider, ReconcileContext};
use crate::reconcile::ReconcilePlan;

pub(super) mod discovery;
pub(crate) mod reconcile;
pub(super) mod settings;

#[cfg(test)]
mod settings_tests;
#[cfg(test)]
mod tests;

/// VS Code: points Copilot Chat at the loopback LLM proxy, either through the
/// "Custom Endpoint" model provider (`chatLanguageModels.json`, own models on
/// the `/vscode-copilot` route) or through the Copilot Chat CAPI override in
/// the user `settings.json` (GitHub's models on the `/vscode-copilot-capi`
/// route). User mode only; both paths are `None` for a system daemon, which
/// has no user profile to manage.
pub struct VsCode {
    pub chat_models_path: Option<PathBuf>,
    pub settings_path: Option<PathBuf>,
}
impl VsCode {
    pub const ID: &'static str = "vscode";
    pub const DISPLAY_NAME: &'static str = "VS Code";
}

/// The user's `chatLanguageModels.json` for the resolved home directory
/// (per-OS VS Code user profile root).
pub fn default_vscode_chat_models_path(home: &std::path::Path) -> PathBuf {
    reconcile::chat_models_path(home)
}

/// The user's `settings.json` for the resolved home directory (per-OS VS Code
/// user profile root).
pub fn default_vscode_settings_path(home: &std::path::Path) -> PathBuf {
    settings::settings_path(home)
}

#[async_trait::async_trait]
impl Provider for VsCode {
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
        let configured = config.programs.vscode.as_ref().map(|program| {
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
        // Both files live in the user's VS Code profile, so a system daemon has
        // no paths and rejects the program before any provider writes.
        let paths = match (&self.chat_models_path, &self.settings_path) {
            (Some(chat_models), Some(settings)) if ctx.merge_user_settings => {
                Some((chat_models.as_path(), settings.as_path()))
            }
            _ => None,
        };
        let Some((chat_models_path, settings_path)) = paths else {
            if configured.is_some() {
                anyhow::bail!(
                    "{} Copilot Chat reads its custom models (chatLanguageModels.json) and the gateway override (settings.json) from the user's own VS Code profile; remove programs.vscode or run agentdesktop with --user (daemon.user: true) so it can manage those files",
                    Self::DISPLAY_NAME
                );
            }
            return Ok(plan);
        };
        // The two variants are exclusive: under `githubModels` the custom-models
        // entry is removed, under `ownModels` the settings override is removed.
        let own_models =
            configured.filter(|(program, _)| program.copilot_chat == VsCodeCopilotChat::OwnModels);
        reconcile::plan(chat_models_path, proxy, own_models, &plan)?;
        settings::plan(settings_path, proxy, configured, &plan)?;
        Ok(plan)
    }
}
