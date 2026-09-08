use std::path::PathBuf;

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_core::model::Discovery;

use super::shared::{CommandSpec, render_command};
use super::{Provider, ReconcileContext};
use crate::reconcile::ReconcilePlan;

pub(super) mod discovery;
mod reconcile;

#[derive(Clone)]
pub struct ClaudeCode {
    pub settings_path: PathBuf,
}

impl ClaudeCode {
    fn credential_helper_command(&self, ctx: &ReconcileContext) -> String {
        render_command(&CommandSpec::new(
            &ctx.credential_helper,
            [
                "--socket".to_owned(),
                ctx.socket.to_string_lossy().into_owned(),
                "credential".to_owned(),
                "--client-id".to_owned(),
                "claude-code".to_owned(),
            ],
        ))
    }

    fn hook_command(&self, ctx: &ReconcileContext, include_input: bool) -> CommandSpec {
        let mut args = vec![
            "--socket".to_owned(),
            ctx.socket.to_string_lossy().into_owned(),
            "hook".to_owned(),
            "claude-pre-tool-use".to_owned(),
        ];
        if include_input {
            args.push("--include-input".to_owned());
        }
        CommandSpec::new(&ctx.credential_helper, args)
    }

    fn session_hook_command(&self, ctx: &ReconcileContext) -> CommandSpec {
        CommandSpec::new(
            &ctx.credential_helper,
            [
                "--socket".to_owned(),
                ctx.socket.to_string_lossy().into_owned(),
                "hook".to_owned(),
                "claude-session-start".to_owned(),
            ],
        )
    }
}

impl Default for ClaudeCode {
    fn default() -> Self {
        Self {
            settings_path: default_claude_code_managed_settings_dir().join("50-agentdesktop.json"),
        }
    }
}

impl ClaudeCode {
    pub const ID: &'static str = "claude-code";
    pub const DISPLAY_NAME: &'static str = "Claude Code";
}

#[async_trait::async_trait]
impl Provider for ClaudeCode {
    async fn discover(&self) -> Discovery {
        Discovery {
            agents: discovery::discover().into_iter().collect(),
            model_runtimes: Vec::new(),
        }
    }

    fn plan(&self, ctx: &ReconcileContext, config: &DaemonConfig) -> anyhow::Result<ReconcilePlan> {
        let tool_use = config
            .telemetry
            .collects_tool_use()
            .then(|| self.hook_command(ctx, config.telemetry.includes_tool_input()));
        let session_new = config
            .telemetry
            .collects_session_new()
            .then(|| self.session_hook_command(ctx));
        let configured = config.programs.claude_code.as_ref().map(|provider| {
            let gateway = config
                .llm_gateway
                .as_ref()
                .filter(|_| provider.use_llm_gateway);
            (provider, gateway)
        });
        let plan = ReconcilePlan::default();
        reconcile::plan(
            &self.settings_path,
            ctx.merge_user_settings,
            &self.credential_helper_command(ctx),
            reconcile::Hooks::new(tool_use.as_ref(), session_new.as_ref()),
            config.sandbox.as_ref(),
            configured,
            &plan,
        )?;
        Ok(plan)
    }
}

#[cfg(target_os = "linux")]
pub fn default_claude_code_managed_settings_dir() -> PathBuf {
    PathBuf::from("/etc/claude-code/managed-settings.d")
}

#[cfg(target_os = "macos")]
pub fn default_claude_code_managed_settings_dir() -> PathBuf {
    PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.d")
}

#[cfg(target_os = "windows")]
pub fn default_claude_code_managed_settings_dir() -> PathBuf {
    let program_files = std::env::var_os("ProgramFiles")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
    program_files.join("ClaudeCode").join("managed-settings.d")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claude_hooks_keep_the_executable_and_arguments_separate() {
        let provider = ClaudeCode::default();
        let context = ReconcileContext {
            merge_user_settings: false,
            credential_helper: PathBuf::from(r"C:\Program Files\Agent Desktop\agentdesktop.exe"),
            socket: PathBuf::from(r"\\.\pipe\agentdesktop"),
        };

        let tool = provider.hook_command(&context, true);
        assert_eq!(
            tool.program,
            r"C:\Program Files\Agent Desktop\agentdesktop.exe"
        );
        assert_eq!(
            tool.args,
            [
                "--socket",
                r"\\.\pipe\agentdesktop",
                "hook",
                "claude-pre-tool-use",
                "--include-input",
            ]
        );
        assert_eq!(
            provider.session_hook_command(&context).args,
            [
                "--socket",
                r"\\.\pipe\agentdesktop",
                "hook",
                "claude-session-start",
            ]
        );
    }
}
