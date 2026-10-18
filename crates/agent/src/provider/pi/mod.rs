use std::path::PathBuf;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::{DaemonScope, Discovery};

use super::{Provider, ReconcileContext};
use crate::reconcile::ReconcilePlan;

pub(super) mod discovery;
mod reconcile;

#[derive(Clone, Default)]
pub struct Pi {
    /// User paths only; system daemons have no Pi files to manage.
    pub models_path: Option<PathBuf>,
    pub settings_path: Option<PathBuf>,
}

impl Pi {
    pub const ID: &'static str = "pi";
    pub const DISPLAY_NAME: &'static str = "Pi";
    pub const PACKAGE_NAME: &'static str = "@earendil-works/pi-coding-agent";
}

#[async_trait::async_trait]
impl Provider for Pi {
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
            config.programs.pi.is_some(),
            Self::DISPLAY_NAME,
        ) {
            return Ok(plan);
        }
        let plan = ReconcilePlan::default();
        let (Some(models_path), Some(settings_path)) = (&self.models_path, &self.settings_path)
        else {
            anyhow::bail!("Pi user configuration paths are unavailable");
        };
        let configured = config.programs.pi.as_ref().map(|provider| {
            let gateway = config
                .llm_gateway
                .as_ref()
                .filter(|_| provider.use_llm_gateway);
            (provider, gateway)
        });
        reconcile::plan(
            models_path,
            settings_path,
            &ctx.credential_helper,
            &ctx.socket,
            configured,
            &plan,
        )?;
        Ok(plan)
    }
}

pub fn user_pi_agent_dir(home: &std::path::Path) -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|value| !value.is_empty())
        .map(|value| {
            let path = PathBuf::from(value);
            // Path components handle ~/ on every platform and ~\ on Windows,
            // matching Pi's platform-specific tilde expansion.
            match path.strip_prefix("~") {
                Ok(relative) => home.join(relative),
                Err(_) => path,
            }
        })
        .unwrap_or_else(|| home.join(".pi/agent"))
}

#[cfg(test)]
mod tests {
    use agentdesktop_core::config::{DaemonConfig, parse_daemon};

    use super::{Pi, Provider, ReconcileContext};

    #[test]
    fn skips_configured_pi_in_system_mode() {
        let root = tempfile::tempdir().unwrap();
        let pi = Pi {
            models_path: Some(root.path().join("models.json")),
            settings_path: Some(root.path().join("settings.json")),
        };
        let ctx = ReconcileContext {
            llm_proxy: None,
            merge_user_settings: false,
            credential_helper: root.path().join("agentdesktop"),
            socket: root.path().join("agentdesktop.sock"),
        };
        let config =
            parse_daemon("programs:\n  pi:\n    managedConfig:\n      providers: {}\n").unwrap();

        let plan = pi
            .plan(&ctx, &config)
            .expect("Pi is skipped in system mode");
        assert!(plan.inactive_reason().unwrap().contains("--user"));
        plan.apply().unwrap();
        assert!(!pi.models_path.as_ref().unwrap().exists());
        assert!(!pi.settings_path.as_ref().unwrap().exists());
    }

    #[test]
    fn system_mode_leaves_user_pi_files_untouched() {
        let root = tempfile::tempdir().unwrap();
        let pi = Pi {
            models_path: Some(root.path().join("models.json")),
            settings_path: Some(root.path().join("settings.json")),
        };
        let mut ctx = ReconcileContext {
            llm_proxy: None,
            merge_user_settings: true,
            credential_helper: root.path().join("agentdesktop"),
            socket: root.path().join("agentdesktop.sock"),
        };
        let config =
            parse_daemon("programs:\n  pi:\n    managedConfig:\n      providers: {}\n").unwrap();
        pi.plan(&ctx, &config).unwrap().apply().unwrap();
        assert!(pi.models_path.as_ref().unwrap().exists());

        ctx.merge_user_settings = false;
        pi.plan(&ctx, &DaemonConfig::default())
            .unwrap()
            .apply()
            .unwrap();
        assert!(pi.models_path.as_ref().unwrap().exists());
    }

    #[test]
    fn system_mode_needs_no_pi_paths() {
        let ctx = ReconcileContext {
            llm_proxy: None,
            merge_user_settings: false,
            credential_helper: "agentdesktop".into(),
            socket: "agentdesktop.sock".into(),
        };
        Pi::default()
            .plan(&ctx, &DaemonConfig::default())
            .unwrap()
            .apply()
            .unwrap();
    }
}
