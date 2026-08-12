use std::{fs, path::Path};

use agentdesktop_core::config::{
    InferenceGatewayAuthentication, InferenceGatewayConfig, OpenCodeConfig,
};
use anyhow::Context;
use serde_json::{Value, json};
use tracing::info;
use url::Url;

use crate::secure_fs;

const MANAGED_HEADER: &str = "// Managed by AgentDesktop. Manual changes will be replaced.\n";
const CONFIG_PROGRAM: &str = "opencode";

pub fn apply(
    config_path: &Path,
    plugin_path: &Path,
    credential_helper: &Path,
    socket: &Path,
    config: Option<(&OpenCodeConfig, Option<&InferenceGatewayConfig>)>,
) -> anyhow::Result<()> {
    let Some((config, gateway)) = config else {
        remove_owned(config_path, "managed configuration")?;
        return remove_owned(plugin_path, "credential plugin");
    };

    let authentication = gateway.and_then(|gateway| gateway.authentication.as_ref());
    let plugin_url = if matches!(
        authentication,
        Some(InferenceGatewayAuthentication::ControllerJwt { .. })
    ) {
        let source = credential_plugin(credential_helper, socket)?;
        reconcile_file(plugin_path, source.as_bytes(), "credential plugin")?;
        Some(file_url(plugin_path)?)
    } else {
        remove_owned(plugin_path, "credential plugin")?;
        None
    };

    let settings = managed_config(config, gateway, plugin_url.as_deref())?;
    let mut contents = MANAGED_HEADER.as_bytes().to_vec();
    contents.extend_from_slice(
        serde_json::to_string_pretty(&settings)
            .context("serialize OpenCode managed configuration")?
            .as_bytes(),
    );
    contents.push(b'\n');
    reconcile_file(config_path, &contents, "managed configuration")
}

fn managed_config(
    config: &OpenCodeConfig,
    gateway: Option<&InferenceGatewayConfig>,
    plugin_url: Option<&str>,
) -> anyhow::Result<Value> {
    let mut settings = serde_json::to_value(&config.managed_config)
        .context("serialize OpenCode pass-through managed configuration")?;
    let Some(gateway) = gateway else {
        return Ok(settings);
    };

    let model = config
        .model
        .as_deref()
        .context("OpenCode gateway configuration has no model")?;
    let provider_name = "agentdesktop";
    let provider = json!({
        "npm": "@ai-sdk/openai",
        "name": "AgentDesktop",
        "options": {
            "baseURL": responses_base_url(gateway),
            "apiKey": "agentdesktop-managed",
        },
        "models": config.models,
    });
    let generated = json!({
        "$schema": "https://opencode.ai/config.json",
        "enabled_providers": [provider_name],
        "model": format!("{provider_name}/{model}"),
        "provider": {
            (provider_name): provider,
        },
    });
    merge(&mut settings, generated);
    if let Some(plugin_url) = plugin_url {
        append_plugin(&mut settings, plugin_url);
    }
    Ok(settings)
}

fn append_plugin(settings: &mut Value, plugin_url: &str) {
    let plugins = settings
        .as_object_mut()
        .expect("OpenCode settings serialize as an object")
        .entry("plugin")
        .or_insert_with(|| Value::Array(Vec::new()));
    if !plugins.is_array() {
        *plugins = Value::Array(Vec::new());
    }
    let plugins = plugins
        .as_array_mut()
        .expect("plugin was replaced by an array");
    if !plugins
        .iter()
        .any(|value| value.as_str() == Some(plugin_url))
    {
        plugins.push(Value::String(plugin_url.to_owned()));
    }
}

fn credential_plugin(credential_helper: &Path, socket: &Path) -> anyhow::Result<String> {
    let provider_name = "agentdesktop";
    let command = [
        credential_helper.to_string_lossy().into_owned(),
        "--socket".to_owned(),
        socket.to_string_lossy().into_owned(),
        "credential".to_owned(),
        "--client-id".to_owned(),
        "opencode".to_owned(),
    ];
    let provider = serde_json::to_string(&provider_name).context("encode OpenCode provider ID")?;
    let command = serde_json::to_string(&command).context("encode OpenCode credential command")?;

    Ok(format!(
        r#"{MANAGED_HEADER}const provider = {provider};
const command = {command};
let cachedToken = "";
let refreshAfter = 0;

async function credential() {{
  if (cachedToken && Date.now() < refreshAfter) return cachedToken;
  const child = Bun.spawn(command, {{ stdout: "pipe", stderr: "pipe" }});
  const [stdout, stderr, exitCode] = await Promise.all([
    new Response(child.stdout).text(),
    new Response(child.stderr).text(),
    child.exited,
  ]);
  if (exitCode !== 0) {{
    throw new Error(`AgentDesktop credential helper failed: ${{stderr.trim() || `exit ${{exitCode}}`}}`);
  }}
  const token = stdout.trim();
  if (!token) throw new Error("AgentDesktop credential helper returned an empty token");
  cachedToken = token;
  refreshAfter = Date.now() + 60_000;
  return token;
}}

export const AgentDesktop = async () => ({{
  "chat.headers": async (input, output) => {{
    if (input.model.providerID !== provider) return;
    output.headers.Authorization = `Bearer ${{await credential()}}`;
  }},
}});
"#
    ))
}

