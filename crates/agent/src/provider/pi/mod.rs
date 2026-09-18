use std::path::PathBuf;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::Discovery;

use super::{Provider, ReconcileContext};
use crate::reconcile::ReconcilePlan;

pub(super) mod discovery;
mod reconcile;

#[derive(Clone)]
pub struct Pi {
    pub models_path: PathBuf,
    pub settings_path: PathBuf,
}

impl Default for Pi {
    fn default() -> Self {
        Self {
            models_path: default_pi_models_path(),
            settings_path: default_pi_settings_path(),
        }
    }
}

impl Pi {
    pub const ID: &'static str = "pi";
    pub const DISPLAY_NAME: &'static str = "Pi";
    pub const PACKAGE_NAME: &'static str = "@earendil-works/pi-coding-agent";
}

#[async_trait::async_trait]
impl Provider for Pi {
    async fn discover(&self) -> Discovery {
        Discovery {
            agents: discovery::discover().into_iter().collect(),
            model_runtimes: Vec::new(),
        }
    }

    fn plan(&self, ctx: &ReconcileContext, config: &DaemonConfig) -> anyhow::Result<ReconcilePlan> {
        let configured = config.programs.pi.as_ref().map(|provider| {
            let gateway = config
                .llm_gateway
                .as_ref()
                .filter(|_| provider.use_llm_gateway);
            (provider, gateway)
        });
        let plan = ReconcilePlan::default();
        reconcile::plan(
            &self.models_path,
            &self.settings_path,
            &ctx.credential_helper,
            &ctx.socket,
            configured,
            &plan,
        )?;
        Ok(plan)
    }
}

/// Returns the system-wide Pi models.json path.
pub fn default_pi_models_path() -> PathBuf {
    pi_agent_dir(None).join("models.json")
}

/// Returns the system-wide Pi settings.json path.
pub fn default_pi_settings_path() -> PathBuf {
    pi_agent_dir(None).join("settings.json")
}

pub fn user_pi_agent_dir(home: &std::path::Path) -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".pi/agent"))
}

fn pi_agent_dir(home: Option<&std::path::Path>) -> PathBuf {
    if let Some(home) = home {
        return user_pi_agent_dir(home);
    }

    #[cfg(windows)]
    return windows_system_pi_agent_dir(
        std::env::var_os("SystemDrive").or_else(|| std::env::var_os("SYSTEMDRIVE")),
    );

    #[cfg(not(windows))]
    PathBuf::from("/etc/pi/agent")
}

#[cfg(any(windows, test))]
fn windows_system_pi_agent_dir(system_drive: Option<std::ffi::OsString>) -> PathBuf {
    let drive = system_drive
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "C:".into());
    let mut drive = drive
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_owned();
    if drive.len() == 1 && drive.as_bytes()[0].is_ascii_alphabetic() {
        drive.push(':');
    }
    if drive.is_empty() || !drive.ends_with(':') {
        drive = "C:".to_owned();
    }
    PathBuf::from(format!(r"{drive}\etc\pi\agent"))
}

#[cfg(test)]
mod tests {
    use super::windows_system_pi_agent_dir;

    #[test]
    fn windows_system_pi_agent_dir_uses_system_drive_root() {
        assert_eq!(
            windows_system_pi_agent_dir(Some("D:".into())),
            std::path::PathBuf::from(r"D:\etc\pi\agent")
        );
        assert_eq!(
            windows_system_pi_agent_dir(Some(r"E:\".into())),
            std::path::PathBuf::from(r"E:\etc\pi\agent")
        );
        assert_eq!(
            windows_system_pi_agent_dir(None),
            std::path::PathBuf::from(r"C:\etc\pi\agent")
        );
    }
}
