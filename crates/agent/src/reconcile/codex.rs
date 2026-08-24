use std::{fs, path::Path};

use agentdesktop_core::config::{
    CodexConfig, InferenceGatewayAuthentication, InferenceGatewayConfig,
};
use anyhow::Context;
use serde_json::{Value, json};
use tracing::info;

use crate::secure_fs;

use super::{ReconcileMode, deep_merge, responses_base_url};

const MANAGED_HEADER: &str = "# Managed by Agentdesktop. Manual changes will be replaced.\n";

pub fn apply(
    path: &Path,
    credential_helper: &Path,
    socket: &Path,
    config: Option<(&CodexConfig, Option<&InferenceGatewayConfig>)>,
    mode: ReconcileMode,
) -> anyhow::Result<()> {
    let Some((config, gateway)) = config else {
        return remove(path, mode);
    };

    let settings = managed_config(config, gateway, credential_helper, socket)?;
    let mut contents = MANAGED_HEADER.as_bytes().to_vec();
    contents.extend_from_slice(
        toml::to_string_pretty(&settings)
            .context("serialize Codex managed configuration as TOML")?
            .as_bytes(),
    );
    if !contents.ends_with(b"\n") {
        contents.push(b'\n');
    }

    let existing = match fs::read(path) {
        Ok(existing) => Some(existing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read Codex managed configuration from {}", path.display())
            });
        }
    };
    let action = match existing.as_deref() {
        Some(existing) if existing == contents => {
            info!(
                program = "codex",
                action = "unchanged",
                path = %path.display(),
                "managed configuration already current"
            );
            mode.record("codex", "configuration", "unchanged", path);
            return Ok(());
        }
        Some(existing) if existing.starts_with(MANAGED_HEADER.as_bytes()) => "update",
        Some(existing) if mode.is_dry_run() => {
            mode.record_diff(
                "codex",
                "configuration",
                "conflict",
                path,
                Some(existing),
                Some(&contents),
            );
            return Ok(());
        }
        Some(_) => anyhow::bail!(
            "refusing to replace Codex configuration not owned by Agentdesktop at {}",
            path.display()
        ),
        None => "create",
    };

    if mode.writes() {
        let directory = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(directory).with_context(|| {
            format!(
                "create Codex configuration directory {}",
                directory.display()
            )
        })?;
        secure_fs::atomic_write(path, &contents, 0o644)?;
    }
    info!(
        program = "codex",
        action,
        path = %path.display(),
        "reconciled managed configuration"
    );
    mode.record_diff(
        "codex",
        "configuration",
        action,
        path,
        existing.as_deref(),
        Some(&contents),
    );
    Ok(())
}

fn managed_config(
    config: &CodexConfig,
    gateway: Option<&InferenceGatewayConfig>,
    credential_helper: &Path,
    socket: &Path,
) -> anyhow::Result<Value> {
    let mut settings = serde_json::to_value(&config.managed_config)
        .context("serialize Codex pass-through managed configuration")?;
    let Some(gateway) = gateway else {
        return Ok(settings);
    };

    let provider_name = "agentdesktop";
    let mut provider = json!({
        "name": "Agentdesktop",
        "base_url": responses_base_url(gateway),
        "wire_api": "responses",
    });
    if gateway
        .authentication
        .as_ref()
        .is_some_and(InferenceGatewayAuthentication::uses_credential_helper)
    {
        let timeout_ms = if matches!(
            gateway.authentication,
            Some(InferenceGatewayAuthentication::Oidc { .. })
        ) {
            600_000
        } else {
            5_000
        };
        provider["auth"] = json!({
            "command": credential_helper.to_string_lossy(),
            "args": [
                "--socket",
                socket.to_string_lossy(),
                "credential",
                "--client-id",
                "codex",
            ],
            "timeout_ms": timeout_ms,
            "refresh_interval_ms": 60000,
        });
    }
    let generated = json!({
        "model_provider": provider_name,
        "model_providers": {
            (provider_name): provider,
        },
    });
    deep_merge(&mut settings, generated);
    Ok(settings)
}

fn remove(path: &Path, mode: ReconcileMode) -> anyhow::Result<()> {
    match fs::read(path) {
        Ok(contents) if contents.starts_with(MANAGED_HEADER.as_bytes()) => {
            if mode.writes() {
                fs::remove_file(path).with_context(|| {
                    format!("remove Codex managed configuration at {}", path.display())
                })?;
            }
            info!(
                program = "codex",
                action = "remove",
                path = %path.display(),
                "reconciled managed configuration"
            );
            mode.record("codex", "configuration", "remove", path);
            Ok(())
        }
        Ok(_) => {
            info!(
                program = "codex",
                action = "unchanged",
                path = %path.display(),
                "preserving managed configuration not owned by Agentdesktop"
            );
            mode.record("codex", "configuration", "unchanged", path);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            info!(
                program = "codex",
                action = "unchanged",
                path = %path.display(),
                "managed configuration already absent"
            );
            mode.record("codex", "configuration", "unchanged", path);
            Ok(())
        }
        Err(error) => Err(error)
            .with_context(|| format!("read Codex managed configuration from {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use agentdesktop_core::config::parse_daemon;

    use super::managed_config;

    #[test]
    fn pass_through_settings_are_merged_with_managed_gateway_values() {
        let config = parse_daemon(
            r#"
inferenceGateway:
  url: https://gateway.example.com/proxy
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [codex]
programs:
  codex:
    managedConfig:
      model: company-model
      model_provider: ignored
      model_providers:
        existing:
          base_url: https://existing.example.com/v1
      otel:
        environment: production
"#,
        )
        .expect("valid daemon configuration");
        let codex = config.programs.codex.as_ref().unwrap();
        let gateway = config.inference_gateway.as_ref().unwrap();

        let settings = managed_config(
            codex,
            Some(gateway),
            Path::new("/usr/local/bin/agentdesktop"),
            Path::new("/run/agentdesktop/agentdesktop.sock"),
        )
        .expect("merged settings");

        assert_eq!(settings["model"], "company-model");
        assert_eq!(settings["otel"]["environment"], "production");
        assert_eq!(settings["model_provider"], "agentdesktop");
        assert_eq!(
            settings["model_providers"]["existing"]["base_url"],
            "https://existing.example.com/v1"
        );
        let provider = &settings["model_providers"]["agentdesktop"];
        assert_eq!(provider["base_url"], "https://gateway.example.com/proxy/v1");
        assert_eq!(provider["wire_api"], "responses");
        assert_eq!(provider["auth"]["command"], "/usr/local/bin/agentdesktop");
        assert_eq!(provider["auth"]["args"].as_array().unwrap().len(), 5);
        assert_eq!(provider["auth"]["args"][3], "--client-id");
        assert_eq!(provider["auth"]["args"][4], "codex");
        assert_eq!(provider["auth"]["refresh_interval_ms"], 60000);

        let serialized = toml::to_string_pretty(&settings).expect("valid TOML");
        let parsed: toml::Value = toml::from_str(&serialized).expect("parse generated TOML");
        assert_eq!(parsed["model_provider"].as_str(), Some("agentdesktop"));
    }
}
