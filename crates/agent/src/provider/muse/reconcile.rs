use super::Muse;

use std::path::Path;

use agentdesktop_core::config::{LlmGatewayAuthentication, LlmGatewayConfig, MuseConfig};
use anyhow::Context;
use serde_json::{Value, json};

use crate::provider::{
    json_merge,
    shared::{deep_merge, responses_base_url},
};
use crate::reconcile::ReconcilePlan;

const DESCRIPTION: &str = "settings";
/// The only `settings.json` schema this integration writes. Muse Code rejects
/// a settings file without the member, so a file the merge creates carries it.
const SCHEMA_VERSION: u64 = 1;
const SCHEMA_VERSION_KEY: &str = "schema_version";
const ENDPOINT_TRANSPORT: &str = "endpoint_transport";
/// How often Muse Code runs `bearer_command` again. Without it Muse Code keeps
/// a credential until the gateway rejects it with 401 and only then fetches a
/// new one. Controller-issued credentials live five minutes and the daemon
/// may serve one from its cache for up to a minute, so two minutes stays
/// inside that lifetime.
const BEARER_REFRESH_SECS: u64 = 120;
/// `settings.json` may hold literal MCP headers and environment values: a
/// file we write is owner-only, and the dry-run report shows the action and
/// path only.
const SETTINGS_OPTIONS: json_merge::MergeOptions = json_merge::MergeOptions {
    mode: 0o600,
    keyed_arrays: &[],
    redact_diff: true,
    format: json_merge::JsonFormat::Json,
    replace_paths: &[],
    retain_on_removal: &[SCHEMA_VERSION_KEY],
};

pub(super) fn plan(
    settings_path: &Path,
    credential_helper: &Path,
    socket: &Path,
    config: Option<(&MuseConfig, Option<&LlmGatewayConfig>)>,
    plan: &ReconcilePlan,
) -> anyhow::Result<()> {
    let state = json_merge::state_path(settings_path);
    let Some((config, gateway)) = config else {
        json_merge::plan_remove_with(
            settings_path,
            &state,
            DESCRIPTION,
            Muse::DISPLAY_NAME,
            SETTINGS_OPTIONS,
            plan,
        )?;
        return Ok(());
    };
    // A newer settings schema may have moved the members written here: leave
    // the file alone and report the conflict.
    if has_unsupported_schema(settings_path, plan)? {
        plan.record(Muse::DISPLAY_NAME, DESCRIPTION, "conflict", settings_path);
        return Ok(());
    }
    json_merge::plan_merge_with(
        settings_path,
        &state,
        managed_settings(config, gateway, credential_helper, socket)?,
        false,
        DESCRIPTION,
        Muse::DISPLAY_NAME,
        json_merge::MergeOptions {
            // The gateway owns the whole transport: a leftover user proxy,
            // header or command must not ride along to the gateway.
            replace_paths: if gateway.is_some() {
                &["/endpoint_transport"]
            } else {
                &[]
            },
            ..SETTINGS_OPTIONS
        },
        plan,
    )
}

fn has_unsupported_schema(settings_path: &Path, plan: &ReconcilePlan) -> anyhow::Result<bool> {
    let contents = match plan.read(settings_path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "read {} from {}",
                    Muse::DISPLAY_NAME,
                    settings_path.display()
                )
            });
        }
    };
    // Anything else unreadable is the merge's own conflict to report.
    Ok(serde_json::from_slice::<Value>(&contents)
        .ok()
        .and_then(|settings| settings.get(SCHEMA_VERSION_KEY).cloned())
        .is_some_and(|version| version != json!(SCHEMA_VERSION)))
}

