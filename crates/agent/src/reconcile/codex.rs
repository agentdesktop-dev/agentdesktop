use std::{fs, io::Write, path::Path};

use agentdesktop_core::config::{
    CodexConfig, InferenceGatewayAuthentication, InferenceGatewayConfig,
};
use anyhow::Context;
use serde_json::{Value, json};
use tracing::info;

const MANAGED_HEADER: &str = "# Managed by AgentDesktop. Manual changes will be replaced.\n";

pub fn apply(
    path: &Path,
    credential_helper: &Path,
    socket: &Path,
    config: Option<(&CodexConfig, Option<&InferenceGatewayConfig>)>,
) -> anyhow::Result<()> {
    let Some((config, gateway)) = config else {
        return remove(path);
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

    let action = match fs::read(path) {
        Ok(existing) if existing == contents => {
            info!(
                program = "codex",
                action = "unchanged",
                path = %path.display(),
                "managed configuration already current"
            );
            return Ok(());
        }
        Ok(_) => "update",
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "create",
        Err(error) => {
            return Err(error).with_context(|| {
                format!("read Codex managed configuration from {}", path.display())
            });
        }
    };

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
    let temporary = directory.join(".managed_config.toml.agentdesktop.tmp");
    write_file(&temporary, &contents)?;
    fs::rename(&temporary, path)
        .with_context(|| format!("install Codex managed configuration at {}", path.display()))?;
    info!(
        program = "codex",
        action,
        path = %path.display(),
        "reconciled managed configuration"
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
        "name": "AgentDesktop",
        "base_url": responses_base_url(gateway),
        "wire_api": "responses",
    });
    if matches!(
        gateway.authentication,
        Some(InferenceGatewayAuthentication::ControllerJwt { .. })
    ) {
        provider["auth"] = json!({
            "command": credential_helper.to_string_lossy(),
            "args": [
                "--socket",
                socket.to_string_lossy(),
                "credential",
            ],
            "timeout_ms": 5000,
            "refresh_interval_ms": 60000,
        });
    }
    let generated = json!({
        "model_provider": provider_name,
        "model_providers": {
            (provider_name): provider,
        },
    });
    merge(&mut settings, generated);
    Ok(settings)
}

fn responses_base_url(gateway: &InferenceGatewayConfig) -> String {
    let mut url = gateway.url.clone();
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/v1") {
        url.set_path(&format!("{path}/v1"));
    }
    url.to_string().trim_end_matches('/').to_owned()
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
    match fs::read(path) {
        Ok(contents) if contents.starts_with(MANAGED_HEADER.as_bytes()) => {
            fs::remove_file(path).with_context(|| {
                format!("remove Codex managed configuration at {}", path.display())
            })?;
            info!(
                program = "codex",
                action = "remove",
                path = %path.display(),
                "reconciled managed configuration"
            );
            Ok(())
        }
        Ok(_) => {
            info!(
                program = "codex",
                action = "unchanged",
                path = %path.display(),
                "preserving managed configuration not owned by AgentDesktop"
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            info!(
                program = "codex",
                action = "unchanged",
                path = %path.display(),
                "managed configuration already absent"
            );
            Ok(())
        }
        Err(error) => Err(error)
            .with_context(|| format!("read Codex managed configuration from {}", path.display())),
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
        .with_context(|| format!("write Codex managed configuration to {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write Codex managed configuration to {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use agentdesktop_core::config::parse_desired;

    use super::managed_config;

    #[test]
    fn pass_through_settings_are_merged_with_managed_gateway_values() {
        let desired = parse_desired(
            r#"
inferenceGateway:
  url: https://gateway.example.com/proxy
  authentication:
    type: controllerJwt
    audience: agentgateway
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
        .expect("valid desired configuration");
        let codex = desired.programs.codex.as_ref().unwrap();
        let gateway = desired.inference_gateway.as_ref().unwrap();

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
        assert_eq!(provider["auth"]["args"].as_array().unwrap().len(), 3);
        assert_eq!(provider["auth"]["refresh_interval_ms"], 60000);

        let serialized = toml::to_string_pretty(&settings).expect("valid TOML");
        let parsed: toml::Value = toml::from_str(&serialized).expect("parse generated TOML");
        assert_eq!(parsed["model_provider"].as_str(), Some("agentdesktop"));
    }

    #[test]
    fn does_not_duplicate_an_existing_v1_suffix() {
        let desired = parse_desired(
            r#"
inferenceGateway:
  url: https://gateway.example.com/v1/
programs:
  codex: {}
"#,
        )
        .expect("valid desired configuration");
        let codex = desired.programs.codex.as_ref().unwrap();
        let gateway = desired.inference_gateway.as_ref().unwrap();
        let settings = managed_config(
            codex,
            Some(gateway),
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
        )
        .expect("managed settings");

        assert_eq!(
            settings["model_providers"]["agentdesktop"]["base_url"],
            "https://gateway.example.com/v1"
        );
    }
}
