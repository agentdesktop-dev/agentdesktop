use std::path::{Path, PathBuf};

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::{DaemonScope, Discovery};

use super::{Provider, ReconcileContext};
use crate::reconcile::ReconcilePlan;

pub(super) mod discovery;
mod reconcile;

#[derive(Clone, Default)]
pub struct Muse {
    /// User path only; system daemons have no Muse Code file to manage.
    pub settings_path: Option<PathBuf>,
}

impl Muse {
    pub const ID: &'static str = "muse";
    pub const DISPLAY_NAME: &'static str = "Muse Code";
}

#[async_trait::async_trait]
impl Provider for Muse {
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
        if let Some(plan) = ctx.skip_unless(
            DaemonScope::User,
            config.programs.muse.is_some(),
            Self::DISPLAY_NAME,
        ) {
            return Ok(plan);
        }
        let plan = ReconcilePlan::default();
        let Some(settings_path) = &self.settings_path else {
            anyhow::bail!("Muse Code user configuration path is unavailable");
        };
        let configured = config.programs.muse.as_ref().map(|provider| {
            let gateway = config
                .llm_gateway
                .as_ref()
                .filter(|_| provider.use_llm_gateway);
            (provider, gateway)
        });
        reconcile::plan(
            settings_path,
            &ctx.credential_helper,
            &ctx.socket,
            configured,
            &plan,
        )?;
        Ok(plan)
    }
}

/// Muse Code's configuration root for a user: `$XDG_CONFIG_HOME/muse`, else
/// `~/.config/muse`. `config_home` is the already resolved XDG directory.
pub fn user_muse_settings_path(config_home: &Path) -> PathBuf {
    config_home.join("muse/settings.json")
}

#[cfg(test)]
mod tests {
    use agentdesktop_core::config::{DaemonConfig, parse_daemon};

    use super::{Muse, Provider, ReconcileContext};

    fn context(root: &std::path::Path, user: bool) -> ReconcileContext {
        ReconcileContext {
            llm_proxy: None,
            merge_user_settings: user,
            user_mode: user,
            credential_helper: root.join("agentdesktop"),
            socket: root.join("agentdesktop.sock"),
        }
    }

    #[test]
    fn skips_configured_muse_in_system_mode() {
        let root = tempfile::tempdir().unwrap();
        let muse = Muse {
            settings_path: Some(root.path().join("settings.json")),
        };
        let config = parse_daemon("programs:\n  muse: {}\n").unwrap();

        let plan = muse
            .plan(&context(root.path(), false), &config)
            .expect("Muse Code is skipped in system mode");
        assert!(plan.inactive_reason().unwrap().contains("--user"));
        plan.apply().unwrap();
        assert!(!muse.settings_path.as_ref().unwrap().exists());
    }

    #[test]
    fn system_mode_leaves_user_muse_files_untouched() {
        let root = tempfile::tempdir().unwrap();
        let muse = Muse {
            settings_path: Some(root.path().join("settings.json")),
        };
        let config = parse_daemon("programs:\n  muse:\n    model: selected\n").unwrap();
        muse.plan(&context(root.path(), true), &config)
            .unwrap()
            .apply()
            .unwrap();
        assert!(muse.settings_path.as_ref().unwrap().exists());

        muse.plan(&context(root.path(), false), &DaemonConfig::default())
            .unwrap()
            .apply()
            .unwrap();
        assert!(muse.settings_path.as_ref().unwrap().exists());
    }

    #[test]
    fn system_mode_needs_no_muse_path() {
        let root = tempfile::tempdir().unwrap();
        Muse::default()
            .plan(&context(root.path(), false), &DaemonConfig::default())
            .unwrap()
            .apply()
            .unwrap();
    }
}
