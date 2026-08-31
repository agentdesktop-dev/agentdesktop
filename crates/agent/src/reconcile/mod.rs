mod claude_code;
mod claude_desktop;
mod codex;
mod json_merge;
mod open_code;

use std::{
    cell::RefCell,
    fmt::Write,
    path::{Path, PathBuf},
};

use agentdesktop_core::config::{DaemonConfig, LlmGatewayConfig};
#[cfg(any(windows, test))]
use base64::{Engine as _, prelude::BASE64_STANDARD};
use serde_json::Value;
use similar::TextDiff;

#[derive(Clone, Debug, PartialEq, Eq)]
struct CommandSpec {
    program: String,
    args: Vec<String>,
}

impl CommandSpec {
    fn new(program: &Path, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            program: program.to_string_lossy().into_owned(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Clone, Copy)]
enum ReconcileMode<'a> {
    Apply,
    DryRun(&'a DryRunReport),
}

impl ReconcileMode<'_> {
    fn writes(self) -> bool {
        matches!(self, Self::Apply)
    }

    fn record(self, program: &str, description: &str, action: &str, path: &Path) {
        if let Self::DryRun(report) = self {
            let before = (action == "remove")
                .then(|| std::fs::read(path).ok())
                .flatten();
            report.record(program, description, action, path, before.as_deref(), None);
        }
    }

    fn record_diff(
        self,
        program: &str,
        description: &str,
        action: &str,
        path: &Path,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) {
        if let Self::DryRun(report) = self {
            report.record(program, description, action, path, before, after);
        }
    }

    fn is_dry_run(self) -> bool {
        matches!(self, Self::DryRun(_))
    }
}

#[derive(Default)]
struct DryRunReport {
    changes: RefCell<Vec<DryRunChange>>,
}

struct DryRunChange {
    program: String,
    description: String,
    action: String,
    path: PathBuf,
    before: Option<String>,
    after: Option<String>,
}

impl DryRunReport {
    fn record(
        &self,
        program: &str,
        description: &str,
        action: &str,
        path: &Path,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) {
        self.changes.borrow_mut().push(DryRunChange {
            program: program.to_owned(),
            description: description.to_owned(),
            action: action.to_owned(),
            path: path.to_owned(),
            before: before.map(|value| String::from_utf8_lossy(value).into_owned()),
            after: after.map(|value| String::from_utf8_lossy(value).into_owned()),
        });
    }

    fn render(&self) -> String {
        let changes = self.changes.borrow();
        let changed = changes
            .iter()
            .filter(|change| change.action != "unchanged" && change.action != "conflict")
            .count();
        let unchanged = changes
            .iter()
            .filter(|change| change.action == "unchanged")
            .count();
        let conflicts = changes
            .iter()
            .filter(|change| change.action == "conflict")
            .count();
        let mut output = String::from("Dry run — no files will be changed\n");

        for change in changes.iter().filter(|change| change.action != "unchanged") {
            let _ = write!(
                output,
                "\n{}  {} {}\n        {}\n",
                change.action.to_uppercase(),
                program_name(&change.program),
                change.description,
                change.path.display()
            );
            if change.before.is_some() || change.after.is_some() {
                let (before, after) = normalized_diff(
                    change.before.as_deref().unwrap_or(""),
                    change.after.as_deref().unwrap_or(""),
                );
                let diff = TextDiff::from_lines(&before, &after)
                    .unified_diff()
                    .context_radius(3)
                    .header("current", "proposed")
                    .to_string();
                if !diff.is_empty() {
                    let _ = write!(output, "{diff}");
                }
            }
        }

        let noun = if changed == 1 { "change" } else { "changes" };
        let _ = write!(output, "\nSummary: {changed} {noun}, {unchanged} unchanged");
        if conflicts > 0 {
            let _ = write!(output, ", {conflicts} conflicts");
        }
        output.push('\n');
        output
    }
}

