use std::path::Path;

use agentdesktop_core::config::{LlmGatewayAuthentication, LlmGatewayConfig, OpenCodeConfig};
use anyhow::Context;
use serde_json::{Value, json};
use url::Url;

use super::{ReconcileMode, deep_merge, managed_file::HeaderOwnedFile, responses_base_url};

const MANAGED_HEADER: &str = "// Managed by Agentdesktop. Manual changes will be replaced.\n";
const MANAGED_FILE: HeaderOwnedFile = HeaderOwnedFile {
    program: "opencode",
    header: MANAGED_HEADER,
};

pub fn apply(
    config_path: &Path,
    plugin_path: &Path,
    credential_helper: &Path,
    socket: &Path,
    config: Option<(&OpenCodeConfig, Option<&LlmGatewayConfig>)>,
    mode: ReconcileMode,
) -> anyhow::Result<()> {
    let Some((config, gateway)) = config else {
        MANAGED_FILE.reconcile(config_path, "managed configuration", None, mode)?;
        return MANAGED_FILE.reconcile(plugin_path, "credential plugin", None, mode);
    };

    let authentication = gateway.and_then(|gateway| gateway.authentication.as_ref());
    let plugin = if authentication.is_some_and(LlmGatewayAuthentication::uses_credential_helper) {
        Some((
            credential_plugin_body(credential_helper, socket)?,
            file_url(plugin_path)?,
        ))
    } else {
        None
    };
    let plugin_url = plugin.as_ref().map(|(_, url)| url.as_str());
    let settings = managed_config(config, gateway, plugin_url)?;
    let mut body = serde_json::to_string_pretty(&settings)
        .context("serialize OpenCode managed configuration")?
        .into_bytes();
    body.push(b'\n');

    // Activate the plugin before referencing it; remove its reference before deleting it.
    if let Some((source, _)) = &plugin {
        MANAGED_FILE.reconcile(
            plugin_path,
            "credential plugin",
            Some(source.as_bytes()),
            mode,
        )?;
    }
    MANAGED_FILE.reconcile(config_path, "managed configuration", Some(&body), mode)?;
    if plugin.is_none() {
        MANAGED_FILE.reconcile(plugin_path, "credential plugin", None, mode)?;
    }
    Ok(())
}

