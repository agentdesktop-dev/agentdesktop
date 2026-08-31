use std::{fs, path::Path};

use agentdesktop_core::config::{ClaudeDesktopConfig, LlmGatewayAuthentication, LlmGatewayConfig};
use anyhow::Context;
use serde_json::{Value, json};
use tracing::info;

use crate::secure_fs;

#[cfg(any(not(windows), test))]
use super::shell_quote;
use super::{ReconcileMode, deep_merge, json_merge};

const OWNER_MARKER: &[u8] = b"Agentdesktop\n";

pub fn apply(
    settings_path: &Path,
    merge_existing: bool,
    helper_path: &Path,
    credential_binary: &Path,
    socket: &Path,
    config: Option<(&ClaudeDesktopConfig, Option<&LlmGatewayConfig>)>,
    mode: ReconcileMode,
) -> anyhow::Result<()> {
    let settings_owner = owner_path(settings_path);
    let settings_state = json_merge::state_path(settings_path);
    let helper_owner = owner_path(helper_path);
    let Some((config, gateway)) = config else {
        if !merge_existing
            || !json_merge::remove(
                settings_path,
                &settings_state,
                "claude-desktop",
                "managed settings",
                "Claude Desktop settings",
                mode,
            )?
        {
            remove_owned(settings_path, &settings_owner, "managed settings", mode)?;
        } else if mode.writes() {
            remove_owner_marker(&settings_owner)?;
        }
        return remove_owned(helper_path, &helper_owner, "credential helper", mode);
    };

    let uses_credential_helper = gateway.is_some_and(|gateway| {
        gateway
            .authentication
            .as_ref()
            .is_some_and(LlmGatewayAuthentication::uses_credential_helper)
    });
    if uses_credential_helper {
        let script = credential_helper_contents(credential_binary, socket)?;
        write_owned(
            helper_path,
            &helper_owner,
            &script,
            0o755,
            "credential helper",
            mode,
        )?;
    } else {
        remove_owned(helper_path, &helper_owner, "credential helper", mode)?;
    }

    let settings = managed_settings(config, gateway, helper_path)?;
    if merge_existing {
        anyhow::bail!(
            "Claude Desktop settings cannot be applied to its user preferences; use system-managed settings"
        );
    }
    let mut contents = serde_json::to_vec_pretty(&settings)
        .context("serialize Claude Desktop managed settings")?;
    contents.push(b'\n');
    write_owned(
        settings_path,
        &settings_owner,
        &contents,
        0o644,
        "managed settings",
        mode,
    )
}

fn credential_helper_contents(credential_binary: &Path, socket: &Path) -> anyhow::Result<Vec<u8>> {
    #[cfg(windows)]
    return windows_credential_helper_contents(
        &credential_binary.to_string_lossy(),
        &socket.to_string_lossy(),
    );
    #[cfg(not(windows))]
    return Ok(posix_credential_helper_contents(
        &credential_binary.to_string_lossy(),
        &socket.to_string_lossy(),
    ));
}

#[cfg(any(not(windows), test))]
fn posix_credential_helper_contents(credential_binary: &str, socket: &str) -> Vec<u8> {
    format!(
        "#!/bin/sh\nexec {} --socket {} credential --client-id claude-desktop\n",
        shell_quote(credential_binary),
        shell_quote(socket)
    )
    .into_bytes()
}

#[cfg(any(windows, test))]
fn windows_credential_helper_contents(
    credential_binary: &str,
    socket: &str,
) -> anyhow::Result<Vec<u8>> {
    Ok(format!(
        "@echo off\r\n{} --socket {} credential --client-id claude-desktop\r\n",
        batch_quote(credential_binary)?,
        batch_quote(socket)?
    )
    .into_bytes())
}

#[cfg(any(windows, test))]
fn batch_quote(value: &str) -> anyhow::Result<String> {
    if value.contains(['"', '\r', '\n']) {
        anyhow::bail!("Windows helper argument contains an unsupported quote or newline");
    }
    Ok(format!("\"{}\"", value.replace('%', "%%")))
}

fn managed_settings(
    config: &ClaudeDesktopConfig,
    gateway: Option<&LlmGatewayConfig>,
    helper_path: &Path,
) -> anyhow::Result<Value> {
    let mut settings = serde_json::to_value(&config.settings)
        .context("serialize Claude Desktop pass-through settings")?;
    let Some(gateway) = gateway else {
        return Ok(settings);
    };
    let mut generated = json!({
        "inferenceProvider": "gateway",
        "inferenceGatewayBaseUrl": gateway.url.as_str(),
    });
    if gateway
        .authentication
        .as_ref()
        .is_some_and(LlmGatewayAuthentication::uses_credential_helper)
    {
        generated["inferenceCredentialKind"] = json!("helper-script");
        generated["inferenceCredentialHelper"] = json!(helper_path);
        generated["inferenceCredentialHelperTtlSec"] = json!(60);
    }
    deep_merge(&mut settings, generated);
    Ok(settings)
}