fn responses_base_url(gateway: &InferenceGatewayConfig) -> String {
    let mut url = gateway.url.clone();
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/v1") {
        url.set_path(&format!("{path}/v1"));
    }
    url.to_string().trim_end_matches('/').to_owned()
}

fn file_url(path: &Path) -> anyhow::Result<String> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .context("locate current directory for OpenCode plugin path")?
            .join(path)
    };
    Url::from_file_path(&absolute)
        .map(|url| url.to_string())
        .map_err(|()| {
            anyhow::anyhow!(
                "convert OpenCode plugin path {} to file URL",
                absolute.display()
            )
        })
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

fn reconcile_file(path: &Path, contents: &[u8], description: &str) -> anyhow::Result<()> {
    let action = match fs::read(path) {
        Ok(existing) if existing == contents => {
            info!(
                program = CONFIG_PROGRAM,
                kind = description,
                action = "unchanged",
                path = %path.display(),
                "managed file already current"
            );
            return Ok(());
        }
        Ok(_) => "update",
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => "create",
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read {description} from {}", path.display()));
        }
    };
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)
        .with_context(|| format!("create OpenCode directory {}", directory.display()))?;
    secure_fs::atomic_write(path, contents, 0o644)?;
    info!(
        program = CONFIG_PROGRAM,
        kind = description,
        action,
        path = %path.display(),
        "reconciled managed file"
    );
    Ok(())
}

fn remove_owned(path: &Path, description: &str) -> anyhow::Result<()> {
    match fs::read(path) {
        Ok(contents) if contents.starts_with(MANAGED_HEADER.as_bytes()) => {
            fs::remove_file(path)
                .with_context(|| format!("remove {description} at {}", path.display()))?;
            info!(
                program = CONFIG_PROGRAM,
                kind = description,
                action = "remove",
                path = %path.display(),
                "reconciled managed file"
            );
            Ok(())
        }
        Ok(_) => {
            info!(
                program = CONFIG_PROGRAM,
                kind = description,
                action = "unchanged",
                path = %path.display(),
                "preserving managed file not owned by AgentDesktop"
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("read {description} from {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use agentdesktop_core::config::parse_desired;

    use super::{credential_plugin, managed_config};

    #[test]
    fn pass_through_settings_are_merged_with_gateway_and_plugin() {
        let desired = parse_desired(
            r#"
inferenceGateway:
  url: https://gateway.example.com/proxy
  authentication:
    type: controllerJwt
    audience: agentgateway
programs:
  openCode:
    model: gpt-company
    models:
      gpt-company:
        name: Company GPT
        limit:
          context: 200000
    managedConfig:
      autoupdate: false
      plugin:
        - opencode-existing-plugin
      provider:
        existing:
          options:
            baseURL: https://existing.example.com/v1
"#,
        )
        .expect("valid desired configuration");
        let open_code = desired.programs.open_code.as_ref().unwrap();
        let gateway = desired.inference_gateway.as_ref().unwrap();
        let settings = managed_config(
            open_code,
            Some(gateway),
            Some("file:///etc/opencode/plugins/agentdesktop.js"),
        )
        .expect("merged settings");

        assert_eq!(settings["autoupdate"], false);
        assert_eq!(settings["model"], "agentdesktop/gpt-company");
        assert_eq!(settings["enabled_providers"][0], "agentdesktop");
        assert_eq!(settings["plugin"][0], "opencode-existing-plugin");
        assert_eq!(
            settings["plugin"][1],
            "file:///etc/opencode/plugins/agentdesktop.js"
        );
        assert_eq!(
            settings["provider"]["existing"]["options"]["baseURL"],
            "https://existing.example.com/v1"
        );
        let provider = &settings["provider"]["agentdesktop"];
        assert_eq!(provider["npm"], "@ai-sdk/openai");
        assert_eq!(
            provider["options"]["baseURL"],
            "https://gateway.example.com/proxy/v1"
        );
        assert_eq!(provider["models"]["gpt-company"]["name"], "Company GPT");
    }

    #[test]
    fn plugin_uses_argument_array_and_scopes_the_header() {
        let plugin = credential_plugin(
            Path::new("/usr/local/bin/agentdesktop"),
            Path::new("/run/agentdesktop/agentdesktop.sock"),
        )
        .expect("credential plugin");

        assert!(plugin.contains(r#"const provider = "agentdesktop";"#));
        assert!(plugin.contains(
            r#"const command = ["/usr/local/bin/agentdesktop","--socket","/run/agentdesktop/agentdesktop.sock","credential","--client-id","opencode"];"#
        ));
        assert!(plugin.contains("input.model.providerID !== provider"));
        assert!(plugin.contains("output.headers.Authorization"));
    }
}
