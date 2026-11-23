use super::OpenCode;

use std::path::{Path, PathBuf};

use agentdesktop_core::config::{
    LlmGatewayAuthentication, LlmGatewayConfig, OpenCodeApi, OpenCodeConfig,
};
use anyhow::Context;
use serde_json::{Value, json};
use tracing::debug;
use url::Url;

use crate::reconcile::ReconcilePlan;

use crate::provider::shared::{deep_merge, responses_base_url};

/// Mode of the managed file; a looser mode is repaired on the next apply.
const MANAGED_FILE_MODE: u32 = 0o644;

const MANAGED_HEADER: &str = "// Managed by Agentdesktop. Manual changes will be replaced.\n";
const CONFIG_PROGRAM: &str = OpenCode::ID;

pub(super) fn plan(
    config_path: &Path,
    plugin_path: &Path,
    credential_helper: &Path,
    socket: &Path,
    config: Option<(&OpenCodeConfig, Option<&LlmGatewayConfig>)>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let plugin_v2_path = credential_plugin_v2_path(plugin_path);
    let Some((config, gateway)) = config else {
        remove_owned(config_path, "managed configuration", plan)?;
        remove_owned(plugin_path, "credential plugin", plan)?;
        return remove_owned(&plugin_v2_path, "credential plugin (OpenCode 2)", plan);
    };

    let authentication = gateway.and_then(|gateway| gateway.authentication.as_ref());
    let plugin_urls =
        if authentication.is_some_and(LlmGatewayAuthentication::uses_credential_helper) {
            let source = credential_plugin(credential_helper, socket)?;
            reconcile_file(plugin_path, source.as_bytes(), "credential plugin", plan)?;
            let source = credential_plugin_v2(credential_helper, socket)?;
            reconcile_file(
                &plugin_v2_path,
                source.as_bytes(),
                "credential plugin (OpenCode 2)",
                plan,
            )?;
            let plugin_v2_dir = plugin_v2_path
                .parent()
                .expect("OpenCode 2 plugin entrypoint has a directory");
            Some(PluginUrls {
                v1: file_url(plugin_path)?,
                v2: file_url(plugin_v2_dir)?,
            })
        } else {
            remove_owned(plugin_path, "credential plugin", plan)?;
            remove_owned(&plugin_v2_path, "credential plugin (OpenCode 2)", plan)?;
            None
        };

    let settings = managed_config(config, gateway, plugin_urls.as_ref())?;
    let mut contents = MANAGED_HEADER.as_bytes().to_vec();
    contents.extend_from_slice(
        serde_json::to_string_pretty(&settings)
            .context("serialize OpenCode managed configuration")?
            .as_bytes(),
    );
    contents.push(b'\n');
    reconcile_file(config_path, &contents, "managed configuration", plan)
}

/// Configuration entries for the credential plugin, one per OpenCode plugin API.
struct PluginUrls {
    /// OpenCode 1: the plugin file, listed under `plugin`.
    v1: String,
    /// OpenCode 2: the plugin directory, listed under `plugins`. OpenCode 1
    /// ignores `plugins` and OpenCode 2 skips the V1 file, so both can be set.
    v2: String,
}

/// Returns the OpenCode 2 plugin entrypoint next to the V1 plugin file:
/// `plugins/agentdesktop.js` -> `plugins/agentdesktop/index.js`. OpenCode 2
/// only loads configured local plugins from directories.
fn credential_plugin_v2_path(plugin_path: &Path) -> PathBuf {
    let stem = plugin_path
        .file_stem()
        .map(|stem| stem.to_owned())
        .unwrap_or_else(|| "agentdesktop".into());
    plugin_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(stem)
        .join("index.js")
}