fn owner_path(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("managed");
    path.with_file_name(format!(".{name}.owner"))
}

fn write_owned(
    path: &Path,
    owner_path: &Path,
    contents: &[u8],
    mode: u32,
    description: &str,
    reconcile_mode: ReconcileMode,
) -> anyhow::Result<()> {
    let owned = is_owned(owner_path)?;
    let existing = match fs::read(path) {
        Ok(existing) => Some(existing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let action = match existing.as_deref() {
        Some(existing) if existing == contents => {
            if !owned && reconcile_mode.writes() {
                secure_fs::atomic_write(owner_path, OWNER_MARKER, 0o644)?;
            }
            "unchanged"
        }
        Some(_) if owned => "update",
        Some(existing) if reconcile_mode.is_dry_run() => {
            reconcile_mode.record_diff(
                "claude-desktop",
                description,
                "conflict",
                path,
                Some(existing),
                Some(contents),
            );
            return Ok(());
        }
        Some(_) => anyhow::bail!(
            "refusing to replace Claude Desktop {description} not owned by Agentdesktop at {}",
            path.display()
        ),
        None => "create",
    };
    if action != "unchanged" && reconcile_mode.writes() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
        }
        secure_fs::atomic_write(path, contents, mode)?;
        secure_fs::atomic_write(owner_path, OWNER_MARKER, 0o644)?;
    }
    info!(program = "claude-desktop", action, path = %path.display(), "reconciled {description}");
    reconcile_mode.record_diff(
        "claude-desktop",
        description,
        action,
        path,
        existing.as_deref(),
        Some(contents),
    );
    Ok(())
}

fn remove_owned(
    path: &Path,
    owner_path: &Path,
    description: &str,
    mode: ReconcileMode,
) -> anyhow::Result<()> {
    if !is_owned(owner_path)? {
        mode.record("claude-desktop", description, "unchanged", path);
        return Ok(());
    }
    let exists = match fs::metadata(path) {
        Ok(_) => {
            if mode.writes() {
                fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
            }
            info!(program = "claude-desktop", action = "remove", path = %path.display(), "reconciled {description}");
            mode.record("claude-desktop", description, "remove", path);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
    };
    if !exists {
        mode.record("claude-desktop", description, "unchanged", path);
    }
    if !mode.writes() {
        return Ok(());
    }
    match fs::remove_file(owner_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", owner_path.display())),
    }
}

fn is_owned(path: &Path) -> anyhow::Result<bool> {
    match fs::read(path) {
        Ok(contents) => Ok(contents == OWNER_MARKER),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn remove_owner_marker(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        batch_quote, managed_settings, posix_credential_helper_contents,
        windows_credential_helper_contents,
    };
    use agentdesktop_core::config::parse_daemon;

    #[test]
    fn generates_platform_specific_credential_helpers() {
        let binary = r"C:\Program Files\Agent Desktop\agentdesktop.exe";
        let socket = r"\\.\pipe\agentdesktop";

        let windows = windows_credential_helper_contents(binary, socket).expect("Windows helper");
        assert_eq!(
            String::from_utf8(windows).unwrap(),
            "@echo off\r\n\"C:\\Program Files\\Agent Desktop\\agentdesktop.exe\" --socket \"\\\\.\\pipe\\agentdesktop\" credential --client-id claude-desktop\r\n"
        );

        let posix = posix_credential_helper_contents(
            "/opt/Agent Desktop/agentdesktop",
            "/run/agentdesktop.sock",
        );
        assert_eq!(
            String::from_utf8(posix).unwrap(),
            "#!/bin/sh\nexec '/opt/Agent Desktop/agentdesktop' --socket '/run/agentdesktop.sock' credential --client-id claude-desktop\n"
        );
    }

    #[test]
    fn escapes_batch_expansion_and_rejects_unrepresentable_arguments() {
        assert_eq!(
            batch_quote(r"C:\Agent%TEMP%\agentdesktop.exe").unwrap(),
            r#""C:\Agent%%TEMP%%\agentdesktop.exe""#
        );
        assert!(batch_quote("bad\"path").is_err());
        assert!(batch_quote("bad\npath").is_err());
    }

    #[test]
    fn gateway_settings_override_pass_through_values() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-desktop]
programs:
  claudeDesktop:
    isLocalDevMcpEnabled: true
    inferenceProvider: ignored
"#,
        )
        .unwrap();
        let desktop = config.programs.claude_desktop.as_ref().unwrap();
        let settings =
            managed_settings(desktop, config.llm_gateway.as_ref(), Path::new("/helper")).unwrap();
        assert_eq!(settings["isLocalDevMcpEnabled"], true);
        assert_eq!(settings["inferenceProvider"], "gateway");
        assert_eq!(settings["inferenceCredentialHelper"], "/helper");
    }
}
