use std::{fs, io::Write, path::Path};

use anyhow::Context;

use agentdesktop_core::config::{
    ClaudeCodeConfig, InferenceGatewayAuthentication, InferenceGatewayConfig,
};
use serde_json::{Value, json};
use tracing::info;

const FILE_NAME: &str = "50-agentdesktop.json";

pub fn apply(
    directory: &Path,
    credential_helper: &str,
    config: Option<(&ClaudeCodeConfig, Option<&InferenceGatewayConfig>)>,
) -> anyhow::Result<()> {
    let path = directory.join(FILE_NAME);
    let Some((config, gateway)) = config else {
        return remove(&path);
    };

    let settings = managed_settings(config, gateway, credential_helper)?;
    let mut contents =
        serde_json::to_vec_pretty(&settings).context("serialize Claude Code managed settings")?;
    contents.push(b'\n');
    let action = match fs::read(&path) {
        Ok(existing) if existing == contents => {
            info!(
                program = "claude-code",
                action = "unchanged",
                path = %path.display(),
                "managed settings already current"
            );
            return Ok(());
        }
        Ok(_) => "update",
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
    let temporary = directory.join(format!(".{FILE_NAME}.tmp"));
    write_file(&temporary, &contents)?;
    fs::rename(&temporary, &path)
        .with_context(|| format!("install Claude Code managed settings at {}", path.display()))?;
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
) -> anyhow::Result<Value> {
    let mut settings = serde_json::to_value(&config.settings)
        .context("serialize Claude Code pass-through settings")?;
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
        let name = config
            .inference_gateway
            .as_deref()
            .context("Claude Code gateway configuration has no gateway name")?;
        generated["env"]["CLAUDE_CODE_API_KEY_HELPER_TTL_MS"] = json!("60000");
        generated["apiKeyHelper"] = json!(format!("{credential_helper} {name}"));
    }
    merge(&mut settings, generated);
    Ok(settings)
}

fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, overlay) => *base = overlay,
    }
}

fn remove(path: &Path) -> anyhow::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {
            info!(
                program = "claude-code",
                action = "remove",
                path = %path.display(),
                "reconciled managed settings"
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            info!(
                program = "claude-code",
                action = "unchanged",
                path = %path.display(),
                "managed settings already absent"
            );
            Ok(())
        }
        Err(error) => Err(error)
            .with_context(|| format!("remove Claude Code managed settings at {}", path.display())),
    }
}

fn write_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o644);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("write Claude Code managed settings to {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write Claude Code managed settings to {}", path.display()))
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
inferenceGateways:
  corporate:
    url: https://gateway.example.com
    authentication:
      type: controllerJwt
      audience: agentgateway
programs:
  claudeCode:
    inferenceGateway: corporate
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
        let gateway = &desired.inference_gateways["corporate"];

        let settings = managed_settings(claude, Some(gateway), "agentdesktop credential")
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
        assert_eq!(
            settings["apiKeyHelper"],
            "agentdesktop credential corporate"
        );
        assert_eq!(settings["permissions"], json!({ "defaultMode": "plan" }));
    }
}
