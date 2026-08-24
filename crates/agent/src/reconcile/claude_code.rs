use std::{fs, path::Path};

use anyhow::Context;

use agentdesktop_core::config::{
    ClaudeCodeConfig, InferenceGatewayAuthentication, InferenceGatewayConfig,
};
use serde_json::{Value, json};
use tracing::info;

use crate::secure_fs;

use super::{CommandSpec, ReconcileMode, deep_merge, json_merge};

const OWNER_MARKER: &[u8] = b"Agentdesktop\n";

pub fn apply(
    path: &Path,
    merge_existing: bool,
    credential_helper: &str,
    tool_use_hook: Option<&CommandSpec>,
    session_new_hook: Option<&CommandSpec>,
    config: Option<(&ClaudeCodeConfig, Option<&InferenceGatewayConfig>)>,
    mode: ReconcileMode,
) -> anyhow::Result<()> {
    let owner_path = owner_path(path);
    let merge_state_path = json_merge::state_path(path);
    let Some((config, gateway)) = config else {
        if merge_existing
            && json_merge::remove(
                path,
                &merge_state_path,
                "claude-code",
                "settings",
                "Claude Code settings",
                mode,
            )?
        {
            if mode.writes() {
                remove_owner_marker(&owner_path)?;
            }
            return Ok(());
        }
        return remove(path, &owner_path, mode);
    };

    let settings = managed_settings(
        config,
        gateway,
        credential_helper,
        tool_use_hook,
        session_new_hook,
    )?;
    if merge_existing {
        json_merge::apply(
            path,
            &merge_state_path,
            settings,
            is_owned(&owner_path)?,
            "claude-code",
            "settings",
            "Claude Code settings",
            mode,
        )?;
        if mode.writes() {
            remove_owner_marker(&owner_path)?;
        }
        return Ok(());
    }
    let mut contents =
        serde_json::to_vec_pretty(&settings).context("serialize Claude Code managed settings")?;
    contents.push(b'\n');
    let owned = is_owned(&owner_path)?;
    let existing = match fs::read(path) {
        Ok(existing) => Some(existing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read Claude Code managed settings from {}", path.display())
            });
        }
    };
    let action = match existing.as_deref() {
        Some(existing) if existing == contents => {
            if !owned && mode.writes() {
                secure_fs::atomic_write(&owner_path, OWNER_MARKER, 0o644)?;
            }
            info!(
                program = "claude-code",
                action = "unchanged",
                path = %path.display(),
                "managed settings already current"
            );
            mode.record("claude-code", "settings", "unchanged", path);
            return Ok(());
        }
        Some(_) if owned => "update",
        Some(existing) if mode.is_dry_run() => {
            mode.record_diff(
                "claude-code",
                "settings",
                "conflict",
                path,
                Some(existing),
                Some(&contents),
            );
            return Ok(());
        }
        Some(_) => anyhow::bail!(
            "refusing to replace Claude Code managed settings not owned by Agentdesktop at {}",
            path.display()
        ),
        None => "create",
    };

    if mode.writes() {
        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(directory).with_context(|| {
            format!(
                "create Claude Code settings directory {}",
                directory.display()
            )
        })?;
        secure_fs::atomic_write(path, &contents, 0o644)?;
        secure_fs::atomic_write(&owner_path, OWNER_MARKER, 0o644)?;
    }
    info!(
        program = "claude-code",
        action,
        path = %path.display(),
        "reconciled managed settings"
    );
    mode.record_diff(
        "claude-code",
        "settings",
        action,
        path,
        existing.as_deref(),
        Some(&contents),
    );
    Ok(())
}

fn owner_path(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("settings.json");
    path.with_file_name(format!(".{name}.owner"))
}

fn managed_settings(
    config: &ClaudeCodeConfig,
    gateway: Option<&InferenceGatewayConfig>,
    credential_helper: &str,
    tool_use_hook: Option<&CommandSpec>,
    session_new_hook: Option<&CommandSpec>,
) -> anyhow::Result<Value> {
    let mut settings = serde_json::to_value(&config.settings)
        .context("serialize Claude Code pass-through settings")?;
    if let Some(command) = tool_use_hook {
        append_hook(&mut settings, "PreToolUse", command)?;
    }
    if let Some(command) = session_new_hook {
        append_hook(&mut settings, "SessionStart", command)?;
    }
    let Some(gateway) = gateway else {
        return Ok(settings);
    };

    let mut generated = json!({
        "env": {
            "ANTHROPIC_BASE_URL": gateway.url.as_str(),
        }
    });
    if gateway
        .authentication
        .as_ref()
        .is_some_and(InferenceGatewayAuthentication::uses_credential_helper)
    {
        generated["env"]["CLAUDE_CODE_API_KEY_HELPER_TTL_MS"] = json!("60000");
        generated["apiKeyHelper"] = json!(credential_helper);
    }
    deep_merge(&mut settings, generated);
    Ok(settings)
}

