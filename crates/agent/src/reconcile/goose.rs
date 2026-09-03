use std::{fs, path::Path};

use agentdesktop_core::config::{GooseConfig, LlmGatewayAuthentication, LlmGatewayConfig};
use anyhow::Context;
use serde_json::{Value, json};
use tracing::info;

use crate::secure_fs;

use super::{ReconcileMode, deep_merge};

const MANAGED_HEADER: &str = "# Managed by Agentdesktop. Manual changes will be replaced.\n";
const OWNER_MARKER: &[u8] = b"Agentdesktop\n";

pub fn apply(
    config_path: &Path,
    provider_path: &Path,
    credential_helper: &Path,
    socket: &Path,
    config: Option<(&GooseConfig, Option<&LlmGatewayConfig>)>,
    mode: ReconcileMode<'_>,
) -> anyhow::Result<()> {
    let provider_owner_path = owner_path(provider_path);
    let Some((config, gateway)) = config else {
        remove_owned_yaml(config_path, mode)?;
        return remove_owned_json(provider_path, &provider_owner_path, mode);
    };

    if mode.writes() {
        ensure_owned_or_absent(
            config_path,
            |contents| contents.starts_with(MANAGED_HEADER.as_bytes()),
            "configuration",
        )?;
        if gateway.is_some() {
            ensure_owned_or_absent(
                provider_path,
                |_| provider_owner_path.is_file(),
                "provider definition",
            )?;
        }
    }

    let settings = managed_config(config, gateway)?;
    reconcile_yaml(config_path, &settings, mode)?;

    if let Some(gateway) = gateway {
        let provider = managed_provider(config, gateway, credential_helper, socket)?;
        reconcile_json(provider_path, &provider_owner_path, &provider, mode)
    } else {
        remove_owned_json(provider_path, &provider_owner_path, mode)
    }
}

fn managed_config(
    config: &GooseConfig,
    gateway: Option<&LlmGatewayConfig>,
) -> anyhow::Result<Value> {
    let mut settings = serde_json::to_value(&config.managed_config)
        .context("serialize Goose pass-through configuration")?;
    let Some(_gateway) = gateway else {
        return Ok(settings);
    };
    let model = config
        .model
        .as_deref()
        .context("Goose gateway configuration has no model")?;
    deep_merge(
        &mut settings,
        json!({
            "active_provider": "agentdesktop",
            "providers": {
                "agentdesktop": {
                    "enabled": true,
                    "model": model,
                    "configured": true,
                },
            },
        }),
    );
    Ok(settings)
}

fn managed_provider(
    config: &GooseConfig,
    gateway: &LlmGatewayConfig,
    credential_helper: &Path,
    socket: &Path,
) -> anyhow::Result<Value> {
    let model = config
        .model
        .as_deref()
        .context("Goose gateway configuration has no model")?;
    let authentication = gateway.authentication.as_ref();
    let auth = authentication
        .filter(|authentication| authentication.uses_credential_helper())
        .map(|authentication| {
            let timeout_seconds = if matches!(authentication, LlmGatewayAuthentication::Oidc { .. })
            {
                600
            } else {
                10
            };
            json!({
                "command": credential_helper.to_string_lossy(),
                "args": [
                    "--socket",
                    socket.to_string_lossy(),
                    "credential",
                    "--client-id",
                    "goose",
                ],
                "refresh_interval": 60,
                "timeout_seconds": timeout_seconds,
            })
        });

    Ok(json!({
        "name": "agentdesktop",
        "engine": "openai",
        "display_name": "Agentdesktop",
        "description": "Managed Agentdesktop LLM gateway",
        "api_key_env": "",
        "base_url": gateway.url.as_str(),
        "models": [{ "name": model }],
        "headers": null,
        "timeout_seconds": null,
        "supports_streaming": true,
        "requires_auth": authentication.is_some(),
        "catalog_provider_id": null,
        "base_path": null,
        "env_vars": null,
        "auth": auth,
        "dynamic_models": false,
        "skip_canonical_filtering": true,
        "model_doc_link": null,
        "setup_steps": [],
        "preserves_thinking": true,
        "emit_clear_thinking": false,
        "setup": null,
    }))
}

fn reconcile_yaml(path: &Path, settings: &Value, mode: ReconcileMode<'_>) -> anyhow::Result<()> {
    let mut contents = MANAGED_HEADER.as_bytes().to_vec();
    contents.extend_from_slice(
        serde_yaml::to_string(settings)
            .context("serialize Goose managed configuration as YAML")?
            .as_bytes(),
    );
    reconcile_owned(
        path,
        &contents,
        MANAGED_HEADER.as_bytes(),
        "configuration",
        mode,
    )
}