fn managed_config(
    config: &OpenCodeConfig,
    gateway: Option<&LlmGatewayConfig>,
    plugin_urls: Option<&PluginUrls>,
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
    let npm = match config.api.unwrap_or_default() {
        OpenCodeApi::OpenaiResponses => "@ai-sdk/openai",
        OpenCodeApi::OpenaiCompletions => "@ai-sdk/openai-compatible",
    };
    let provider = json!({
        "npm": npm,
        "name": "Agentdesktop",
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
    deep_merge(&mut settings, generated);
    if let Some(plugin_urls) = plugin_urls {
        append_plugin(&mut settings, "plugin", &plugin_urls.v1);
        append_plugin(&mut settings, "plugins", &plugin_urls.v2);
    }
    Ok(settings)
}

fn append_plugin(settings: &mut Value, key: &str, plugin_url: &str) {
    let plugins = settings
        .as_object_mut()
        .expect("OpenCode settings serialize as an object")
        .entry(key)
        .or_insert_with(|| Value::Array(Vec::new()));
    if !plugins.is_array() {
        *plugins = Value::Array(Vec::new());
    }
    let plugins = plugins
        .as_array_mut()
        .expect("plugin list was replaced by an array");
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
        OpenCode::ID.to_owned(),
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
    throw new Error(`Agentdesktop credential helper failed: ${{stderr.trim() || `exit ${{exitCode}}`}}`);
  }}
  const token = stdout.trim();
  if (!token) throw new Error("Agentdesktop credential helper returned an empty token");
  cachedToken = token;
  refreshAfter = Date.now() + 60_000;
  return token;
}}

export const Agentdesktop = async () => ({{
  "chat.headers": async (input, output) => {{
    if (input.model.providerID !== provider) return;
    output.headers.Authorization = `Bearer ${{await credential()}}`;
  }},
}});
"#
    ))
}

/// OpenCode 2 credential plugin. Same token flow as [`credential_plugin`],
/// ported to the V2 plugin API (default export with `id` and `setup`). The
/// header is set on the native HTTP request: a `model.request` header is
/// replaced afterwards by the provider's own `Authorization` header built from
/// the placeholder API key.
fn credential_plugin_v2(credential_helper: &Path, socket: &Path) -> anyhow::Result<String> {
    let provider_name = "agentdesktop";
    let command = [
        credential_helper.to_string_lossy().into_owned(),
        "--socket".to_owned(),
        socket.to_string_lossy().into_owned(),
        "credential".to_owned(),
        "--client-id".to_owned(),
        OpenCode::ID.to_owned(),
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
    throw new Error(`Agentdesktop credential helper failed: ${{stderr.trim() || `exit ${{exitCode}}`}}`);
  }}
  const token = stdout.trim();
  if (!token) throw new Error("Agentdesktop credential helper returned an empty token");
  cachedToken = token;
  refreshAfter = Date.now() + 60_000;
  return token;
}}

export default {{
  id: "agentdesktop",
  async setup(ctx) {{
    await ctx.session.hook(
      "http.request",
      async (event) => {{
        event.request.headers.set("authorization", `Bearer ${{await credential()}}`);
      }},
      {{ providerID: provider }},
    );
  }},
}};
"#
    ))
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

fn reconcile_file(
    path: &Path,
    contents: &[u8],
    description: &str,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let existing = match plan.read(path) {
        Ok(existing) => Some(existing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read {description} from {}", path.display()));
        }
    };
    let action = match existing.as_deref() {
        // Identical bytes at a mode looser than planned: rewritten (update).
        Some(existing)
            if existing == contents
                && !crate::reconcile::grants_beyond(path, MANAGED_FILE_MODE) =>
        {
            debug!(
                program = CONFIG_PROGRAM,
                kind = description,
                action = "unchanged",
                path = %path.display(),
                "managed file already current"
            );
            plan.record(OpenCode::DISPLAY_NAME, description, "unchanged", path);
            return Ok(());
        }
        Some(existing) if existing.starts_with(MANAGED_HEADER.as_bytes()) => "update",
        Some(existing) => {
            plan.record_diff(
                OpenCode::DISPLAY_NAME,
                description,
                "conflict",
                path,
                Some(existing),
                Some(contents),
            );
            return Ok(());
        }
        None => "create",
    };
    plan.write_file(path, contents, MANAGED_FILE_MODE)?;
    debug!(
        program = CONFIG_PROGRAM,
        kind = description,
        action,
        path = %path.display(),
        "planned managed file"
    );
    plan.record_diff(
        OpenCode::DISPLAY_NAME,
        description,
        action,
        path,
        existing.as_deref(),
        Some(contents),
    );
    Ok(())
}