fn managed_config(
    config: &OpenCodeConfig,
    gateway: Option<&LlmGatewayConfig>,
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

fn credential_plugin_body(credential_helper: &Path, socket: &Path) -> anyhow::Result<String> {
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
        r#"const provider = {provider};
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

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use agentdesktop_core::config::parse_daemon;

    use super::{MANAGED_HEADER, apply, credential_plugin_body, file_url, managed_config};
    use crate::reconcile::{DryRunReport, ReconcileMode};

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("opencode-{}", rand::random::<u64>()));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn apply_renders_both_files_and_observes_lifecycle_and_conflict_ordering() {
        let root = Scratch::new();
        let path = root.0.join("managed.jsonc");
        let plugin = root.0.join("credential plugin.js");
        let helper = root.0.join("credential helper");
        let socket = root.0.join("agent.sock");
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com/proxy
  authentication: { type: controllerJwt, audience: agentgateway, allowedClientIds: [opencode] }
programs:
  openCode:
    model: company-model
    models: { company-model: { name: Company GPT } }
    managedConfig: { plugin: [existing-plugin], autoupdate: false }
"#,
        )
        .unwrap();
        let open_code = config.programs.open_code.as_ref().unwrap();
        let gateway = config.llm_gateway.as_ref().unwrap();
        let mut no_auth = gateway.clone();
        no_auth.authentication = None;
        let source = format!(
            "{MANAGED_HEADER}{}",
            credential_plugin_body(&helper, &socket).unwrap()
        );
        let url = file_url(&plugin).unwrap();
        let files = [&path, &plugin];
        let read = || files.map(|path| fs::read_to_string(path).ok());
        let run = |enabled: bool, gateway, mode: ReconcileMode<'_>| {
            let desired = enabled.then_some((open_code, gateway));
            apply(&path, &plugin, &helper, &socket, desired, mode)
        };
        for (gateway, enabled, actions) in [
            (Some(gateway), true, ["create", "create"]),
            (Some(gateway), true, ["unchanged", "unchanged"]),
            (Some(&no_auth), true, ["update", "remove"]),
            (Some(gateway), true, ["create", "update"]),
            (None, true, ["update", "remove"]),
            (None, true, ["unchanged", "unchanged"]),
            (Some(gateway), true, ["create", "update"]),
            (Some(gateway), false, ["remove", "remove"]),
        ] {
            let authenticated = gateway
                .and_then(|gateway| gateway.authentication.as_ref())
                .is_some();
            let plugin_url = authenticated.then_some(url.as_str());
            let settings = managed_config(open_code, gateway, plugin_url).unwrap();
            let json = serde_json::to_string_pretty(&settings).unwrap();
            let rendered = format!("{MANAGED_HEADER}{json}\n");
            let paths = if enabled && authenticated {
                [&plugin, &path]
            } else {
                files
            };
            let before = read();
            let after = [
                enabled.then_some(rendered),
                (enabled && authenticated).then_some(source.clone()),
            ];
            let report = DryRunReport::default();
            run(enabled, gateway, ReconcileMode::DryRun(&report)).unwrap();
            assert_eq!(read(), before);
            let changes = report.changes.borrow();
            assert_eq!(changes.len(), 2);
            for (change, (file_path, action)) in changes.iter().zip(paths.into_iter().zip(actions))
            {
                assert_eq!((&change.path, change.action.as_str()), (file_path, action));
                let index = files.iter().position(|path| *path == file_path).unwrap();
                assert_eq!(
                    change.before.as_deref(),
                    before[index].as_deref().filter(|_| action != "unchanged")
                );
                assert_eq!(
                    change.after.as_deref(),
                    after[index].as_deref().filter(|_| action != "unchanged")
                );
            }
            run(enabled, gateway, ReconcileMode::Apply).unwrap();
            assert_eq!(read(), after);
        }
        for (config_seed, plugin_seed) in [
            ("user config\n", MANAGED_HEADER),
            (MANAGED_HEADER, "user plugin\n"),
            ("user config\n", "user plugin\n"),
        ] {
            fs::write(&path, config_seed).unwrap();
            fs::write(&plugin, plugin_seed).unwrap();
            let before = read();
            let report = DryRunReport::default();
            run(true, Some(gateway), ReconcileMode::DryRun(&report)).unwrap();
            let changes = report.changes.borrow();
            assert_eq!(changes.len(), 2);
            let expected = [(&plugin, plugin_seed), (&path, config_seed)];
            for (change, (path, seed)) in changes.iter().zip(expected) {
                assert_eq!(&change.path, path);
                let action = if seed == MANAGED_HEADER {
                    "update"
                } else {
                    "conflict"
                };
                assert_eq!(change.action, action);
            }
            assert_eq!(read(), before);
            let error = run(true, Some(gateway), ReconcileMode::Apply).unwrap_err();
            assert!(error.to_string().contains("not owned by Agentdesktop"));
            let updated = if plugin_seed == MANAGED_HEADER {
                &source
            } else {
                plugin_seed
            };
            assert_eq!(
                read(),
                [Some(config_seed.to_owned()), Some(updated.to_owned())]
            );
        }
        // A directory is a deterministic read error: activation stops; removal reaches it second.
        fs::remove_file(&plugin).unwrap();
        fs::create_dir(&plugin).unwrap();
        fs::write(&path, MANAGED_HEADER).unwrap();
        for enabled in [true, false] {
            let report = DryRunReport::default();
            for mode in [ReconcileMode::DryRun(&report), ReconcileMode::Apply] {
                let error = run(enabled, Some(gateway), mode).unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("read OpenCode credential plugin")
                );
                assert!(plugin.is_dir());
                let expected = (enabled || mode.is_dry_run()).then_some(MANAGED_HEADER);
                assert_eq!(fs::read_to_string(&path).ok().as_deref(), expected);
            }
            let changes = report.changes.borrow();
            assert_eq!(changes.len(), usize::from(!enabled));
            if !enabled {
                assert_eq!(
                    (&changes[0].path, changes[0].action.as_str()),
                    (&path, "remove")
                );
            }
        }
    }

    #[test]
    fn disabling_authentication_preserves_plugin_referenced_by_foreign_config() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com/proxy
  authentication: { type: controllerJwt, audience: agentgateway, allowedClientIds: [opencode] }