fn reconcile_json(
    path: &Path,
    owner_path: &Path,
    settings: &Value,
    mode: ReconcileMode<'_>,
) -> anyhow::Result<()> {
    let mut contents = serde_json::to_vec_pretty(settings)
        .context("serialize Goose managed provider definition")?;
    contents.push(b'\n');
    let owned = owner_path.is_file();
    let existing = read_optional(path, "Goose managed provider definition")?;
    let action = match existing.as_deref() {
        Some(existing) if existing == contents => "unchanged",
        Some(_) if owned => "update",
        Some(existing) if mode.is_dry_run() => {
            mode.record_diff(
                "goose",
                "provider definition",
                "conflict",
                path,
                Some(existing),
                Some(&contents),
            );
            return Ok(());
        }
        Some(_) => anyhow::bail!(
            "refusing to replace Goose provider definition not owned by Agentdesktop at {}",
            path.display()
        ),
        None => "create",
    };
    mode.record_diff(
        "goose",
        "provider definition",
        action,
        path,
        existing.as_deref(),
        Some(&contents),
    );
    if mode.writes() {
        write_file(path, &contents)?;
        secure_fs::atomic_write(owner_path, OWNER_MARKER, 0o600)?;
    }
    info!(program = "goose", action, path = %path.display(), "reconciled managed provider definition");
    Ok(())
}

fn reconcile_owned(
    path: &Path,
    contents: &[u8],
    marker: &[u8],
    description: &str,
    mode: ReconcileMode<'_>,
) -> anyhow::Result<()> {
    let existing = read_optional(path, "Goose managed configuration")?;
    let action = match existing.as_deref() {
        Some(existing) if existing == contents => "unchanged",
        Some(existing) if existing.starts_with(marker) => "update",
        Some(existing) if mode.is_dry_run() => {
            mode.record_diff(
                "goose",
                description,
                "conflict",
                path,
                Some(existing),
                Some(contents),
            );
            return Ok(());
        }
        Some(_) => anyhow::bail!(
            "refusing to replace Goose configuration not owned by Agentdesktop at {}",
            path.display()
        ),
        None => "create",
    };
    mode.record_diff(
        "goose",
        description,
        action,
        path,
        existing.as_deref(),
        Some(contents),
    );
    if mode.writes() && action != "unchanged" {
        write_file(path, contents)?;
    }
    info!(program = "goose", action, path = %path.display(), "reconciled managed configuration");
    Ok(())
}

fn remove_owned_yaml(path: &Path, mode: ReconcileMode<'_>) -> anyhow::Result<()> {
    remove_file_if_owned(
        path,
        |contents| contents.starts_with(MANAGED_HEADER.as_bytes()),
        "configuration",
        mode,
    )
}

fn remove_owned_json(
    path: &Path,
    owner_path: &Path,
    mode: ReconcileMode<'_>,
) -> anyhow::Result<()> {
    let owned = owner_path.is_file();
    remove_file_if_owned(path, |_| owned, "provider definition", mode)?;
    if mode.writes() && owned {
        match fs::remove_file(owner_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove Goose provider ownership marker"),
        }
    }
    Ok(())
}

fn remove_file_if_owned(
    path: &Path,
    owned: impl FnOnce(&[u8]) -> bool,
    description: &str,
    mode: ReconcileMode<'_>,
) -> anyhow::Result<()> {
    let Some(contents) = read_optional(path, "Goose managed file")? else {
        mode.record("goose", description, "unchanged", path);
        return Ok(());
    };
    if !owned(&contents) {
        mode.record("goose", description, "unchanged", path);
        return Ok(());
    }
    mode.record_diff("goose", description, "remove", path, Some(&contents), None);
    if mode.writes() {
        fs::remove_file(path)
            .with_context(|| format!("remove Goose {description} at {}", path.display()))?;
    }
    info!(program = "goose", action = "remove", path = %path.display(), "reconciled managed file");
    Ok(())
}

fn read_optional(path: &Path, display_name: &str) -> anyhow::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(contents) => Ok(Some(contents)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("read {display_name} from {}", path.display()))
        }
    }
}

fn ensure_owned_or_absent(
    path: &Path,
    owned: impl FnOnce(&[u8]) -> bool,
    description: &str,
) -> anyhow::Result<()> {
    if let Some(contents) = read_optional(path, "Goose managed file")?
        && !owned(&contents)
    {
        anyhow::bail!(
            "refusing to replace Goose {description} not owned by Agentdesktop at {}",
            path.display()
        );
    }
    Ok(())
}

fn write_file(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory).with_context(|| {
        format!(
            "create Goose configuration directory {}",
            directory.display()
        )
    })?;
    secure_fs::atomic_write(path, contents, 0o644)
}

