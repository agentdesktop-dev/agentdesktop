use std::path::Path;

use agentdesktop_core::config::LlmGatewayConfig;
#[cfg(any(windows, test))]
use base64::{Engine as _, prelude::BASE64_STANDARD};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandSpec {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
}

impl CommandSpec {
    pub(crate) fn new(program: &Path, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            program: program.to_string_lossy().into_owned(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }
}

#[cfg(any(not(windows), test))]
pub(crate) fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(any(windows, test))]
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn render_command(command: &CommandSpec) -> String {
    #[cfg(windows)]
    return render_windows_command(command);
    #[cfg(not(windows))]
    return render_posix_command(command);
}

#[cfg(any(not(windows), test))]
pub(crate) fn render_posix_command(command: &CommandSpec) -> String {
    std::iter::once(command.program.as_str())
        .chain(command.args.iter().map(String::as_str))
        .map(shell_quote)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(any(windows, test))]
pub(crate) fn render_windows_command(command: &CommandSpec) -> String {
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

pub(crate) fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                deep_merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, overlay) => *base = overlay,
    }
}

pub(crate) fn responses_base_url(gateway: &LlmGatewayConfig) -> String {
    let mut url = gateway.url.clone();
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/v1") {
        url.set_path(&format!("{path}/v1"));
    }
    url.to_string().trim_end_matches('/').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
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
            .as_chunks::<2>()
            .0
            .iter()
            .map(|bytes| u16::from_le_bytes(*bytes))
            .collect::<Vec<_>>();
        let script = String::from_utf16(&utf16).expect("valid UTF-16LE");

        assert_eq!(
            script,
            r"& 'C:\Program Files\Agent Desktop\agentdesktop.exe' '--socket' '\\.\pipe\agentdesktop' 'credential' '--client-id' 'claude-code'"
        );
    }
}
