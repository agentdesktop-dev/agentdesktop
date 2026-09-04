use std::{fs, path::Path};

use agentdesktop_core::config::{GrokConfig, LlmGatewayAuthentication, LlmGatewayConfig};
use anyhow::Context;
use serde_json::{Map, Value, json};
use tracing::info;

use crate::secure_fs;

use super::{ReconcileMode, deep_merge, responses_base_url};

const MANAGED_HEADER: &str = "# Managed by Agentdesktop. Manual changes will be replaced.\n";
const CONFIG_PROGRAM: &str = "grok";
const PROVIDER_NAME: &str = "agentdesktop";

pub fn apply(
    path: &Path,
    credential_helper: &Path,
    socket: &Path,
    config: Option<(&GrokConfig, Option<&LlmGatewayConfig>)>,
    mode: ReconcileMode,
) -> anyhow::Result<()> {
    let Some((config, gateway)) = config else {
        return remove(path, mode);
    };

    let settings = managed_config(config, gateway, credential_helper, socket)?;
    let mut contents = MANAGED_HEADER.as_bytes().to_vec();
    contents.extend_from_slice(
        toml::to_string_pretty(&settings)
            .context("serialize Grok managed configuration as TOML")?
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
                format!("read Grok managed configuration from {}", path.display())
            });
        }
    };
    let action = match existing.as_deref() {
        Some(existing) if existing == contents => {
            info!(
                program = CONFIG_PROGRAM,
                action = "unchanged",
                path = %path.display(),
                "managed configuration already current"
            );
            mode.record(CONFIG_PROGRAM, "configuration", "unchanged", path);
            return Ok(());
        }
        Some(existing) if existing.starts_with(MANAGED_HEADER.as_bytes()) => "update",
        Some(existing) if mode.is_dry_run() => {
            mode.record_diff(
                CONFIG_PROGRAM,
                "configuration",
                "conflict",
                path,
                Some(existing),
                Some(&contents),
            );
            return Ok(());
        }
        Some(_) => anyhow::bail!(
            "refusing to replace Grok configuration not owned by Agentdesktop at {}",
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
                "create Grok configuration directory {}",
                directory.display()
            )
        })?;
        secure_fs::atomic_write(path, &contents, 0o644)?;
    }
    info!(
        program = CONFIG_PROGRAM,
        action,
        path = %path.display(),
        "reconciled managed configuration"
    );
    mode.record_diff(
        CONFIG_PROGRAM,
        "configuration",
        action,
        path,
        existing.as_deref(),
        Some(&contents),
    );
    Ok(())
}

fn managed_config(
    config: &GrokConfig,
    gateway: Option<&LlmGatewayConfig>,
    credential_helper: &Path,
    socket: &Path,
) -> anyhow::Result<Value> {
    let mut settings = serde_json::to_value(&config.managed_config)
        .context("serialize Grok pass-through managed configuration")?;
    let Some(gateway) = gateway else {
        return Ok(settings);
    };

    let default_model = config
        .model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .context("Grok gateway configuration has no model")?;
    let catalog = catalog_entries(config, default_model);
    let mut models = Map::new();
    for (id, value) in catalog {
        let mut entry = match value {
            Value::Object(_) => value,
            _ => json!({}),
        };
        if entry.get("model").and_then(Value::as_str).is_none() {
            entry["model"] = json!(id);
        }
        entry["base_url"] = json!(responses_base_url(gateway));
        if gateway
            .authentication
            .as_ref()
            .is_some_and(LlmGatewayAuthentication::uses_credential_helper)
        {
            entry["auth_provider"] = json!(PROVIDER_NAME);
        }
        models.insert(id, entry);
    }

    let mut generated = json!({
        "models": { "default": default_model },
        "model": models,
    });
    if gateway
        .authentication
        .as_ref()
        .is_some_and(LlmGatewayAuthentication::uses_credential_helper)
    {
        let timeout_secs = if matches!(
            gateway.authentication,
            Some(LlmGatewayAuthentication::Oidc { .. })
        ) {
            600
        } else {
            5
        };
        generated["auth_provider"] = json!({
            (PROVIDER_NAME): {
                "command": credential_helper.to_string_lossy(),
                "args": [
                    "--socket",
                    socket.to_string_lossy(),
                    "credential",
                    "--client-id",
                    "grok",
                ],
                "timeout_secs": timeout_secs,
            },
        });
    }
    deep_merge(&mut settings, generated);
    Ok(settings)
}

fn catalog_entries(config: &GrokConfig, default_model: &str) -> Vec<(String, Value)> {
    if config.models.is_empty() {
        return vec![(
            default_model.to_owned(),
            json!({
                "model": default_model,
                "name": "Agentdesktop",
            }),
        )];
    }
    config
        .models
        .iter()
        .map(|(id, value)| (id.clone(), value.clone()))
        .collect()
}

