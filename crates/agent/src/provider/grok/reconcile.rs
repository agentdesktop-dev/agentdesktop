use super::Grok;

use std::path::Path;

use agentdesktop_core::config::{GrokConfig, LlmGatewayAuthentication, LlmGatewayConfig};
use anyhow::Context;
use serde_json::{Map, Value, json};
use tracing::debug;

use crate::provider::shared::{deep_merge, responses_base_url};
use crate::reconcile::ReconcilePlan;

const MANAGED_HEADER: &str = "# Managed by agentdesktop. Manual changes will be replaced.\n";
const PROVIDER_NAME: &str = "agentdesktop";

pub(super) fn plan(
    path: &Path,
    credential_helper: &Path,
    socket: &Path,
    config: Option<(&GrokConfig, Option<&LlmGatewayConfig>)>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let Some((config, gateway)) = config else {
        return remove(path, plan);
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

    let existing = match plan.read(path) {
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
            debug!(
                program = Grok::ID,
                action = "unchanged",
                path = %path.display(),
                "managed configuration already current"
            );
            plan.record(Grok::DISPLAY_NAME, "configuration", "unchanged", path);
            return Ok(());
        }
        Some(existing) if existing.starts_with(MANAGED_HEADER.as_bytes()) => "update",
        Some(existing) => {
            plan.record_diff(
                Grok::DISPLAY_NAME,
                "configuration",
                "conflict",
                path,
                Some(existing),
                Some(&contents),
            );
            return Ok(());
        }
        None => "create",
    };

    plan.write_file(path, &contents, 0o644)?;
    debug!(
        program = Grok::ID,
        action,
        path = %path.display(),
        "planned managed configuration"
    );
    plan.record_diff(
        Grok::DISPLAY_NAME,
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
    let uses_credential_helper = gateway
        .authentication
        .as_ref()
        .is_some_and(LlmGatewayAuthentication::uses_credential_helper);
    let mut models = Map::new();
    for (id, value) in &catalog {
        let mut entry = match value {
            Value::Object(_) => value.clone(),
            _ => json!({}),
        };
        if entry.get("model").and_then(Value::as_str).is_none() {
            entry["model"] = json!(id);
        }
        entry["base_url"] = json!(responses_base_url(gateway));
        if uses_credential_helper {
            entry["auth_provider"] = json!(PROVIDER_NAME);
        }
        models.insert(id.clone(), entry);
    }

    let mut generated = json!({
        "models": { "default": default_model },
        "model": models,
    });
    if uses_credential_helper {
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
                    Grok::ID,
                ],
                "timeout_secs": timeout_secs,
            },
        });
    }
    deep_merge(&mut settings, generated);
    if uses_credential_helper {
        // Grok resolves static credentials before auth_provider. Clear both
        // sources after merging so pass-through values cannot shadow our helper.
        for (id, _) in catalog {
            if let Some(entry) = settings["model"][&id].as_object_mut() {
                entry.remove("api_key");
                entry.remove("env_key");
            }
        }
    }
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

fn remove(path: &Path, plan: &ReconcilePlan) -> anyhow::Result<()> {
    match plan.read(path) {
        Ok(contents) if contents.starts_with(MANAGED_HEADER.as_bytes()) => {
            plan.remove_file(path).with_context(|| {
                format!("remove Grok managed configuration at {}", path.display())
            })?;
            debug!(
                program = Grok::ID,
                action = "remove",
                path = %path.display(),
                "planned managed configuration"
            );
            plan.record(Grok::DISPLAY_NAME, "configuration", "remove", path);
            Ok(())
        }
        Ok(_) => {
            debug!(
                program = Grok::ID,
                action = "unchanged",
                path = %path.display(),
                "preserving managed configuration not owned by Agentdesktop"
            );
            plan.record(Grok::DISPLAY_NAME, "configuration", "unchanged", path);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            debug!(
                program = Grok::ID,
                action = "unchanged",
                path = %path.display(),
                "managed configuration already absent"
            );
            plan.record(Grok::DISPLAY_NAME, "configuration", "unchanged", path);
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
    fn gateway_credentials_override_static_credentials_from_both_config_maps() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [grok]
programs:
  grok:
    model: grok-4.6
    models:
      grok-4.6:
        api_key: catalog-key
        env_key: CATALOG_KEY
      secondary:
        model: another-model
    managedConfig:
      model:
        grok-4.6:
          api_key: pass-through-key
          env_key: PASS_THROUGH_KEY
          context_window: 128000
        secondary:
          api_key: secondary-key
          env_key: [SECONDARY_KEY]
        unmanaged:
          api_key: unmanaged-key
"#,
        )
        .unwrap();
        let grok = config.programs.grok.as_ref().unwrap();
        let settings = managed_config(
            grok,
            config.llm_gateway.as_ref(),
            Path::new("/bin/agentdesktop"),
            Path::new("/tmp/agentdesktop.sock"),
        )
        .unwrap();

        for id in ["grok-4.6", "secondary"] {
            let model = &settings["model"][id];
            assert_eq!(model["auth_provider"], "agentdesktop");
            assert!(
                model.get("api_key").is_none(),
                "static key shadows helper for {id}"
            );
            assert!(
                model.get("env_key").is_none(),
                "environment key shadows helper for {id}"
            );
        }
        assert_eq!(settings["model"]["grok-4.6"]["context_window"], 128000);
        assert_eq!(settings["model"]["unmanaged"]["api_key"], "unmanaged-key");

        // The synthesized default entry must also clear pass-through credentials.
        let mut grok = grok.clone();
        grok.models.clear();
        let settings = managed_config(
            &grok,
            config.llm_gateway.as_ref(),
            Path::new("/bin/agentdesktop"),
            Path::new("/tmp/agentdesktop.sock"),
        )
        .unwrap();
        assert!(settings["model"]["grok-4.6"].get("api_key").is_none());
        assert!(settings["model"]["grok-4.6"].get("env_key").is_none());
    }

    #[test]
    fn static_credentials_are_preserved_without_a_gateway_credential_helper() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  grok:
    model: custom
    models:
      custom:
        api_key: catalog-key
        env_key: CATALOG_KEY
    managedConfig:
      model:
        custom:
          api_key: pass-through-key
          env_key: PASS_THROUGH_KEY
"#,
        )
        .unwrap();
        let grok = config.programs.grok.as_ref().unwrap();
        for (gateway, expected_key, expected_env) in [
            (config.llm_gateway.as_ref(), "catalog-key", "CATALOG_KEY"),
            (None, "pass-through-key", "PASS_THROUGH_KEY"),
        ] {
            let settings = managed_config(
                grok,
                gateway,
                Path::new("/bin/agentdesktop"),
                Path::new("/tmp/agentdesktop.sock"),
            )
            .unwrap();
            assert_eq!(settings["model"]["custom"]["api_key"], expected_key);
            assert_eq!(settings["model"]["custom"]["env_key"], expected_env);
            assert!(settings["model"]["custom"].get("auth_provider").is_none());
        }
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
