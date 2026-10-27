use std::path::PathBuf;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::{DaemonScope, Discovery};

use super::{Provider, ReconcileContext};
use crate::reconcile::ReconcilePlan;
#[cfg(windows)]
use crate::reconcile::registry::RegistryKey;

pub(super) mod discovery;
mod reconcile;

#[derive(Clone)]
pub struct ClaudeDesktop {
    pub managed_settings_path: PathBuf,
    pub credential_helper_path: PathBuf,
    /// The policy registry key the managed settings are reconciled into.
    /// Production uses [`claude_desktop_policy_key`]; tests inject a
    /// disposable `HKCU` key so they never touch live machine policy.
    #[cfg(windows)]
    pub(crate) policy_key: RegistryKey,
}

impl Default for ClaudeDesktop {
    fn default() -> Self {
        Self {
            managed_settings_path: default_claude_desktop_managed_settings_path(),
            credential_helper_path: default_claude_desktop_credential_helper_path(),
            #[cfg(windows)]
            policy_key: claude_desktop_policy_key(),
        }
    }
}

/// The live machine policy key Claude Desktop reads its managed settings
/// from on Windows.
#[cfg(windows)]
pub(crate) fn claude_desktop_policy_key() -> RegistryKey {
    RegistryKey::local_machine(reconcile::POLICY_KEY)
}

impl ClaudeDesktop {
    pub const ID: &'static str = "claude-desktop";
    pub const DISPLAY_NAME: &'static str = "Claude Desktop";
}

#[async_trait::async_trait]
impl Provider for ClaudeDesktop {
    fn id(&self) -> &'static str {
        Self::ID
    }

    async fn discover(&self) -> Discovery {
        Discovery {
            agents: discovery::discover(&self.managed_settings_path)
                .into_iter()
                .collect(),
            model_runtimes: Vec::new(),
        }
    }

    fn plan(&self, ctx: &ReconcileContext, config: &DaemonConfig) -> anyhow::Result<ReconcilePlan> {
        if let Some(plan) = ctx.skip_unless(
            DaemonScope::System,
            config.programs.claude_desktop.is_some(),
            Self::DISPLAY_NAME,
        ) {
            return Ok(plan);
        }
        let configured = config.programs.claude_desktop.as_ref().map(|provider| {
            let gateway = config
                .llm_gateway
                .as_ref()
                .filter(|_| provider.use_llm_gateway);
            (provider, gateway)
        });
        let plan = ReconcilePlan::default();
        reconcile::plan(
            self,
            ctx.merge_user_settings,
            &ctx.credential_helper,
            &ctx.socket,
            configured,
            &plan,
        )?;
        Ok(plan)
    }
}

/// Returns Claude Desktop's system-managed settings path.
///
/// On Windows Claude Desktop reads its managed configuration from the
/// `HKLM\SOFTWARE\Policies\Claude` registry key, never from a file, so the
/// reconciler targets that key and only removes what an earlier agentdesktop
/// wrote at this path.
#[cfg(not(target_os = "macos"))]
pub fn default_claude_desktop_managed_settings_path() -> PathBuf {
    PathBuf::from("/etc/claude-desktop/managed-settings.json")
}

/// Returns Claude Desktop's system-managed settings path.
///
/// Claude Desktop on macOS reads its managed configuration the same way as any other
/// managed macOS application: via `CFPreferencesCopyAppValue` against the
/// `com.anthropic.claudefordesktop` preference domain, backed by a property list under
/// `/Library/Managed Preferences/`. It never reads a JSON file under `/etc`, so this must
/// be a `.plist` and the reconciler must write it with `plist::to_writer_xml` — see
/// `reconcile::serialize_managed_settings`.
#[cfg(target_os = "macos")]
pub fn default_claude_desktop_managed_settings_path() -> PathBuf {
    PathBuf::from("/Library/Managed Preferences/com.anthropic.claudefordesktop.plist")
}

/// Returns the path of Agentdesktop's Claude Desktop credential helper.
pub fn default_claude_desktop_credential_helper_path() -> PathBuf {
    #[cfg(windows)]
    return std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("AgentDesktop/claude-desktop-credential-helper.cmd");
    #[cfg(not(windows))]
    return PathBuf::from("/etc/claude-desktop/agentdesktop-credential-helper");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claude_desktop_helper_default_matches_the_native_script_type() {
        #[cfg(windows)]
        {
            assert_eq!(
                default_claude_desktop_credential_helper_path().extension(),
                Some(std::ffi::OsStr::new("cmd"))
            );
        }
        #[cfg(not(windows))]
        {
            assert_eq!(
                default_claude_desktop_credential_helper_path(),
                PathBuf::from("/etc/claude-desktop/agentdesktop-credential-helper")
            );
        }
    }
}