fn normalized_diff(before: &str, after: &str) -> (String, String) {
    match (
        serde_json::from_str::<Value>(before),
        serde_json::from_str::<Value>(after),
    ) {
        (Ok(before), Ok(after)) => (
            format!(
                "{}\n",
                serde_json::to_string_pretty(&before).expect("JSON value serializes")
            ),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&after).expect("JSON value serializes")
            ),
        ),
        _ => (before.to_owned(), after.to_owned()),
    }
}

fn program_name(program: &str) -> &str {
    match program {
        "claude-code" => "Claude Code",
        "claude-desktop" => "Claude Desktop",
        "codex" => "Codex",
        "opencode" => "OpenCode",
        program => program,
    }
}

#[derive(Clone)]
pub struct Reconciler {
    merge_user_settings: bool,
    claude_code_settings_path: PathBuf,
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
        merge_user_settings: bool,
        claude_code_settings_path: PathBuf,
        claude_desktop_managed_settings_path: PathBuf,
        claude_desktop_credential_helper_path: PathBuf,
        codex_managed_config_path: PathBuf,
        open_code_managed_config_path: PathBuf,
        open_code_plugin_path: PathBuf,
        credential_helper: PathBuf,
        socket: PathBuf,
    ) -> Self {
        Self {
            merge_user_settings,
            claude_code_settings_path,
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
        self.reconcile(config, ReconcileMode::Apply)
    }

    pub fn dry_run(&self, config: &DaemonConfig) -> anyhow::Result<()> {
        let report = DryRunReport::default();
        self.reconcile(config, ReconcileMode::DryRun(&report))?;
        print!("{}", report.render());
        Ok(())
    }

    fn reconcile(&self, config: &DaemonConfig, mode: ReconcileMode<'_>) -> anyhow::Result<()> {
        if self.merge_user_settings && config.programs.claude_desktop.is_some() {
            anyhow::bail!(
                "Claude Desktop does not read inference settings from its user preferences; remove programs.claudeDesktop or run Agentdesktop without --user as root so it can manage /etc/claude-desktop/managed-settings.json"
            );
        }
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
                .llm_gateway
                .as_ref()
                .filter(|_| claude_code.use_llm_gateway);
            (claude_code, gateway)
        });
        claude_code::apply(
            &self.claude_code_settings_path,
            self.merge_user_settings,
            &self.claude_credential_helper_command(),
            tool_use_hook.as_ref(),
            session_new_hook.as_ref(),
            config.sandbox.as_ref(),
            claude_code,
            mode,
        )?;
        let claude_desktop = config.programs.claude_desktop.as_ref().map(|desktop| {
            let gateway = config
                .llm_gateway
                .as_ref()
                .filter(|_| desktop.use_llm_gateway);
            (desktop, gateway)
        });
        claude_desktop::apply(
            &self.claude_desktop_managed_settings_path,
            self.merge_user_settings,
            &self.claude_desktop_credential_helper_path,
            &self.credential_helper,
            &self.socket,
            claude_desktop,
            mode,
        )?;
        let codex = config.programs.codex.as_ref().map(|codex| {
            let gateway = config
                .llm_gateway
                .as_ref()
                .filter(|_| codex.use_llm_gateway);
            (codex, gateway)
        });
        codex::apply(
            &self.codex_managed_config_path,
            &self.credential_helper,
            &self.socket,
            config.sandbox.as_ref(),
            codex,
            mode,
        )?;
        let open_code = config.programs.open_code.as_ref().map(|open_code| {
            let gateway = config
                .llm_gateway
                .as_ref()
                .filter(|_| open_code.use_llm_gateway);
            (open_code, gateway)
        });
        open_code::apply(
            &self.open_code_managed_config_path,
            &self.open_code_plugin_path,
            &self.credential_helper,
            &self.socket,
            open_code,
            mode,
        )
    }

    fn claude_credential_helper_command(&self) -> String {
        render_command(&CommandSpec::new(
            &self.credential_helper,
            [
                "--socket".to_owned(),
                self.socket.to_string_lossy().into_owned(),
                "credential".to_owned(),
                "--client-id".to_owned(),
                "claude-code".to_owned(),
            ],
        ))
    }

    fn claude_hook_command(&self, include_input: bool) -> CommandSpec {
        let mut args = vec![
            "--socket".to_owned(),
            self.socket.to_string_lossy().into_owned(),
            "hook".to_owned(),
            "claude-pre-tool-use".to_owned(),
        ];
        if include_input {
            args.push("--include-input".to_owned());
        }
        CommandSpec::new(&self.credential_helper, args)
    }

    fn claude_session_hook_command(&self) -> CommandSpec {
        CommandSpec::new(
            &self.credential_helper,
            [
                "--socket".to_owned(),
                self.socket.to_string_lossy().into_owned(),
                "hook".to_owned(),
                "claude-session-start".to_owned(),
            ],
        )
    }
}