fn owner_path(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("agentdesktop.json");
    path.with_file_name(format!(".{name}.owner"))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use agentdesktop_core::config::parse_daemon;

    use super::{apply, managed_config, managed_provider, owner_path};
    use crate::reconcile::ReconcileMode;

    fn temp_root(test: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "agentdesktop-goose-{test}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    #[test]
    fn gateway_config_uses_command_auth_and_selected_model() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com/proxy
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [goose]
programs:
  goose:
    model: company-model
    managedConfig:
      GOOSE_MODE: smart_approve
"#,
        )
        .expect("valid daemon configuration");
        let goose = config.programs.goose.as_ref().unwrap();
        let gateway = config.llm_gateway.as_ref().unwrap();

        let settings = managed_config(goose, Some(gateway)).expect("managed config");
        assert_eq!(settings["active_provider"], "agentdesktop");
        assert_eq!(
            settings["providers"]["agentdesktop"]["model"],
            "company-model"
        );
        assert_eq!(settings["GOOSE_MODE"], "smart_approve");

        let provider = managed_provider(
            goose,
            gateway,
            Path::new("/usr/local/bin/agentdesktop"),
            Path::new("/run/agentdesktop/agentdesktop.sock"),
        )
        .expect("managed provider");
        assert_eq!(provider["base_url"], "https://gateway.example.com/proxy");
        assert_eq!(provider["models"][0]["name"], "company-model");
        assert_eq!(provider["auth"]["command"], "/usr/local/bin/agentdesktop");
        assert_eq!(provider["auth"]["args"][4], "goose");
        assert_eq!(provider["auth"]["refresh_interval"], 60);
        assert_eq!(provider["requires_auth"], true);
    }

    #[test]
    fn unauthenticated_gateway_omits_command_auth() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: http://127.0.0.1:4000
programs:
  goose:
    model: test-model
"#,
        )
        .expect("valid daemon configuration");
        let provider = managed_provider(
            config.programs.goose.as_ref().unwrap(),
            config.llm_gateway.as_ref().unwrap(),
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
        )
        .expect("managed provider");

        assert!(provider["auth"].is_null());
        assert_eq!(provider["requires_auth"], false);
    }

    #[test]
    fn reconciliation_creates_and_removes_only_owned_files() {
        let root = temp_root("owned");
        let config_path = root.join("goose/config.yaml");
        let provider_path = root.join("goose/custom_providers/agentdesktop.json");
        let config = parse_daemon(
            r#"
llmGateway:
  url: http://127.0.0.1:4000
programs:
  goose:
    model: test-model
"#,
        )
        .unwrap();
        let goose = config.programs.goose.as_ref().unwrap();
        let gateway = config.llm_gateway.as_ref().unwrap();

        apply(
            &config_path,
            &provider_path,
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
            Some((goose, Some(gateway))),
            ReconcileMode::Apply,
        )
        .unwrap();

        assert!(
            fs::read_to_string(&config_path)
                .unwrap()
                .starts_with("# Managed by Agentdesktop.")
        );
        assert!(provider_path.is_file());
        assert!(owner_path(&provider_path).is_file());

        apply(
            &config_path,
            &provider_path,
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
            None,
            ReconcileMode::Apply,
        )
        .unwrap();

        assert!(!config_path.exists());
        assert!(!provider_path.exists());
        assert!(!owner_path(&provider_path).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reconciliation_preserves_unowned_goose_configuration() {
        let root = temp_root("unowned");
        fs::create_dir_all(&root).unwrap();
        let config_path = root.join("config.yaml");
        let provider_path = root.join("agentdesktop.json");
        fs::write(&config_path, "GOOSE_MODE: chat\n").unwrap();
        let config = parse_daemon(
            r#"
programs:
  goose:
    useLlmGateway: false
"#,
        )
        .unwrap();

        let error = apply(
            &config_path,
            &provider_path,
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
            Some((config.programs.goose.as_ref().unwrap(), None)),
            ReconcileMode::Apply,
        )
        .expect_err("unowned configuration must be preserved");

        assert!(error.to_string().contains("not owned by Agentdesktop"));
        assert_eq!(
            fs::read_to_string(config_path).unwrap(),
            "GOOSE_MODE: chat\n"
        );
        assert!(!provider_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn provider_conflict_is_detected_before_writing_configuration() {
        let root = temp_root("provider-conflict");
        fs::create_dir_all(&root).unwrap();
        let config_path = root.join("config.yaml");
        let provider_path = root.join("agentdesktop.json");
        fs::write(&provider_path, "{}\n").unwrap();
        let config = parse_daemon(
            r#"
llmGateway:
  url: http://127.0.0.1:4000
programs:
  goose:
    model: test-model
"#,
        )
        .unwrap();

        let error = apply(
            &config_path,
            &provider_path,
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
            Some((
                config.programs.goose.as_ref().unwrap(),
                config.llm_gateway.as_ref(),
            )),
            ReconcileMode::Apply,
        )
        .expect_err("unowned provider must be preserved");

        assert!(error.to_string().contains("provider definition"));
        assert!(!config_path.exists());
        assert_eq!(fs::read_to_string(&provider_path).unwrap(), "{}\n");
        fs::remove_dir_all(root).unwrap();
    }
}