fn managed_settings(
    config: &MuseConfig,
    gateway: Option<&LlmGatewayConfig>,
    credential_helper: &Path,
    socket: &Path,
) -> anyhow::Result<Value> {
    let mut settings = serde_json::to_value(&config.managed_config)
        .context("serialize Muse Code pass-through managed configuration")?;
    let mut generated = json!({ (SCHEMA_VERSION_KEY): SCHEMA_VERSION });
    if let Some(model) = config.model.as_deref() {
        generated["model"] = json!(model);
    }
    if let Some(gateway) = gateway {
        let uses_credential_helper = gateway
            .authentication
            .as_ref()
            .is_some_and(LlmGatewayAuthentication::uses_credential_helper);
        // Muse Code runs `bearer_command` as a program with arguments, not
        // through a shell, so nothing is quoted. The previous command and its
        // refresh interval are dropped with it.
        let transport = settings
            .get_mut(ENDPOINT_TRANSPORT)
            .and_then(Value::as_object_mut);
        if let Some(transport) = transport {
            transport.remove("bearer_command");
            transport.remove("bearer_refresh_secs");
        }
        generated[ENDPOINT_TRANSPORT] = if uses_credential_helper {
            json!({
                "base_url": responses_base_url(gateway),
                "auth": "command",
                "bearer_command": [
                    credential_helper.to_string_lossy(),
                    "--socket",
                    socket.to_string_lossy(),
                    "credential",
                    "--client-id",
                    Muse::ID,
                ],
                "bearer_refresh_secs": BEARER_REFRESH_SECS,
            })
        } else {
            json!({
                "base_url": responses_base_url(gateway),
                "auth": "none",
            })
        };
    }
    deep_merge(&mut settings, generated);
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use agentdesktop_core::config::parse_daemon;
    use serde_json::{Value, json};

    use super::plan;
    use crate::reconcile::ReconcilePlan;

    const GATEWAY: &str = "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: controllerJwt\n    audience: agentgateway\n    allowedClientIds: [muse]\n";

    fn reconcile_fixture(settings: &Path, config: Option<&str>) -> ReconcilePlan {
        let config = config.map(|yaml| parse_daemon(yaml).unwrap());
        let changes = ReconcilePlan::default();
        plan(
            settings,
            Path::new("/opt/agentdesktop/agentdesktop"),
            Path::new("/run/agentdesktop.sock"),
            config.as_ref().map(|config| {
                let muse = config.programs.muse.as_ref().unwrap();
                (
                    muse,
                    config.llm_gateway.as_ref().filter(|_| muse.use_llm_gateway),
                )
            }),
            &changes,
        )
        .unwrap();
        changes
    }

    fn read(path: &Path) -> Value {
        serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn gateway_writes_the_credential_command_and_schema_version() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("settings.json");
        let config = format!("{GATEWAY}programs:\n  muse:\n    model: muse-spark\n");
        let changes = reconcile_fixture(&settings, Some(&config));
        assert!(!settings.exists(), "planning must not write files");
        changes.apply().unwrap();

        assert_eq!(
            read(&settings),
            json!({
                "schema_version": 1,
                "model": "muse-spark",
                "endpoint_transport": {
                    "base_url": "https://gateway.example.com/v1",
                    "auth": "command",
                    "bearer_command": [
                        "/opt/agentdesktop/agentdesktop",
                        "--socket",
                        "/run/agentdesktop.sock",
                        "credential",
                        "--client-id",
                        "muse",
                    ],
                    "bearer_refresh_secs": 120,
                },
            })
        );

        let repeated = reconcile_fixture(&settings, Some(&config));
        assert!(
            repeated.render().contains("Summary: 0 changes"),
            "{}",
            repeated.render()
        );
    }

    #[test]
    fn unauthenticated_gateway_sends_no_credential() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("settings.json");
        reconcile_fixture(
            &settings,
            Some("llmGateway:\n  url: https://gateway.example.com/base/\nprograms:\n  muse: {}\n"),
        )
        .apply()
        .unwrap();

        assert_eq!(
            read(&settings)["endpoint_transport"],
            json!({"base_url": "https://gateway.example.com/base/v1", "auth": "none"})
        );
    }

    #[test]
    fn gateway_replaces_the_user_transport_and_cleanup_restores_it() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("settings.json");
        let original = json!({
            "schema_version": 1,
            "provider": "meta",
            "model": "personal-model",
            "endpoint_transport": {
                "base_url": "https://personal.example.com/v1",
                "auth": "command",
                "bearer_command": ["personal-helper", "token"],
                "bearer_refresh_secs": 30,
                "proxy": "http://proxy.example.com:8080",
            },
            "mcpServers": {"docs": {"type": "stdio", "command": "npx"}},
        });
        fs::write(&settings, serde_json::to_vec_pretty(&original).unwrap()).unwrap();
        let config = format!(
            "{GATEWAY}programs:\n  muse:\n    managedConfig:\n      reasoning_effort: high\n      endpoint_transport:\n        ca_bundle: /etc/ssl/corp.pem\n        bearer_command: [ignored]\n        bearer_refresh_secs: 5\n"
        );
        reconcile_fixture(&settings, Some(&config)).apply().unwrap();

        let merged = read(&settings);
        assert_eq!(merged["model"], "personal-model");
        assert_eq!(merged["reasoning_effort"], "high");
        assert_eq!(merged["mcpServers"], original["mcpServers"]);
        assert_eq!(
            merged["endpoint_transport"],
            json!({
                "base_url": "https://gateway.example.com/v1",
                "auth": "command",
                "bearer_command": [
                    "/opt/agentdesktop/agentdesktop",
                    "--socket",
                    "/run/agentdesktop.sock",
                    "credential",
                    "--client-id",
                    "muse",
                ],
                "bearer_refresh_secs": 120,
                "ca_bundle": "/etc/ssl/corp.pem",
            })
        );

        reconcile_fixture(&settings, None).apply().unwrap();
        assert_eq!(read(&settings), original);
    }

    #[test]
    fn without_a_gateway_the_user_transport_is_kept() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("settings.json");
        let transport = json!({"base_url": "https://personal.example.com/v1", "auth": "none"});
        fs::write(
            &settings,
            serde_json::to_vec(&json!({"schema_version": 1, "endpoint_transport": transport}))
                .unwrap(),
        )
        .unwrap();
        let config =
            format!("{GATEWAY}programs:\n  muse:\n    useLlmGateway: false\n    model: selected\n");
        reconcile_fixture(&settings, Some(&config)).apply().unwrap();

        let merged = read(&settings);
        assert_eq!(merged["endpoint_transport"], transport);
        assert_eq!(merged["model"], "selected");
    }

    #[test]
    fn cleanup_removes_a_created_file_but_keeps_schema_version_with_user_content() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("settings.json");
        let config = format!("{GATEWAY}programs:\n  muse: {{}}\n");
        reconcile_fixture(&settings, Some(&config)).apply().unwrap();
        reconcile_fixture(&settings, None).apply().unwrap();
        assert!(!settings.exists());

        // Muse Code rejects a settings file without `schema_version`, so it
        // stays when the user added settings to the file the merge created.
        reconcile_fixture(&settings, Some(&config)).apply().unwrap();
        let mut edited = read(&settings);
        edited["reasoning_effort"] = json!("high");
        fs::write(&settings, serde_json::to_vec_pretty(&edited).unwrap()).unwrap();
        reconcile_fixture(&settings, None).apply().unwrap();
        assert_eq!(
            read(&settings),
            json!({"schema_version": 1, "reasoning_effort": "high"})
        );
    }

    #[test]
    fn newer_settings_schema_is_a_conflict() {
        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("settings.json");
        let original = r#"{"schema_version":2,"model":"personal"}"#;
        fs::write(&settings, original).unwrap();
        let config = format!("{GATEWAY}programs:\n  muse: {{}}\n");
        let changes = reconcile_fixture(&settings, Some(&config));
        assert!(changes.has_conflicts(), "{}", changes.render());
        assert_eq!(fs::read_to_string(&settings).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn settings_are_private_and_dry_run_omits_contents() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let settings = root.path().join("settings.json");
        fs::write(
            &settings,
            r#"{"schema_version":1,"mcpServers":{"docs":{"type":"streamable-http","url":"https://example.com/mcp","headers":{"Authorization":"Bearer test-secret"}}}}"#,
        )
        .unwrap();
        fs::set_permissions(&settings, fs::Permissions::from_mode(0o644)).unwrap();
        let config = format!("{GATEWAY}programs:\n  muse: {{}}\n");
        let changes = reconcile_fixture(&settings, Some(&config));
        let report = changes.render();
        assert!(report.contains("UPDATE  Muse Code settings"), "{report}");
        assert!(!report.contains("test-secret"), "{report}");
        changes.apply().unwrap();
        assert_eq!(
            fs::metadata(&settings).unwrap().permissions().mode() & 0o077,
            0
        );
        assert_eq!(
            read(&settings)["mcpServers"]["docs"]["headers"]["Authorization"],
            "Bearer test-secret"
        );
    }
}
