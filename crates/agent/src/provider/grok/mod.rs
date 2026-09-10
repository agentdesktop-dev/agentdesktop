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
        if ctx.merge_user_settings && config.programs.grok.is_some() {
            anyhow::bail!(
                "Grok Build can delete or replace its user-level managed_config.toml during startup; remove programs.grok or run agentdesktop in system mode so it can manage Grok's system managed_config.toml"
            );
        }
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
    #[cfg(windows)]
    return windows_system_managed_config_path(
        std::env::var_os("SystemDrive").or_else(|| std::env::var_os("SYSTEMDRIVE")),
    );

    #[cfg(not(windows))]
    PathBuf::from("/etc/grok/managed_config.toml")
}

#[cfg(any(windows, test))]
fn windows_system_managed_config_path(system_drive: Option<std::ffi::OsString>) -> PathBuf {
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
    PathBuf::from(format!(r"{drive}\etc\grok\managed_config.toml"))
}

#[cfg(test)]
mod tests {
    use super::windows_system_managed_config_path;

    #[test]
    fn windows_system_managed_config_path_uses_system_drive_root() {
        assert_eq!(
            windows_system_managed_config_path(Some("D:".into())),
            std::path::PathBuf::from(r"D:\etc\grok\managed_config.toml")
        );
        assert_eq!(
            windows_system_managed_config_path(Some(r"E:\".into())),
            std::path::PathBuf::from(r"E:\etc\grok\managed_config.toml")
        );
        assert_eq!(
            windows_system_managed_config_path(None),
            std::path::PathBuf::from(r"C:\etc\grok\managed_config.toml")
        );
    }
}
