use super::Pi;

use std::path::Path;

use agentdesktop_core::config::{LlmGatewayAuthentication, LlmGatewayConfig, PiConfig};
use anyhow::Context;
use serde_json::{Map, Value, json};

use crate::provider::{
    json_merge,
    shared::{CommandSpec, deep_merge, render_command, responses_base_url},
};
use crate::reconcile::ReconcilePlan;

const PROVIDER_NAME: &str = "agentdesktop";
const DEFAULT_API: &str = "anthropic-messages";

pub(super) fn plan(
    models_path: &Path,
    settings_path: &Path,
    credential_helper: &Path,
    socket: &Path,
    config: Option<(&PiConfig, Option<&LlmGatewayConfig>)>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let models_state = json_merge::state_path(models_path);
    let settings_state = json_merge::state_path(settings_path);
    let Some((config, gateway)) = config else {
        json_merge::plan_remove(models_path, &models_state, "models", Pi::DISPLAY_NAME, plan)?;
        json_merge::plan_remove(
            settings_path,
            &settings_state,
            "settings",
            Pi::DISPLAY_NAME,
            plan,
        )?;
        return Ok(());
    };

    json_merge::plan_merge(
        models_path,
        &models_state,
        managed_models(config, gateway, credential_helper, socket)?,
        false,
        "models",
        Pi::DISPLAY_NAME,
        plan,
    )?;
    json_merge::plan_merge(
        settings_path,
        &settings_state,
        managed_settings(config, gateway)?,
        false,
        "settings",
        Pi::DISPLAY_NAME,
        plan,
    )?;
    Ok(())
}

fn managed_models(
    config: &PiConfig,
    gateway: Option<&LlmGatewayConfig>,
    credential_helper: &Path,
    socket: &Path,
) -> anyhow::Result<Value> {
    let mut settings = serde_json::to_value(&config.managed_config)
        .context("serialize Pi pass-through managed configuration")?;
    let Some(gateway) = gateway else {
        return Ok(settings);
    };

    let default_model = config
        .model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .context("Pi gateway configuration has no model")?;
    let api = config
        .api
        .as_deref()
        .map(str::trim)
        .filter(|api| !api.is_empty())
        .unwrap_or(DEFAULT_API);
    let uses_credential_helper = gateway
        .authentication
        .as_ref()
        .is_some_and(LlmGatewayAuthentication::uses_credential_helper);
    let api_key = if uses_credential_helper {
        format!(
            "!{}",
            render_command(&CommandSpec::new(
                credential_helper,
                [
                    "--socket",
                    &socket.to_string_lossy(),
                    "credential",
                    "--client-id",
                    Pi::ID,
                ],
            ))
        )
    } else {
        "agentdesktop-managed".to_owned()
    };

    let mut models = Vec::new();
    for (id, mut entry) in catalog_entries(config, default_model) {
        if entry.get("id").and_then(Value::as_str).is_none() {
            entry.insert("id".to_owned(), json!(id));
        }
        models.push(Value::Object(entry));
    }

    let generated = json!({
        "providers": {
            (PROVIDER_NAME): {
                "baseUrl": gateway_base_url(gateway, api),
                "api": api,
                "apiKey": api_key,
                "authHeader": true,
                "models": models,
            }
        }
    });
    deep_merge(&mut settings, generated);
    Ok(settings)
}

fn managed_settings(
    config: &PiConfig,
    gateway: Option<&LlmGatewayConfig>,
) -> anyhow::Result<Value> {
    if gateway.is_none() {
        return Ok(json!({}));
    }
    let model = config
        .model
        .as_deref()
        .filter(|model| !model.trim().is_empty())
        .context("Pi gateway configuration has no model")?;
    Ok(json!({
        "defaultProvider": PROVIDER_NAME,
        "defaultModel": model,
    }))
}

fn catalog_entries(config: &PiConfig, default_model: &str) -> Vec<(String, Map<String, Value>)> {
    if config.models.is_empty() {
        return vec![(default_model.to_owned(), Map::new())];
    }
    config
        .models
        .iter()
        .map(|(id, value)| {
            let object = match value {
                Value::Object(object) => object.clone(),
                _ => Map::new(),
            };
            (id.clone(), object)
        })
        .collect()
}

fn gateway_base_url(gateway: &LlmGatewayConfig, api: &str) -> String {
    match api {
        "openai-completions" | "openai-responses" => responses_base_url(gateway),
        _ => gateway.url.as_str().trim_end_matches('/').to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use agentdesktop_core::config::parse_daemon;

    use super::{managed_models, managed_settings};

    #[test]
    fn pass_through_settings_are_merged_with_managed_gateway_values() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com/proxy
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [pi]
programs:
  pi:
    model: claude-sonnet-4-5
    models:
      claude-sonnet-4-5:
        name: Company Claude
        contextWindow: 200000
      claude-haiku-4-5:
        name: Haiku
    managedConfig:
      providers:
        ollama:
          baseUrl: http://127.0.0.1:11434/v1
"#,
        )
        .expect("valid daemon configuration");
        let pi = config.programs.pi.as_ref().unwrap();
        let gateway = config.llm_gateway.as_ref().unwrap();

        let settings = managed_models(
            pi,
            Some(gateway),
            Path::new("/usr/local/bin/agentdesktop"),
            Path::new("/run/agentdesktop/agentdesktop.sock"),
        )
        .expect("merged settings");

        assert_eq!(
            settings["providers"]["ollama"]["baseUrl"],
            "http://127.0.0.1:11434/v1"
        );
        let provider = &settings["providers"]["agentdesktop"];
        assert_eq!(provider["baseUrl"], "https://gateway.example.com/proxy");
        assert_eq!(provider["api"], "anthropic-messages");
        assert_eq!(provider["authHeader"], true);
        let api_key = provider["apiKey"].as_str().unwrap();
        assert!(api_key.starts_with('!'), "{api_key}");
        #[cfg(unix)]
        {
            assert!(api_key.contains("credential"), "{api_key}");
            assert!(api_key.contains("--client-id"), "{api_key}");
            assert!(api_key.contains("/usr/local/bin/agentdesktop"), "{api_key}");
        }
        let models = provider["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["id"], "claude-haiku-4-5");
        assert_eq!(models[1]["id"], "claude-sonnet-4-5");
        assert_eq!(models[1]["name"], "Company Claude");
        assert_eq!(models[1]["contextWindow"], 200000);

        let startup = managed_settings(pi, Some(gateway)).expect("startup settings");
        assert_eq!(startup["defaultProvider"], "agentdesktop");
        assert_eq!(startup["defaultModel"], "claude-sonnet-4-5");
    }

    #[test]
    fn openai_compatible_gateways_append_v1() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  pi:
    model: local-model
    api: openai-completions
"#,
        )
        .expect("valid daemon configuration");
        let pi = config.programs.pi.as_ref().unwrap();
        let gateway = config.llm_gateway.as_ref().unwrap();
        let settings = managed_models(
            pi,
            Some(gateway),
            Path::new("agentdesktop"),
            Path::new("/tmp/agentdesktop.sock"),
        )
        .expect("merged settings");
        assert_eq!(
            settings["providers"]["agentdesktop"]["baseUrl"],
            "https://gateway.example.com/v1"
        );
        assert_eq!(
            settings["providers"]["agentdesktop"]["api"],
            "openai-completions"
        );
    }
}