fn remove(path: &Path, mode: ReconcileMode) -> anyhow::Result<()> {
    match fs::read(path) {
        Ok(contents) if contents.starts_with(MANAGED_HEADER.as_bytes()) => {
            if mode.writes() {
                fs::remove_file(path).with_context(|| {
                    format!("remove Grok managed configuration at {}", path.display())
                })?;
            }
            info!(
                program = CONFIG_PROGRAM,
                action = "remove",
                path = %path.display(),
                "reconciled managed configuration"
            );
            mode.record(CONFIG_PROGRAM, "configuration", "remove", path);
            Ok(())
        }
        Ok(_) => {
            info!(
                program = CONFIG_PROGRAM,
                action = "unchanged",
                path = %path.display(),
                "preserving managed configuration not owned by Agentdesktop"
            );
            mode.record(CONFIG_PROGRAM, "configuration", "unchanged", path);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            info!(
                program = CONFIG_PROGRAM,
                action = "unchanged",
                path = %path.display(),
                "managed configuration already absent"
            );
            mode.record(CONFIG_PROGRAM, "configuration", "unchanged", path);
            Ok(())
        }
        Err(error) => Err(error)
            .with_context(|| format!("read Grok managed configuration from {}", path.display())),
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
llmGateway:
  url: https://gateway.example.com/proxy
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [grok]
programs:
  grok:
    model: grok-4.6
    models:
      grok-4.6:
        name: Company Grok
        context_window: 128000
        model: ignored-until-overlay
      qwen:
        name: Qwen
        model: Qwen/Qwen3
    managedConfig:
      features:
        telemetry: false
      models:
        default: ignored
"#,
        )
        .expect("valid daemon configuration");
        let grok = config.programs.grok.as_ref().unwrap();
        let gateway = config.llm_gateway.as_ref().unwrap();

        let settings = managed_config(
            grok,
            Some(gateway),
            Path::new("/usr/local/bin/agentdesktop"),
            Path::new("/run/agentdesktop/agentdesktop.sock"),
        )
        .expect("merged settings");

        assert_eq!(settings["features"]["telemetry"], false);
        assert_eq!(settings["models"]["default"], "grok-4.6");
        let model = &settings["model"]["grok-4.6"];
        assert_eq!(model["name"], "Company Grok");
        assert_eq!(model["context_window"], 128000);
        assert_eq!(model["base_url"], "https://gateway.example.com/proxy/v1");
        assert_eq!(model["auth_provider"], "agentdesktop");
        assert_eq!(settings["model"]["qwen"]["model"], "Qwen/Qwen3");
        assert_eq!(
            settings["model"]["qwen"]["base_url"],
            "https://gateway.example.com/proxy/v1"
        );
        let provider = &settings["auth_provider"]["agentdesktop"];
        assert_eq!(provider["command"], "/usr/local/bin/agentdesktop");
        assert_eq!(provider["args"].as_array().unwrap().len(), 5);
        assert_eq!(provider["args"][3], "--client-id");
        assert_eq!(provider["args"][4], "grok");
        assert_eq!(provider["timeout_secs"], 5);

        let serialized = toml::to_string_pretty(&settings).expect("valid TOML");
        let parsed: toml::Value = toml::from_str(&serialized).expect("parse generated TOML");
        assert_eq!(parsed["models"]["default"].as_str(), Some("grok-4.6"));
        assert_eq!(
            parsed["auth_provider"]["agentdesktop"]["command"].as_str(),
            Some("/usr/local/bin/agentdesktop")
        );
    }

    #[test]
    fn generates_a_catalog_entry_when_models_is_omitted() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: http://127.0.0.1:4000
programs:
  grok:
    model: grok-4.6
"#,
        )
        .expect("valid daemon configuration");
        let settings = managed_config(
            config.programs.grok.as_ref().unwrap(),
            config.llm_gateway.as_ref(),
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
        )
        .expect("generated catalog");

        assert_eq!(settings["models"]["default"], "grok-4.6");
        assert_eq!(settings["model"]["grok-4.6"]["model"], "grok-4.6");
        assert_eq!(settings["model"]["grok-4.6"]["name"], "Agentdesktop");
        assert_eq!(
            settings["model"]["grok-4.6"]["base_url"],
            "http://127.0.0.1:4000/v1"
        );
        assert!(settings.get("auth_provider").is_none());
    }

    #[test]
    fn oidc_uses_a_longer_credential_helper_timeout() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: http://127.0.0.1:4001
  authentication:
    type: oidc
    issuer: http://127.0.0.1:5557/dex
    clientId: agentdesktop-local
    allowInsecure: true
programs:
  grok:
    model: grok-4.6
"#,
        )
        .expect("valid daemon configuration");
        let settings = managed_config(
            config.programs.grok.as_ref().unwrap(),
            config.llm_gateway.as_ref(),
            Path::new("/bin/agentdesktop"),
            Path::new("/tmp/agentdesktop.sock"),
        )
        .expect("oidc settings");

        assert_eq!(
            settings["auth_provider"]["agentdesktop"]["timeout_secs"],
            600
        );
    }
}