fn append_hook(
    settings: &mut Value,
    event: &str,
    hook_command: &CommandSpec,
) -> anyhow::Result<()> {
    let settings = settings
        .as_object_mut()
        .context("Claude Code managed settings must be an object")?;
    let hooks = settings
        .entry("hooks")
        .or_insert_with(|| Value::Object(Default::default()))
        .as_object_mut()
        .context("Claude Code hooks must be an object")?;
    let event_hooks = hooks
        .entry(event)
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .with_context(|| format!("Claude Code {event} hooks must be an array"))?;
    let generated_hook = json!({
        "type": "command",
        "command": hook_command.program,
        "args": hook_command.args,
    });
    let already_present = event_hooks.iter().any(|group| {
        group
            .get("hooks")
            .and_then(Value::as_array)
            .is_some_and(|hooks| hooks.contains(&generated_hook))
    });
    if !already_present {
        event_hooks.push(json!({
            "matcher": "",
            "hooks": [generated_hook],
        }));
    }
    Ok(())
}

fn remove(path: &Path, owner_path: &Path, mode: ReconcileMode) -> anyhow::Result<()> {
    if !is_owned(owner_path)? {
        if path.exists() {
            info!(
                program = "claude-code",
                action = "unchanged",
                path = %path.display(),
                "preserving managed settings not owned by Agentdesktop"
            );
        }
        mode.record("claude-code", "settings", "unchanged", path);
        return Ok(());
    }
    match fs::metadata(path) {
        Ok(_) => {
            if mode.writes() {
                fs::remove_file(path).with_context(|| {
                    format!("remove Claude Code managed settings at {}", path.display())
                })?;
                remove_owner_marker(owner_path)?;
            }
            info!(
                program = "claude-code",
                action = "remove",
                path = %path.display(),
                "reconciled managed settings"
            );
            mode.record("claude-code", "settings", "remove", path);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            info!(
                program = "claude-code",
                action = "unchanged",
                path = %path.display(),
                "managed settings already absent"
            );
            if mode.writes() {
                remove_owner_marker(owner_path)?;
            }
            mode.record("claude-code", "settings", "unchanged", path);
            Ok(())
        }
        Err(error) => Err(error)
            .with_context(|| format!("inspect Claude Code managed settings at {}", path.display())),
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
    use std::{fs, path::Path};

    use agentdesktop_core::config::parse_daemon;
    use serde_json::{Value, json};

    use super::{CommandSpec, ReconcileMode, apply, json_merge, managed_settings};

    #[test]
    fn pass_through_settings_are_deep_merged_with_managed_gateway_values() {
        let config = parse_daemon(
            r#"
inferenceGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-code]
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
        .expect("valid daemon configuration");
        let claude = config.programs.claude_code.as_ref().unwrap();
        let gateway = config.inference_gateway.as_ref().unwrap();
        let hook = CommandSpec::new(Path::new("agentdesktop"), ["hook", "claude-pre-tool-use"]);

        let settings = managed_settings(
            claude,
            Some(gateway),
            "agentdesktop credential",
            Some(&hook),
            None,
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
            "agentdesktop"
        );
        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["hooks"][0]["args"],
            json!(["hook", "claude-pre-tool-use"])
        );
    }

    #[test]
    fn user_settings_are_merged_and_only_managed_values_are_removed() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-claude-user-merge-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let path = root.join(".claude/settings.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{
  "theme": "dark",
  "env": {
    "MINE": "yes",
    "AGENTDESKTOP_MANAGED": "mine"
  },
  "companyAnnouncements": ["Personal announcement"]
}
"#,
        )
        .unwrap();
        let config = parse_daemon(
            r#"
programs:
  claudeCode:
    companyAnnouncements: ["Managed announcement"]
    env:
      AGENTDESKTOP_MANAGED: "yes"
"#,
        )
        .unwrap();
        let claude = config.programs.claude_code.as_ref().unwrap();

        apply(
            &path,
            true,
            "agentdesktop credential",
            None,
            None,
            Some((claude, None)),
            ReconcileMode::Apply,
        )
        .unwrap();

        let merged: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(merged["theme"], "dark");
        assert_eq!(merged["env"]["MINE"], "yes");
        assert_eq!(merged["env"]["AGENTDESKTOP_MANAGED"], "yes");
        assert_eq!(
            merged["companyAnnouncements"],
            json!(["Personal announcement", "Managed announcement"])
        );

        apply(
            &path,
            true,
            "agentdesktop credential",
            None,
            None,
            None,
            ReconcileMode::Apply,
        )
        .unwrap();

        let restored: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            restored,
            json!({
                "theme": "dark",
                "env": {
                    "MINE": "yes",
                    "AGENTDESKTOP_MANAGED": "mine"
                },
                "companyAnnouncements": ["Personal announcement"]
            })
        );
        assert!(!json_merge::state_path(&path).exists());
        fs::remove_dir_all(root).unwrap();
    }
}
