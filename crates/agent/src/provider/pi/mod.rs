use std::path::PathBuf;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::{DaemonScope, Discovery};

use super::{Provider, ReconcileContext};
use crate::reconcile::ReconcilePlan;

pub(super) mod discovery;
mod reconcile;

#[derive(Clone, Default)]
pub struct Pi {
    /// User daemons always have these. A system daemon has them only when
    /// `daemon.pi.models` and `daemon.pi.settings` name one user's files.
    pub models_path: Option<PathBuf>,
    pub settings_path: Option<PathBuf>,
    /// Set for a system daemon configured with `daemon.pi` paths: manage that
    /// one user's Pi files instead of skipping Pi as user-only.
    pub manage_for_user: bool,
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
        // A system daemon manages Pi only for a user whose files are configured.
        let for_user = !ctx.merge_user_settings && self.manage_for_user;
        if !for_user
            && let Some(plan) = ctx.skip_unless(
                DaemonScope::User,
                config.programs.pi.is_some(),
                Self::DISPLAY_NAME,
            )
        {
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
        if for_user {
            // Pi reads these as the user, so files and any directories the plan
            // creates belong to whoever owns the user's Pi directory.
            plan.set_owner(owner_of_nearest(models_path));
        }
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

/// Owner of the closest existing ancestor of `path`, so a system daemon can
/// hand Pi's files to the user they belong to. `None` off Unix or if nothing
/// along the path can be inspected.
fn owner_of_nearest(path: &std::path::Path) -> Option<crate::reconcile::plan::FileOwner> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        path.ancestors()
            .skip(1)
            .find_map(|dir| std::fs::metadata(dir).ok())
            .map(|metadata| crate::reconcile::plan::FileOwner {
                uid: metadata.uid(),
                gid: metadata.gid(),
            })
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
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
        // User paths alone do not opt a system daemon in (daemon.pi does).
        let pi = Pi {
            models_path: Some(root.path().join("models.json")),
            settings_path: Some(root.path().join("settings.json")),
            manage_for_user: false,
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
        assert!(!root.path().join("models.json").exists());
        assert!(!root.path().join("settings.json").exists());
    }

    #[test]
    fn system_mode_leaves_user_pi_files_untouched() {
        let root = tempfile::tempdir().unwrap();
        let pi = Pi {
            models_path: Some(root.path().join("models.json")),
            settings_path: Some(root.path().join("settings.json")),
            manage_for_user: false,
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
        Pi::default()
            .plan(&ctx, &DaemonConfig::default())
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

    #[test]
    fn system_mode_manages_a_configured_users_pi_files() {
        let root = tempfile::tempdir().unwrap();
        let agent_dir = root.path().join("home/dev/.pi/agent");
        let pi = Pi {
            models_path: Some(agent_dir.join("models.json")),
            settings_path: Some(agent_dir.join("settings.json")),
            manage_for_user: true,
        };
        let ctx = ReconcileContext {
            llm_proxy: None,
            merge_user_settings: false,
            credential_helper: root.path().join("agentdesktop"),
            socket: root.path().join("agentdesktop.sock"),
        };
        let config =
            parse_daemon("programs:\n  pi:\n    managedConfig:\n      providers: {}\n").unwrap();

        let plan = pi.plan(&ctx, &config).unwrap();
        assert!(plan.inactive_reason().is_none());
        plan.apply().unwrap();
        assert!(pi.models_path.as_ref().unwrap().exists());

        // Files and the directories created for them belong to the owner of the
        // nearest existing ancestor (here the test user).
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let owner = std::fs::metadata(root.path()).unwrap();
            for path in [pi.models_path.as_ref().unwrap(), &agent_dir] {
                let metadata = std::fs::metadata(path).unwrap();
                assert_eq!((metadata.uid(), metadata.gid()), (owner.uid(), owner.gid()));
            }
        }
    }
}
