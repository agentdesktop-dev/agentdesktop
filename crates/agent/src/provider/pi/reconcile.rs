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
const MODELS_OPTIONS: json_merge::MergeOptions = json_merge::MergeOptions {
    format: json_merge::JsonFormat::Jsonc,
    permissions: 0o600,
    replace_paths: &[],
};

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
        json_merge::plan_remove(
            models_path,
            &models_state,
            MODELS_OPTIONS,
            "models",
            Pi::DISPLAY_NAME,
            plan,
        )?;
        json_merge::plan_remove(
            settings_path,
            &settings_state,
            json_merge::MergeOptions::default(),
            "settings",
            Pi::DISPLAY_NAME,
            plan,
        )?;
        return Ok(());
    };

    json_merge::plan_merge(
        models_path,
        &models_state,
        json_merge::MergeOptions {
            replace_paths: if gateway.is_some() {
                &["/providers/agentdesktop/models"]
            } else {
                &[]
            },
            ..MODELS_OPTIONS
        },
        managed_models(config, gateway, credential_helper, socket)?,
        false,
        "models",
        Pi::DISPLAY_NAME,
        plan,
    )?;
    json_merge::plan_merge(
        settings_path,
        &settings_state,
        json_merge::MergeOptions::default(),
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
        entry.insert("id".to_owned(), json!(id));
        let model_api = entry.get("api").and_then(Value::as_str).unwrap_or(api);
        let base_url = gateway_base_url(gateway, model_api);
        entry.insert("baseUrl".to_owned(), json!(base_url));
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
    use std::fs;
    use std::path::Path;

    use agentdesktop_core::config::parse_daemon;
    use serde_json::{Value, json};

    use super::{managed_models, managed_settings, plan};
    use crate::reconcile::ReconcilePlan;

    fn reconcile_fixture(models: &Path, config: Option<&str>) -> ReconcilePlan {
        let config = config.map(|yaml| parse_daemon(yaml).unwrap());
        let changes = ReconcilePlan::default();
        plan(
            models,
            &models.with_file_name("settings.json"),
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
            config.as_ref().map(|config| {
                (
                    config.programs.pi.as_ref().unwrap(),
                    config.llm_gateway.as_ref(),
                )
            }),
            &changes,
        )
        .unwrap();
        changes
    }

    #[cfg(unix)]
    #[test]
    fn models_create_and_merge_use_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        for existing_mode in [None, Some(0o600), Some(0o644)] {
            let root = tempfile::tempdir().unwrap();
            let models = root.path().join("models.json");
            if let Some(mode) = existing_mode {
                fs::write(
                    &models,
                    r#"{"providers":{"personal":{"apiKey":"test-secret"}}}"#,
                )
                .unwrap();
                fs::set_permissions(&models, fs::Permissions::from_mode(mode)).unwrap();
            }
            let changes = reconcile_fixture(&models, Some("programs:\n  pi: {}\n"));
            if let Some(mode) = existing_mode {
                assert_eq!(
                    fs::metadata(&models).unwrap().permissions().mode() & 0o777,
                    mode
                );
            } else {
                assert!(!models.exists(), "planning must not write files");
            }
            changes.apply().unwrap();
            assert_eq!(
                fs::metadata(&models).unwrap().permissions().mode() & 0o077,
                0
            );
            if existing_mode.is_some() {
                let merged: Value = serde_json::from_slice(&fs::read(&models).unwrap()).unwrap();
                assert_eq!(merged["providers"]["personal"]["apiKey"], "test-secret");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn models_cleanup_keeps_personal_credentials_private() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let models = root.path().join("models.json");
        let original = json!({"providers":{"personal":{"apiKey":"test-secret"}}});
        fs::write(&models, serde_json::to_vec(&original).unwrap()).unwrap();
        reconcile_fixture(&models, Some("llmGateway:\n  url: https://gateway.example.com\nprograms:\n  pi:\n    model: selected\n"))
            .apply().unwrap();
        fs::set_permissions(&models, fs::Permissions::from_mode(0o600)).unwrap();

        reconcile_fixture(&models, None).apply().unwrap();
        assert_eq!(
            fs::metadata(&models).unwrap().permissions().mode() & 0o077,
            0
        );
        let restored: Value = serde_json::from_slice(&fs::read(&models).unwrap()).unwrap();
        assert_eq!(restored, original);
    }

    #[cfg(unix)]
    #[test]
    fn models_reconcile_repairs_permissions_without_content_changes() {
        use std::os::unix::fs::PermissionsExt;

        // Cover both an active provider and cleanup of a previous merge.
        for cleanup in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let models = root.path().join("models.json");
            fs::write(
                &models,
                r#"{"providers":{"personal":{"apiKey":"test-secret"}}}"#,
            )
            .unwrap();
            let config = Some("programs:\n  pi: {}\n");
            reconcile_fixture(&models, config).apply().unwrap();
            let before = fs::read(&models).unwrap();
            // Simulate a file written by the old 0644 merge implementation.
            fs::set_permissions(&models, fs::Permissions::from_mode(0o644)).unwrap();
            let changes = reconcile_fixture(&models, if cleanup { None } else { config });
            assert!(
                changes.render().contains("UPDATE  Pi models"),
                "{}",
                changes.render()
            );
            changes.apply().unwrap();
            assert_eq!(fs::read(&models).unwrap(), before);
            assert_eq!(
                fs::metadata(&models).unwrap().permissions().mode() & 0o077,
                0
            );
            if !cleanup {
                let repeated = reconcile_fixture(&models, config);
                assert!(repeated.render().contains("Summary: 0 changes"));
                repeated.apply().unwrap();
            }
        }
    }

    #[test]
    fn gateway_catalog_replaces_existing_models_and_restores_on_removal() {
        let root = tempfile::tempdir().unwrap();
        let models = root.path().join("models.json");
        let original = json!({"providers":{
            "agentdesktop": {
                "baseUrl":"https://old.example.com",
                "apiKey":"test-secret",
                "models":[
                    {"id":"selected","baseUrl":"https://gateway.example.com"},
                    {"id":"selected","baseUrl":"https://outside.example.com"},
                    {"id":"stale","baseUrl":"https://outside.example.com"}
                ]
            },
            "personal":{"models":[{"id":"personal-model"}],"apiKey":"personal-secret"}
        }});
        fs::write(&models, serde_json::to_vec(&original).unwrap()).unwrap();

        for selected in ["selected", "next"] {
            let config = format!(
                "llmGateway:\n  url: https://gateway.example.com\nprograms:\n  pi:\n    model: {selected}\n"
            );
            reconcile_fixture(&models, Some(&config)).apply().unwrap();
            let merged: Value = serde_json::from_slice(&fs::read(&models).unwrap()).unwrap();
            assert_eq!(
                merged["providers"]["agentdesktop"]["models"],
                json!([
                    {"id":selected,"baseUrl":"https://gateway.example.com"}
                ])
            );
            assert_eq!(
                merged["providers"]["personal"],
                original["providers"]["personal"]
            );
            let repeated = reconcile_fixture(&models, Some(&config));
            assert!(repeated.render().contains("Summary: 0 changes"));
            repeated.apply().unwrap();
        }

        // Unrelated user edits made while managed must survive cleanup.
        let mut edited: Value = serde_json::from_slice(&fs::read(&models).unwrap()).unwrap();
        edited["providers"]["personal"]["apiKey"] = json!("updated-personal-secret");
        fs::write(&models, serde_json::to_vec(&edited).unwrap()).unwrap();
        reconcile_fixture(&models, None).apply().unwrap();
        let restored: Value = serde_json::from_slice(&fs::read(&models).unwrap()).unwrap();
        assert_eq!(
            restored["providers"]["agentdesktop"],
            original["providers"]["agentdesktop"]
        );
        assert_eq!(
            restored["providers"]["personal"]["apiKey"],
            "updated-personal-secret"
        );
        assert!(!super::json_merge::state_path(&models).exists());
    }

    #[test]
    fn cleanup_restores_catalog_order_after_user_additions() {
        for restore_original in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let models = root.path().join("models.json");
            // Pi resolves duplicate IDs using the last entry. The original catalog
            // must keep that ordering even if one entry matches the managed model.
            let original_catalog = json!([
                {"id":"selected","baseUrl":"https://outside.example.com"},
                {"id":"selected","baseUrl":"https://gateway.example.com"},
                {"id":"selected","baseUrl":"https://gateway.example.com"}
            ]);
            fs::write(
                &models,
                serde_json::to_vec(&json!({"providers":{"agentdesktop":{
                    "models":original_catalog
                }}}))
                .unwrap(),
            )
            .unwrap();
            reconcile_fixture(&models, Some("llmGateway:\n  url: https://gateway.example.com\nprograms:\n  pi:\n    model: selected\n"))
            .apply().unwrap();
            let mut edited: Value = serde_json::from_slice(&fs::read(&models).unwrap()).unwrap();
            let addition = json!({"id":"added","baseUrl":"https://added.example.com"});
            if restore_original {
                // A user may restore an original entry before disabling management.
                edited["providers"]["agentdesktop"]["models"]
                    .as_array_mut()
                    .unwrap()
                    .push(original_catalog[0].clone());
            }
            edited["providers"]["agentdesktop"]["models"]
                .as_array_mut()
                .unwrap()
                .push(addition.clone());
            fs::write(&models, serde_json::to_vec(&edited).unwrap()).unwrap();

            reconcile_fixture(&models, None).apply().unwrap();
            let restored: Value = serde_json::from_slice(&fs::read(&models).unwrap()).unwrap();
            let mut expected = original_catalog;
            expected.as_array_mut().unwrap().push(addition);
            assert_eq!(restored["providers"]["agentdesktop"]["models"], expected);
        }
    }

    #[test]
    fn disabling_gateway_restores_catalog_and_preserves_user_additions() {
        let root = tempfile::tempdir().unwrap();
        let models = root.path().join("models.json");
        let original_model = json!({"id":"original","baseUrl":"https://original.example.com"});
        fs::write(
            &models,
            serde_json::to_vec(&json!({"providers":{"agentdesktop":{
                "models":[original_model.clone()]
            }}}))
            .unwrap(),
        )
        .unwrap();
        reconcile_fixture(&models, Some("llmGateway:\n  url: https://gateway.example.com\nprograms:\n  pi:\n    model: selected\n"))
            .apply().unwrap();
        let mut edited: Value = serde_json::from_slice(&fs::read(&models).unwrap()).unwrap();
        assert_eq!(
            edited["providers"]["agentdesktop"]["models"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let added_model = json!({"id":"added","baseUrl":"https://added.example.com"});
        edited["providers"]["agentdesktop"]["models"]
            .as_array_mut()
            .unwrap()
            .push(added_model.clone());
        fs::write(&models, serde_json::to_vec(&edited).unwrap()).unwrap();

        reconcile_fixture(
            &models,
            Some("programs:\n  pi:\n    useLlmGateway: false\n"),
        )
        .apply()
        .unwrap();
        let restored: Value = serde_json::from_slice(&fs::read(&models).unwrap()).unwrap();
        let catalog = restored["providers"]["agentdesktop"]["models"]
            .as_array()
            .unwrap();
        assert_eq!(catalog.len(), 2);
        assert!(catalog.contains(&original_model));
        assert!(catalog.contains(&added_model));
        reconcile_fixture(&models, None).apply().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(&models).unwrap()).unwrap(),
            restored
        );
    }

    #[test]
    fn gateway_overrides_per_model_endpoints() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com/proxy
