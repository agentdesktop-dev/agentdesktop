use std::{fs, path::Path};

use anyhow::Context;

use agentdesktop_core::config::{
    ClaudeCodeConfig, InferenceGatewayAuthentication, InferenceGatewayConfig,
};
use serde_json::{Value, json};
use tracing::info;

use crate::secure_fs;

use super::deep_merge;

const FILE_NAME: &str = "50-agentdesktop.json";
const OWNER_FILE_NAME: &str = ".50-agentdesktop.json.owner";
const OWNER_MARKER: &[u8] = b"AgentDesktop\n";

pub fn apply(
    directory: &Path,
    credential_helper: &str,
    hook_command: &str,
    config: Option<(&ClaudeCodeConfig, Option<&InferenceGatewayConfig>)>,
) -> anyhow::Result<()> {
    let path = directory.join(FILE_NAME);
    let owner_path = directory.join(OWNER_FILE_NAME);
    let Some((config, gateway)) = config else {
        return remove(&path, &owner_path);
    };

    let settings = managed_settings(config, gateway, credential_helper, hook_command)?;
    let mut contents =
        serde_json::to_vec_pretty(&settings).context("serialize Claude Code managed settings")?;
    contents.push(b'\n');
    let owned = is_owned(&owner_path)?;
    let action = match fs::read(&path) {
        Ok(existing) if existing == contents => {
            if !owned {
                secure_fs::atomic_write(&owner_path, OWNER_MARKER, 0o644)?;
            }
            info!(
                program = "claude-code",
                action = "unchanged",
                path = %path.display(),
                "managed settings already current"
            );
            return Ok(());
        }
        Ok(_) if owned => "update",
        Ok(_) => anyhow::bail!(
            "refusing to replace Claude Code managed settings not owned by AgentDesktop at {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "create",
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read Claude Code managed settings from {}", path.display())
            });
        }
    };

    fs::create_dir_all(directory).with_context(|| {
        format!(
            "create Claude Code settings directory {}",
            directory.display()
        )
    })?;
    secure_fs::atomic_write(&path, &contents, 0o644)?;
    secure_fs::atomic_write(&owner_path, OWNER_MARKER, 0o644)?;
    info!(
        program = "claude-code",
        action,
        path = %path.display(),
        "reconciled managed settings"
    );
    Ok(())
}

fn managed_settings(
    config: &ClaudeCodeConfig,
    gateway: Option<&InferenceGatewayConfig>,
    credential_helper: &str,
    hook_command: &str,
) -> anyhow::Result<Value> {
    let mut settings = serde_json::to_value(&config.settings)
        .context("serialize Claude Code pass-through settings")?;
    append_pre_tool_use_hook(&mut settings, hook_command)?;
    let Some(gateway) = gateway else {
        return Ok(settings);
    };

    let mut generated = json!({
        "env": {
            "ANTHROPIC_BASE_URL": gateway.url.as_str(),
        }
    });
    if matches!(
        gateway.authentication,
        Some(InferenceGatewayAuthentication::ControllerJwt { .. })
    ) {
        generated["env"]["CLAUDE_CODE_API_KEY_HELPER_TTL_MS"] = json!("60000");
        generated["apiKeyHelper"] = json!(credential_helper);
    }
    deep_merge(&mut settings, generated);
    Ok(settings)
}

fn append_pre_tool_use_hook(settings: &mut Value, hook_command: &str) -> anyhow::Result<()> {
    let settings = settings
        .as_object_mut()
        .context("Claude Code managed settings must be an object")?;
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Default::default()))
        .as_object_mut()
        .context("Claude Code hooks must be an object")?;
    let pre_tool_use = hooks
        .entry("PreToolUse")
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .context("Claude Code PreToolUse hooks must be an array")?;
    let already_present = pre_tool_use.iter().any(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hooks| {
                hooks
                    .iter()
                    .any(|hook| hook.get("command").and_then(Value::as_str) == Some(hook_command))
            })
    });
    if !already_present {
        pre_tool_use.push(json!({
            "matcher": "",
            "hooks": [{
                "type": "command",
                "command": hook_command,
            }],
        }));
    }
    Ok(())
}

fn remove(path: &Path, owner_path: &Path) -> anyhow::Result<()> {
    if !is_owned(owner_path)? {
        if path.exists() {
            info!(
                program = "claude-code",
                action = "unchanged",
                path = %path.display(),
                "preserving managed settings not owned by AgentDesktop"
            );
        }
        return Ok(());
    }
    match fs::remove_file(path) {
        Ok(()) => {
            info!(
                program = "claude-code",
                action = "remove",
                path = %path.display(),
                "reconciled managed settings"
            );
            remove_owner_marker(owner_path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            info!(
                program = "claude-code",
                action = "unchanged",
                path = %path.display(),
                "managed settings already absent"
            );
            remove_owner_marker(owner_path)
        }
        Err(error) => Err(error)
            .with_context(|| format!("remove Claude Code managed settings at {}", path.display())),
    }
}

fn is_owned(owner_path: &Path) -> anyhow::Result<bool> {
    match fs::read(owner_path) {
        Ok(contents) => Ok(contents == OWNER_MARKER),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("read ownership marker from {}", owner_path.display())),
    }
}

fn remove_owner_marker(owner_path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(owner_path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error)
            .with_context(|| format!("remove ownership marker at {}", owner_path.display())),
    }
}

#[cfg(test)]
mod tests {
    use agentdesktop_core::config::parse_desired;
    use serde_json::json;

    use super::managed_settings;

    #[test]
    fn pass_through_settings_are_deep_merged_with_managed_gateway_values() {
        let desired = parse_desired(
            r#"
inferenceGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
programs:
  claudeCode:
    apiKeyHelper: ignored-helper
    env:
      COMPANY_ENVIRONMENT: production
      ANTHROPIC_BASE_URL: https://ignored.example.com
    permissions:
      defaultMode: plan
"#,
        )
        .expect("valid desired configuration");
        let claude = desired.programs.claude_code.as_ref().unwrap();
        let gateway = desired.inference_gateway.as_ref().unwrap();

        let settings = managed_settings(
            claude,
            Some(gateway),
            "agentdesktop credential",
            "agentdesktop hook claude-pre-tool-use",
        )
        .expect("merged settings");

        assert_eq!(settings["env"]["COMPANY_ENVIRONMENT"], "production");
        assert_eq!(
            settings["env"]["ANTHROPIC_BASE_URL"],
            "https://gateway.example.com/"
        );
        assert_eq!(
            settings["env"]["CLAUDE_CODE_API_KEY_HELPER_TTL_MS"],
            "60000"
        );
        assert_eq!(settings["apiKeyHelper"], "agentdesktop credential");
        assert_eq!(settings["permissions"], json!({ "defaultMode": "plan" }));
        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "agentdesktop hook claude-pre-tool-use"
        );
    }
}