fn remove_owned(path: &Path, description: &str, plan: &ReconcilePlan) -> anyhow::Result<()> {
    match plan.read(path) {
        Ok(contents) if contents.starts_with(MANAGED_HEADER.as_bytes()) => {
            plan.remove_file(path)
                .with_context(|| format!("remove {description} at {}", path.display()))?;
            debug!(
                program = CONFIG_PROGRAM,
                kind = description,
                action = "remove",
                path = %path.display(),
                "planned managed file"
            );
            plan.record(OpenCode::DISPLAY_NAME, description, "remove", path);
            Ok(())
        }
        Ok(_) => {
            debug!(
                program = CONFIG_PROGRAM,
                kind = description,
                action = "unchanged",
                path = %path.display(),
                "preserving managed file not owned by Agentdesktop"
            );
            plan.record(OpenCode::DISPLAY_NAME, description, "unchanged", path);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            plan.record(OpenCode::DISPLAY_NAME, description, "unchanged", path);
            Ok(())
        }
        Err(error) => {
            Err(error).with_context(|| format!("read {description} from {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use agentdesktop_core::config::parse_daemon;

    use super::{
        PluginUrls, credential_plugin, credential_plugin_v2, credential_plugin_v2_path,
        managed_config,
    };

    #[test]
    fn pass_through_settings_are_merged_with_gateway_and_plugin() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com/proxy
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [opencode]
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
        .expect("valid daemon configuration");
        let open_code = config.programs.open_code.as_ref().unwrap();
        let gateway = config.llm_gateway.as_ref().unwrap();
        let settings = managed_config(
            open_code,
            Some(gateway),
            Some(&PluginUrls {
                v1: "file:///etc/opencode/plugins/agentdesktop.js".to_owned(),
                v2: "file:///etc/opencode/plugins/agentdesktop".to_owned(),
            }),
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
            settings["plugins"],
            serde_json::json!(["file:///etc/opencode/plugins/agentdesktop"])
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
    fn completions_api_selects_openai_compatible_provider() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [opencode]
programs:
  openCode:
    api: openai-completions
    model: qwen
    models:
      qwen: {}
    managedConfig:
      provider:
        agentdesktop:
          npm: "@ai-sdk/openai"
"#,
        )
        .expect("valid daemon configuration");
        let open_code = config.programs.open_code.as_ref().unwrap();
        let gateway = config.llm_gateway.as_ref().unwrap();
        let settings = managed_config(open_code, Some(gateway), None).expect("merged settings");

        let provider = &settings["provider"]["agentdesktop"];
        assert_eq!(provider["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(
            provider["options"]["baseURL"],
            "https://gateway.example.com/v1"
        );
        assert_eq!(settings["model"], "agentdesktop/qwen");
    }

    #[test]
    fn unknown_api_is_rejected() {
        let error = parse_daemon(
            r#"
programs:
  openCode:
    api: anthropic-messages
"#,
        )
        .expect_err("unknown dialect");
        assert!(format!("{error:#}").contains("anthropic-messages"));
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

    #[test]
    fn v2_plugin_is_a_directory_next_to_the_v1_plugin() {
        assert_eq!(
            credential_plugin_v2_path(Path::new("/etc/opencode/plugins/agentdesktop.js")),
            Path::new("/etc/opencode/plugins/agentdesktop/index.js")
        );
    }

    #[test]
    fn v2_plugin_exports_a_definition_and_sets_the_native_header() {
        let plugin = credential_plugin_v2(
            Path::new("/usr/local/bin/agentdesktop"),
            Path::new("/run/agentdesktop/agentdesktop.sock"),
        )
        .expect("credential plugin");

        assert!(plugin.starts_with(super::MANAGED_HEADER));
        assert!(plugin.contains(
            r#"const command = ["/usr/local/bin/agentdesktop","--socket","/run/agentdesktop/agentdesktop.sock","credential","--client-id","opencode"];"#
        ));
        assert!(plugin.contains("export default {"));
        assert!(plugin.contains(r#"id: "agentdesktop","#));
        assert!(plugin.contains(
            r#"ctx.session.hook(
      "http.request","#
        ));
        assert!(plugin.contains(r#"event.request.headers.set("authorization""#));
        assert!(plugin.contains("{ providerID: provider }"));
    }

    #[test]
    fn credential_helper_writes_both_plugins_and_cleanup_removes_them() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [opencode]
programs:
  openCode:
    model: qwen
    models:
      qwen: {}
"#,
        )
        .expect("valid daemon configuration");
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("opencode.jsonc");
        let plugin = dir.path().join("plugins/agentdesktop.js");
        let plugin_v2 = dir.path().join("plugins/agentdesktop/index.js");
        let open_code = config.programs.open_code.as_ref().unwrap();
        let gateway = config.llm_gateway.as_ref();

        let plan = crate::reconcile::ReconcilePlan::default();
        super::plan(
            &settings,
            &plugin,
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
            Some((open_code, gateway)),
            &plan,
        )
        .expect("plan");
        plan.apply().expect("apply");

        assert!(plugin.is_file());
        assert!(plugin_v2.is_file());
        let managed = std::fs::read_to_string(&settings).unwrap();
        let managed: serde_json::Value =
            serde_json::from_str(managed.trim_start_matches(super::MANAGED_HEADER)).unwrap();
        assert_eq!(
            managed["plugins"][0],
            super::file_url(plugin_v2.parent().unwrap()).unwrap()
        );

        let plan = crate::reconcile::ReconcilePlan::default();
        super::plan(
            &settings,
            &plugin,
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
            None,
            &plan,
        )
        .expect("cleanup plan");
        plan.apply().expect("cleanup apply");

        assert!(!settings.exists());
        assert!(!plugin.exists());
        assert!(!plugin_v2.exists());
    }

    #[cfg(unix)]
    fn mode_repair_fixture() -> (tempfile::TempDir, agentdesktop_core::config::DaemonConfig) {
        let config = parse_daemon(
            r#"
programs:
  openCode: {}
"#,
        )
        .expect("valid daemon configuration");
        (tempfile::tempdir().unwrap(), config)
    }

    #[test]
    #[cfg(unix)]
    fn identical_bytes_with_looser_mode_is_an_update_at_the_planned_mode() {
        let (dir, config) = mode_repair_fixture();
        let path = dir.path().join("opencode.jsonc");
        let plugin = dir.path().join("plugin.js");
        let plan_fn = |plan: &crate::reconcile::ReconcilePlan| {
            super::plan(
                &path,
                &plugin,
                Path::new("agentdesktop"),
                Path::new("agentdesktop.sock"),
                Some((config.programs.open_code.as_ref().unwrap(), None)),
                plan,
            )
        };
        crate::reconcile::plan::mode_repair::assert_identical_bytes_with_looser_mode_is_an_update(
            &[&path],
            0o644,
            &plan_fn,
        );
    }

    #[test]
    #[cfg(unix)]
    fn identical_bytes_within_mode_is_unchanged() {
        let (dir, config) = mode_repair_fixture();
        let path = dir.path().join("opencode.jsonc");
        let plugin = dir.path().join("plugin.js");
        let plan_fn = |plan: &crate::reconcile::ReconcilePlan| {
            super::plan(
                &path,
                &plugin,
                Path::new("agentdesktop"),
                Path::new("agentdesktop.sock"),
                Some((config.programs.open_code.as_ref().unwrap(), None)),
                plan,
            )
        };
        crate::reconcile::plan::mode_repair::assert_identical_bytes_within_mode_is_unchanged(
            &[&path],
            &plan_fn,
        );
    }
}
