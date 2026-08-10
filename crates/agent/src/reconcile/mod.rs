mod claude_code;

use std::path::PathBuf;

use agentplane_core::config::Config;

#[derive(Clone)]
pub struct Reconciler {
    claude_code_managed_settings_dir: PathBuf,
}

impl Reconciler {
    pub fn new(claude_code_managed_settings_dir: PathBuf) -> Self {
        Self {
            claude_code_managed_settings_dir,
        }
    }

    pub fn apply(&self, config: &Config) -> anyhow::Result<()> {
        claude_code::apply(
            &self.claude_code_managed_settings_dir,
            config.programs.claude_code.as_ref(),
        )
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

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn default_claude_code_managed_settings_dir() -> PathBuf {
    PathBuf::from("/etc/claude-code/managed-settings.d")
}