programs:
  openCode:
    model: company-model
    models: { company-model: { name: Company GPT } }
    managedConfig: { plugin: [existing-plugin] }
"#,
        )
        .unwrap();
        let open_code = config.programs.open_code.as_ref().unwrap();
        let gateway = config.llm_gateway.as_ref().unwrap();
        let mut no_auth = gateway.clone();
        no_auth.authentication = None;
        for desired_gateway in [Some(&no_auth), None] {
            let root = Scratch::new();
            let path = root.0.join("managed.jsonc");
            let plugin = root.0.join("credential plugin.js");
            let helper = root.0.join("credential helper");
            let socket = root.0.join("agent.sock");
            let run = |gateway, mode: ReconcileMode<'_>| {
                apply(
                    &path,
                    &plugin,
                    &helper,
                    &socket,
                    Some((open_code, gateway)),
                    mode,
                )
            };
            run(Some(gateway), ReconcileMode::Apply).unwrap();
            let owned = fs::read_to_string(&path).unwrap();
            let foreign = owned.strip_prefix(MANAGED_HEADER).unwrap();
            let url = file_url(&plugin).unwrap();
            let settings: serde_json::Value = serde_json::from_str(foreign).unwrap();
            assert!(
                settings["plugin"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|value| value == &url)
            );
            fs::write(&path, foreign).unwrap();
            let files = [&path, &plugin];
            let read = || files.map(|path| fs::read_to_string(path).ok());
            let before = read();
            assert!(before.iter().all(Option::is_some));
            let report = DryRunReport::default();
            run(desired_gateway, ReconcileMode::DryRun(&report)).unwrap();
            assert_eq!(read(), before);
            let error = run(desired_gateway, ReconcileMode::Apply).unwrap_err();
            assert!(error.to_string().contains("not owned by Agentdesktop"));
            assert_eq!(read(), before);

            let changes = report.changes.borrow();
            assert_eq!(changes.len(), 2);
            assert_eq!(
                (&changes[0].path, changes[0].action.as_str()),
                (&path, "conflict")
            );
            assert_eq!(
                (&changes[1].path, changes[1].action.as_str()),
                (&plugin, "remove")
            );
            assert!(!changes[0].after.as_deref().unwrap().contains(&url));
            assert_eq!(changes[0].before.as_deref(), Some(foreign));
            assert_eq!(changes[1].before, before[1]);
            assert!(changes[1].after.is_none());
        }
    }

    #[test]
    fn invalid_gateway_configuration_fails_before_mutating_files_or_recording_a_plan() {
        let root = Scratch::new();
        let path = root.0.join("managed.jsonc");
        let plugin = root.0.join("credential plugin.js");
        let helper = root.0.join("credential helper");
        let socket = root.0.join("agent.sock");
        let mut config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com/proxy
  authentication: { type: controllerJwt, audience: agentgateway, allowedClientIds: [opencode] }
programs:
  openCode:
    model: company-model
    models: { company-model: { name: Company GPT } }
"#,
        )
        .unwrap();
        let open_code = config.programs.open_code.as_mut().unwrap();
        open_code.model = None;
        let run = |mode: ReconcileMode<'_>| {
            apply(
                &path,
                &plugin,
                &helper,
                &socket,
                Some((open_code, config.llm_gateway.as_ref())),
                mode,
            )
        };
        for seed in [None, Some(MANAGED_HEADER)] {
            if let Some(seed) = seed {
                fs::write(&path, seed).unwrap();
                fs::write(&plugin, seed).unwrap();
            }
            let report = DryRunReport::default();
            for mode in [ReconcileMode::DryRun(&report), ReconcileMode::Apply] {
                let error = run(mode).unwrap_err();
                assert_eq!(
                    error.to_string(),
                    "OpenCode gateway configuration has no model"
                );
                for file in [&path, &plugin] {
                    assert_eq!(fs::read_to_string(file).ok().as_deref(), seed);
                }
                assert_eq!(
                    fs::read_dir(&root.0).unwrap().count(),
                    if seed.is_some() { 2 } else { 0 }
                );
            }
            assert!(report.changes.borrow().is_empty());
        }
    }

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
        let plugin = credential_plugin_body(
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
