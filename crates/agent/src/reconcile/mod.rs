mod claude_code;
mod codex;
mod open_code;

use std::path::PathBuf;

use agentdesktop_core::config::DesiredConfig;

#[derive(Clone)]
pub struct Reconciler {
    claude_code_managed_settings_dir: PathBuf,
    codex_managed_config_path: PathBuf,
    open_code_managed_config_path: PathBuf,
    open_code_plugin_path: PathBuf,
    credential_helper: PathBuf,
    socket: PathBuf,
}

impl Reconciler {
    pub fn new(
        claude_code_managed_settings_dir: PathBuf,
        codex_managed_config_path: PathBuf,
        open_code_managed_config_path: PathBuf,
        open_code_plugin_path: PathBuf,
        credential_helper: PathBuf,
        socket: PathBuf,
    ) -> Self {
        Self {
            claude_code_managed_settings_dir,
            codex_managed_config_path,
            open_code_managed_config_path,
            open_code_plugin_path,
            credential_helper,
            socket,
        }
    }

    pub fn apply(&self, config: &DesiredConfig) -> anyhow::Result<()> {
        let claude_code = config
            .programs
            .claude_code
            .as_ref()
            .map(|claude_code| (claude_code, config.inference_gateway.as_ref()));
        claude_code::apply(
            &self.claude_code_managed_settings_dir,
            &self.claude_credential_helper_command(),
            claude_code,
        )?;
        let codex = config
            .programs
            .codex
            .as_ref()
            .map(|codex| (codex, config.inference_gateway.as_ref()));
        codex::apply(
            &self.codex_managed_config_path,
            &self.credential_helper,
            &self.socket,
            codex,
        )?;
        let open_code = config
            .programs
            .open_code
            .as_ref()
            .map(|open_code| (open_code, config.inference_gateway.as_ref()));
        open_code::apply(
            &self.open_code_managed_config_path,
            &self.open_code_plugin_path,
            &self.credential_helper,
            &self.socket,
            open_code,
        )
    }

    fn claude_credential_helper_command(&self) -> String {
        format!(
            "{} --socket {} credential --client-id claude-code",
            shell_quote(&self.credential_helper.to_string_lossy()),
            shell_quote(&self.socket.to_string_lossy())
        )
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(target_os = "linux")]
pub fn default_claude_code_managed_settings_dir() -> PathBuf {
    PathBuf::from("/etc/claude-code/managed-settings.d")
}

/// Returns the system-wide Codex managed configuration path.
pub fn default_codex_managed_config_path() -> PathBuf {
    PathBuf::from("/etc/codex/managed_config.toml")
}

/// Returns the system-wide OpenCode managed configuration path.
pub fn default_open_code_managed_config_path() -> PathBuf {
    PathBuf::from("/etc/opencode/opencode.jsonc")
}

/// Returns the path of AgentDesktop's managed OpenCode credential plugin.
pub fn default_open_code_plugin_path() -> PathBuf {
    PathBuf::from("/etc/opencode/plugins/agentdesktop.js")
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

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn default_claude_code_managed_settings_dir() -> PathBuf {
    PathBuf::from("/etc/claude-code/managed-settings.d")
}