programs:
  pi:
    model: claude
    models:
      claude:
        baseUrl: https://outside.example.com
      chat:
        api: openai-completions
        baseUrl: https://outside.example.com/v1
      responses:
        api: openai-responses
"#,
        )
        .unwrap();
        let managed = managed_models(
            config.programs.pi.as_ref().unwrap(),
            config.llm_gateway.as_ref(),
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
        )
        .unwrap();
        for model in managed["providers"]["agentdesktop"]["models"]
            .as_array()
            .unwrap()
        {
            let expected = if model["id"] == "claude" {
                "https://gateway.example.com/proxy"
            } else {
                "https://gateway.example.com/proxy/v1"
            };
            assert_eq!(model["baseUrl"], expected, "model {}", model["id"]);
        }
    }

    #[test]
    fn model_map_keys_define_catalog_ids() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  pi:
    model: selected
    models:
      selected:
        id: different
        name: Selected Model
"#,
        )
        .unwrap();
        let pi = config.programs.pi.as_ref().unwrap();
        let gateway = config.llm_gateway.as_ref();
        let managed = managed_models(
            pi,
            gateway,
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
        )
        .unwrap();
        let settings = managed_settings(pi, gateway).unwrap();
        let model = &managed["providers"]["agentdesktop"]["models"][0];
        assert_eq!(model["id"], settings["defaultModel"]);
        assert_eq!(model["name"], "Selected Model");
    }

    #[test]
    fn commented_models_survive_merge_repeat_and_removal() {
        let root = tempfile::tempdir().unwrap();
        let models = root.path().join("models.json");
        let settings = root.path().join("settings.json");
        fs::write(
            &models,
            "\u{feff}{\n // Personal provider\n \"providers\": {\"local\": {\"baseUrl\": \"http://localhost:11434/v1\",},},\n}\n",
        )
        .unwrap();
        fs::write(&settings, r#"{"theme":"dark"}"#).unwrap();
        let config = parse_daemon(
            "llmGateway:\n  url: https://gateway.example.com\nprograms:\n  pi:\n    model: selected\n",
        )
        .unwrap();
        let configured = Some((
            config.programs.pi.as_ref().unwrap(),
            config.llm_gateway.as_ref(),
        ));
        let helper = root.path().join("agentdesktop");
        let socket = root.path().join("agentdesktop.sock");
        let changes = ReconcilePlan::default();
        plan(&models, &settings, &helper, &socket, configured, &changes).unwrap();
        assert!(!changes.has_conflicts(), "Pi accepts commented models.json");
        changes.apply().unwrap();
        let merged: Value = serde_json::from_slice(&fs::read(&models).unwrap()).unwrap();
        assert_eq!(
            merged["providers"]["local"]["baseUrl"],
            "http://localhost:11434/v1"
        );
        assert_eq!(
            merged["providers"]["agentdesktop"]["models"][0]["id"],
            "selected"
        );

        let before = fs::read(&models).unwrap();
        let repeated = ReconcilePlan::default();
        plan(&models, &settings, &helper, &socket, configured, &repeated).unwrap();
        assert!(repeated.render().contains("Summary: 0 changes"));
        repeated.apply().unwrap();
        assert_eq!(fs::read(&models).unwrap(), before);

        // A user can add comments while agentdesktop manages the file.
        fs::write(
            &models,
            format!("// Updated comment\n{}", String::from_utf8(before).unwrap()),
        )
        .unwrap();
        let removal = ReconcilePlan::default();
        plan(&models, &settings, &helper, &socket, None, &removal).unwrap();
        removal.apply().unwrap();
        let restored: Value = serde_json::from_slice(&fs::read(&models).unwrap()).unwrap();
        assert_eq!(
            restored,
            json!({"providers":{"local":{"baseUrl":"http://localhost:11434/v1"}}})
        );
        let restored_settings: Value =
            serde_json::from_slice(&fs::read(&settings).unwrap()).unwrap();
        assert_eq!(restored_settings, json!({"theme":"dark"}));
        assert!(!super::json_merge::state_path(&models).exists());
        assert!(!super::json_merge::state_path(&settings).exists());
    }

    #[test]
    fn commented_settings_remain_a_conflict() {
        let root = tempfile::tempdir().unwrap();
        let models = root.path().join("models.json");
        let settings = root.path().join("settings.json");
        let existing = b"// Pi settings require strict JSON\n{\"theme\":\"dark\"}";
        fs::write(&settings, existing).unwrap();
        let config = parse_daemon("programs:\n  pi: {}\n").unwrap();
        let changes = ReconcilePlan::default();
        plan(
            &models,
            &settings,
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
            Some((config.programs.pi.as_ref().unwrap(), None)),
            &changes,
        )
        .unwrap();
        assert!(changes.has_conflicts());
        assert!(changes.apply().is_err());
        assert_eq!(fs::read(&settings).unwrap(), existing);
        assert!(!models.exists());
    }

    #[test]
    fn malformed_models_prevent_settings_writes() {
        let root = tempfile::tempdir().unwrap();
        let models = root.path().join("models.json");
        let settings = root.path().join("settings.json");
        let invalid = b"{\"providers\":";
        fs::write(&models, invalid).unwrap();
        let config = parse_daemon("programs:\n  pi: {}\n").unwrap();
        let changes = ReconcilePlan::default();
        plan(
            &models,
            &settings,
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
            Some((config.programs.pi.as_ref().unwrap(), None)),
            &changes,
        )
        .unwrap();
        assert!(changes.has_conflicts());
        assert!(changes.apply().is_err());
        assert_eq!(fs::read(&models).unwrap(), invalid);
        assert!(!settings.exists());
    }

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