#[cfg(any(not(windows), test))]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(any(windows, test))]
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn render_command(command: &CommandSpec) -> String {
    #[cfg(windows)]
    return render_windows_command(command);
    #[cfg(not(windows))]
    return render_posix_command(command);
}

#[cfg(any(not(windows), test))]
fn render_posix_command(command: &CommandSpec) -> String {
    std::iter::once(command.program.as_str())
        .chain(command.args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(any(windows, test))]
fn render_windows_command(command: &CommandSpec) -> String {
    let script = std::iter::once(command.program.as_str())
        .chain(command.args.iter().map(String::as_str))
        .map(powershell_quote)
        .collect::<Vec<_>>()
        .join(" ");
    let encoded = format!("& {script}")
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    format!(
        "powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand {}",
        BASE64_STANDARD.encode(encoded)
    )
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

fn responses_base_url(gateway: &LlmGatewayConfig) -> String {
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
    #[cfg(windows)]
    return std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("AgentDesktop/claude-desktop-credential-helper.cmd");
    #[cfg(not(windows))]
    return PathBuf::from("/etc/claude-desktop/agentdesktop-credential-helper");
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

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, path::PathBuf};

    use agentdesktop_core::config::parse_daemon;
    use base64::{Engine as _, prelude::BASE64_STANDARD};

    use super::{
        CommandSpec, DryRunReport, Reconciler, default_claude_desktop_credential_helper_path,
        render_posix_command, render_windows_command,
    };

    #[test]
    fn renders_posix_commands_with_each_argument_quoted() {
        let command = CommandSpec::new(
            Path::new("/Applications/Agent Desktop/agentdesktop"),
            ["--socket", "/tmp/agent's socket", "credential"],
        );

        assert_eq!(
            render_posix_command(&command),
            "'/Applications/Agent Desktop/agentdesktop' '--socket' '/tmp/agent'\\''s socket' 'credential'"
        );
    }

    #[test]
    fn renders_windows_commands_as_shell_neutral_encoded_powershell() {
        let command = CommandSpec::new(
            Path::new(r"C:\Program Files\Agent Desktop\agentdesktop.exe"),
            [
                "--socket",
                r"\\.\pipe\agentdesktop",
                "credential",
                "--client-id",
                "claude-code",
            ],
        );

        let rendered = render_windows_command(&command);
        let encoded = rendered
            .strip_prefix("powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand ")
            .expect("explicit PowerShell launcher");
        let bytes = BASE64_STANDARD.decode(encoded).expect("valid base64");
        let utf16 = bytes
            .chunks_exact(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();
        let script = String::from_utf16(&utf16).expect("valid UTF-16LE");

        assert_eq!(
            script,
            r"& 'C:\Program Files\Agent Desktop\agentdesktop.exe' '--socket' '\\.\pipe\agentdesktop' 'credential' '--client-id' 'claude-code'"
        );
    }

    #[test]
    fn claude_hooks_keep_the_executable_and_arguments_separate() {
        let reconciler = Reconciler::new(
            false,
            PathBuf::new(),
            PathBuf::new(),
            PathBuf::new(),
            PathBuf::new(),
            PathBuf::new(),
            PathBuf::new(),
            PathBuf::from(r"C:\Program Files\Agent Desktop\agentdesktop.exe"),
            PathBuf::from(r"\\.\pipe\agentdesktop"),
        );

        let tool = reconciler.claude_hook_command(true);
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
            reconciler.claude_session_hook_command().args,
            [
                "--socket",
                r"\\.\pipe\agentdesktop",
                "hook",
                "claude-session-start",
            ]
        );
    }

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

    #[test]
    fn dry_run_report_shows_changes_and_hides_unchanged_files() {
        let report = DryRunReport::default();
        report.record(
            "claude-code",
            "settings",
            "update",
            Path::new("/home/user/.claude/settings.json"),
            Some(br#"{"keep":true}"#),
            Some(br#"{"managed":true,"keep":true}"#),
        );
        report.record(
            "codex",
            "configuration",
            "unchanged",
            Path::new("/home/user/.codex/config.toml"),
            None,
            None,
        );

        let rendered = report.render();
        assert!(rendered.contains("UPDATE  Claude Code settings"));
        assert!(rendered.contains("+  \"managed\": true"));
        assert!(!rendered.contains("Codex configuration"));
        assert!(rendered.contains("Summary: 1 change, 1 unchanged"));
    }

    #[test]
    fn user_mode_rejects_claude_desktop_before_writing_other_settings() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-reconcile-user-desktop-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let config = parse_daemon(
            r#"
programs:
  claudeCode: {}
  claudeDesktop: {}
"#,
        )
        .expect("valid configuration");
        let reconciler = Reconciler::new(
            true,
            root.join("claude/settings.json"),
            root.join("claude-desktop/settings.json"),
            root.join("claude-desktop/helper"),
            root.join("codex/config.toml"),
            root.join("opencode/config.json"),
            root.join("opencode/plugin.js"),
            root.join("bin/agentdesktop"),
            root.join("agentdesktop.sock"),
        );

        let error = reconciler.apply(&config).expect_err("user mode must fail");

        assert!(
            error
                .to_string()
                .contains("/etc/claude-desktop/managed-settings.json")
        );
        assert!(!root.exists(), "preflight failure must not write any files");
    }

    #[test]
    fn dry_run_plans_create_update_and_remove_without_changing_files() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-reconcile-dry-run-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let config = parse_daemon(
            r#"
programs:
  claudeCode: {}
  claudeDesktop: {}
  codex: {}
  openCode: {}
"#,
        )
        .expect("valid configuration");
        fs::create_dir_all(root.join("codex")).unwrap();
        fs::create_dir_all(root.join("claude")).unwrap();
        fs::create_dir_all(root.join("opencode")).unwrap();
        let user_claude = br#"{"theme":"dark"}\n"#;
        let old_codex =
            b"# Managed by Agentdesktop. Manual changes will be replaced.\nmodel = \"old\"\n";
        let old_plugin =
            b"// Managed by Agentdesktop. Manual changes will be replaced.\nold plugin\n";
        fs::write(root.join("claude/settings.json"), user_claude).unwrap();
        fs::write(root.join("codex/config.toml"), old_codex).unwrap();
        fs::write(root.join("opencode/plugin.js"), old_plugin).unwrap();
        let reconciler = Reconciler::new(
            false,
            root.join("claude/settings.json"),
            root.join("claude-desktop/settings.json"),
            root.join("claude-desktop/helper"),
            root.join("codex/config.toml"),
            root.join("opencode/config.json"),
            root.join("opencode/plugin.js"),
            root.join("bin/agentdesktop"),
            root.join("agentdesktop.sock"),
        );

        reconciler.dry_run(&config).expect("dry run succeeds");

        assert_eq!(
            fs::read(root.join("claude/settings.json")).unwrap(),
            user_claude
        );
        assert!(!root.join("claude-desktop/settings.json").exists());
        assert!(!root.join("opencode/config.json").exists());
        assert_eq!(fs::read(root.join("codex/config.toml")).unwrap(), old_codex);
        assert_eq!(
            fs::read(root.join("opencode/plugin.js")).unwrap(),
            old_plugin
        );
        fs::remove_dir_all(root).unwrap();
    }
}
