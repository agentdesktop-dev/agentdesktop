mod claude_code;
mod claude_desktop;
mod codex;
mod open_code;

use std::path::PathBuf;

use agentdesktop_core::config::{DaemonConfig, InferenceGatewayConfig};
use serde_json::Value;

#[derive(Clone)]
pub struct Reconciler {
    claude_code_managed_settings_dir: PathBuf,
    claude_desktop_managed_settings_path: PathBuf,
    claude_desktop_credential_helper_path: PathBuf,
    codex_managed_config_path: PathBuf,
    open_code_managed_config_path: PathBuf,
    open_code_plugin_path: PathBuf,
    credential_helper: PathBuf,
    socket: PathBuf,
}

impl Reconciler {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        claude_code_managed_settings_dir: PathBuf,
        claude_desktop_managed_settings_path: PathBuf,
        claude_desktop_credential_helper_path: PathBuf,
        codex_managed_config_path: PathBuf,
        open_code_managed_config_path: PathBuf,
        open_code_plugin_path: PathBuf,
        credential_helper: PathBuf,
        socket: PathBuf,
    ) -> Self {
        Self {
            claude_code_managed_settings_dir,
            claude_desktop_managed_settings_path,
            claude_desktop_credential_helper_path,
            codex_managed_config_path,
            open_code_managed_config_path,
            open_code_plugin_path,
            credential_helper,
            socket,
        }
    }

    pub fn apply(&self, config: &DaemonConfig) -> anyhow::Result<()> {
        let tool_use_hook = config
            .telemetry
            .collects_tool_use()
            .then(|| self.claude_hook_command(config.telemetry.includes_tool_input()));
        let session_new_hook = config
            .telemetry
            .collects_session_new()
            .then(|| self.claude_session_hook_command());
        let claude_code = config.programs.claude_code.as_ref().map(|claude_code| {
            let gateway = config
                .inference_gateway
                .as_ref()
                .filter(|_| claude_code.use_inference_gateway);
            (claude_code, gateway)
        });
        claude_code::apply(
            &self.claude_code_managed_settings_dir,
            &self.claude_credential_helper_command(),
            tool_use_hook.as_deref(),
            session_new_hook.as_deref(),
            claude_code,
        )?;
        let claude_desktop = config.programs.claude_desktop.as_ref().map(|desktop| {
            let gateway = config
                .inference_gateway
                .as_ref()
                .filter(|_| desktop.use_inference_gateway);
            (desktop, gateway)
        });
        claude_desktop::apply(
            &self.claude_desktop_managed_settings_path,
            &self.claude_desktop_credential_helper_path,
            &self.credential_helper,
            &self.socket,
            claude_desktop,
        )?;
        let codex = config.programs.codex.as_ref().map(|codex| {
            let gateway = config
                .inference_gateway
                .as_ref()
                .filter(|_| codex.use_inference_gateway);
            (codex, gateway)
        });
        codex::apply(
            &self.codex_managed_config_path,
            &self.credential_helper,
            &self.socket,
            codex,
        )?;
        let open_code = config.programs.open_code.as_ref().map(|open_code| {
            let gateway = config
                .inference_gateway
                .as_ref()
                .filter(|_| open_code.use_inference_gateway);
            (open_code, gateway)
        });
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

    fn claude_hook_command(&self, include_input: bool) -> String {
        let mut command = format!(
            "{} --socket {} hook claude-pre-tool-use",
            shell_quote(&self.credential_helper.to_string_lossy()),
            shell_quote(&self.socket.to_string_lossy())
        );
        if include_input {
            command.push_str(" --include-input");
        }
        command
    }

    fn claude_session_hook_command(&self) -> String {
        format!(
            "{} --socket {} hook claude-session-start",
            shell_quote(&self.credential_helper.to_string_lossy()),
            shell_quote(&self.socket.to_string_lossy())
        )
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                deep_merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn responses_base_url(gateway: &InferenceGatewayConfig) -> String {
    let mut url = gateway.url.clone();
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/v1") {
        url.set_path(&format!("{path}/v1"));
    }
    url.to_string().trim_end_matches('/').to_owned()
}

#[cfg(target_os = "linux")]
pub fn default_claude_code_managed_settings_dir() -> PathBuf {
    PathBuf::from("/etc/claude-code/managed-settings.d")
}

/// Returns the system-wide Codex managed configuration path.
pub fn default_codex_managed_config_path() -> PathBuf {
    PathBuf::from("/etc/codex/managed_config.toml")
}

/// Returns Claude Desktop's system-managed settings path.
pub fn default_claude_desktop_managed_settings_path() -> PathBuf {
    PathBuf::from("/etc/claude-desktop/managed-settings.json")
}

/// Returns the path of Agentdesktop's Claude Desktop credential helper.
pub fn default_claude_desktop_credential_helper_path() -> PathBuf {
    PathBuf::from("/etc/claude-desktop/agentdesktop-credential-helper")
}

/// Returns the system-wide OpenCode managed configuration path.
pub fn default_open_code_managed_config_path() -> PathBuf {
    PathBuf::from("/etc/opencode/opencode.jsonc")
}

/// Returns the path of Agentdesktop's managed OpenCode credential plugin.
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
