use std::{fs, path::Path};

use agentdesktop_core::config::{
    ClaudeDesktopConfig, InferenceGatewayAuthentication, InferenceGatewayConfig,
};
use anyhow::Context;
use serde_json::{Value, json};
use tracing::info;

use crate::secure_fs;

use super::{deep_merge, shell_quote};

const OWNER_MARKER: &[u8] = b"Agentdesktop\n";

pub fn apply(
    settings_path: &Path,
    helper_path: &Path,
    credential_binary: &Path,
    socket: &Path,
    config: Option<(&ClaudeDesktopConfig, Option<&InferenceGatewayConfig>)>,
) -> anyhow::Result<()> {
    let settings_owner = owner_path(settings_path);
    let helper_owner = owner_path(helper_path);
    let Some((config, gateway)) = config else {
        remove_owned(settings_path, &settings_owner, "managed settings")?;
        return remove_owned(helper_path, &helper_owner, "credential helper");
    };

    let uses_controller_jwt = gateway.is_some_and(|gateway| {
        matches!(
            gateway.authentication,
            Some(InferenceGatewayAuthentication::ControllerJwt { .. })
        )
    });
    if uses_controller_jwt {
        let script = format!(
            "#!/bin/sh\nexec {} --socket {} credential --client-id claude-desktop\n",
            shell_quote(&credential_binary.to_string_lossy()),
            shell_quote(&socket.to_string_lossy())
        );
        write_owned(
            helper_path,
            &helper_owner,
            script.as_bytes(),
            0o755,
            "credential helper",
        )?;
    } else {
        remove_owned(helper_path, &helper_owner, "credential helper")?;
    }

    let settings = managed_settings(config, gateway, helper_path)?;
    let mut contents = serde_json::to_vec_pretty(&settings)
        .context("serialize Claude Desktop managed settings")?;
    contents.push(b'\n');
    write_owned(
        settings_path,
        &settings_owner,
        &contents,
        0o644,
        "managed settings",
    )
}

fn managed_settings(
    config: &ClaudeDesktopConfig,
    gateway: Option<&InferenceGatewayConfig>,
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
    if matches!(
        gateway.authentication,
        Some(InferenceGatewayAuthentication::ControllerJwt { .. })
    ) {
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
) -> anyhow::Result<()> {
    let owned = is_owned(owner_path)?;
    let action = match fs::read(path) {
        Ok(existing) if existing == contents => {
            if !owned {
                secure_fs::atomic_write(owner_path, OWNER_MARKER, 0o644)?;
            }
            "unchanged"
        }
        Ok(_) if owned => "update",
        Ok(_) => anyhow::bail!(
            "refusing to replace Claude Desktop {description} not owned by Agentdesktop at {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "create",
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    if action != "unchanged" {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create directory {}", parent.display()))?;
        }
        secure_fs::atomic_write(path, contents, mode)?;
        secure_fs::atomic_write(owner_path, OWNER_MARKER, 0o644)?;
    }
    info!(program = "claude-desktop", action, path = %path.display(), "reconciled {description}");
    Ok(())
}

fn remove_owned(path: &Path, owner_path: &Path, description: &str) -> anyhow::Result<()> {
    if !is_owned(owner_path)? {
        return Ok(());
    }
    match fs::remove_file(path) {
        Ok(()) => {
            info!(program = "claude-desktop", action = "remove", path = %path.display(), "reconciled {description}")
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use agentdesktop_core::config::parse_daemon;

    use super::managed_settings;

    #[test]
    fn gateway_settings_override_pass_through_values() {
        let config = parse_daemon(
            r#"
inferenceGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
programs:
  claudeDesktop:
    isLocalDevMcpEnabled: true
    inferenceProvider: ignored
"#,
        )
        .unwrap();
        let desktop = config.programs.claude_desktop.as_ref().unwrap();
        let settings = managed_settings(
            desktop,
            config.inference_gateway.as_ref(),
            Path::new("/helper"),
        )
        .unwrap();
        assert_eq!(settings["isLocalDevMcpEnabled"], true);
        assert_eq!(settings["inferenceProvider"], "gateway");
        assert_eq!(settings["inferenceCredentialHelper"], "/helper");
    }
}
