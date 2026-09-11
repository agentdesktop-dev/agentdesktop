use std::path::Path;

use agentdesktop_core::config::{
    CodexConfig, LlmGatewayAuthentication, LlmGatewayConfig, SandboxConfig,
};
use anyhow::Context;
use serde_json::{Value, json};

use super::{ReconcileMode, deep_merge, managed_file::HeaderOwnedFile, responses_base_url};

const MANAGED_HEADER: &str = "# Managed by Agentdesktop. Manual changes will be replaced.\n";
const MANAGED_FILE: HeaderOwnedFile = HeaderOwnedFile {
    program: "codex",
    header: MANAGED_HEADER,
};

pub fn apply(
    path: &Path,
    credential_helper: &Path,
    socket: &Path,
    sandbox: Option<&SandboxConfig>,
    config: Option<(&CodexConfig, Option<&LlmGatewayConfig>)>,
    mode: ReconcileMode,
) -> anyhow::Result<()> {
    let Some((config, gateway)) = config else {
        return MANAGED_FILE.reconcile(path, "configuration", None, mode);
    };

    let settings = managed_config(config, gateway, credential_helper, socket, sandbox)?;
    let mut body = toml::to_string_pretty(&settings)
        .context("serialize Codex managed configuration as TOML")?
        .into_bytes();
    if !body.is_empty() && !body.ends_with(b"\n") {
        body.push(b'\n');
    }

    MANAGED_FILE.reconcile(path, "configuration", Some(&body), mode)
}

fn managed_config(
    config: &CodexConfig,
    gateway: Option<&LlmGatewayConfig>,
    credential_helper: &Path,
    socket: &Path,
    sandbox: Option<&SandboxConfig>,
) -> anyhow::Result<Value> {
    let mut settings = serde_json::to_value(&config.managed_config)
        .context("serialize Codex pass-through managed configuration")?;
    if let Some(sandbox) = sandbox {
        let mut filesystem = serde_json::Map::from_iter([
            (":root".to_owned(), json!("read")),
            (":project_roots".to_owned(), json!("write")),
        ]);
        for path in &sandbox.filesystem.writable {
            filesystem.insert(path.to_string_lossy().into_owned(), json!("write"));
        }
        for path in &sandbox.filesystem.denied {
            filesystem.insert(path.to_string_lossy().into_owned(), json!("deny"));
        }

        let mut domains = serde_json::Map::new();
        for domain in &sandbox.network.allowed_domains {
            domains.insert(domain.clone(), json!("allow"));
        }
        let network_enabled = !domains.is_empty();
        let mut generated = json!({
            "default_permissions": "agentdesktop",
            "permissions": {
                "agentdesktop": {
                    "filesystem": filesystem,
                    "network": {
                        "enabled": network_enabled,
                        "mode": "limited",
                        "domains": domains,
                    },
                },
            },
        });
        if network_enabled {
            generated["features"] = json!({ "network_proxy": true });
        }
        deep_merge(&mut settings, generated);
    }
    let Some(gateway) = gateway else {
        return Ok(settings);
    };

    let provider_name = "agentdesktop";
    let mut provider = json!({
        "name": "Agentdesktop",
        "base_url": responses_base_url(gateway),
        "wire_api": "responses",
    });
    if gateway
        .authentication
        .as_ref()
        .is_some_and(LlmGatewayAuthentication::uses_credential_helper)
    {
        let timeout_ms = if matches!(
            gateway.authentication,
            Some(LlmGatewayAuthentication::Oidc { .. })
        ) {
            600_000
        } else {
            5_000
        };
        provider["auth"] = json!({
            "command": credential_helper.to_string_lossy(),
            "args": [
                "--socket",
                socket.to_string_lossy(),
                "credential",
                "--client-id",
                "codex",
            ],
            "timeout_ms": timeout_ms,
            "refresh_interval_ms": 60000,
        });
    }
    let generated = json!({
        "model_provider": provider_name,
        "model_providers": {
            (provider_name): provider,
        },
    });
    deep_merge(&mut settings, generated);
    Ok(settings)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use agentdesktop_core::config::parse_daemon;

    use super::{MANAGED_HEADER, apply, managed_config};
    use crate::reconcile::{DryRunReport, ReconcileMode};

    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("codex-{}", rand::random::<u64>()));
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
    fn apply_previews_rendered_bytes_and_only_removes_owned_configuration() {
        let root = Scratch::new();
        let path = root.0.join("managed.toml");
        let helper = root.0.join("credential helper");
        let socket = root.0.join("agent.sock");
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com/proxy
  authentication: { type: controllerJwt, audience: agentgateway, allowedClientIds: [codex] }
programs: { codex: { managedConfig: { model: company-model } } }
"#,
        )
        .unwrap();
        let codex = config.programs.codex.as_ref().unwrap();
        let gateway = config.llm_gateway.as_ref();
        let settings = managed_config(codex, gateway, &helper, &socket, None).unwrap();
        let toml = toml::to_string_pretty(&settings).unwrap();
        let rendered = format!("{MANAGED_HEADER}{toml}");
        let rendered = rendered.as_str();
        assert!(rendered.ends_with('\n'));
        let run = |enabled: bool, mode: ReconcileMode<'_>| {
            let desired = enabled.then_some((codex, gateway));
            apply(&path, &helper, &socket, None, desired, mode)
        };
        for (seed, enabled, action, after) in [
            (None, true, "create", Some(rendered)),
            (Some(rendered), true, "unchanged", Some(rendered)),
            (Some(MANAGED_HEADER), true, "update", Some(rendered)),
            (Some(rendered), false, "remove", None),
            (
                Some("user-owned\n"),
                false,
                "unchanged",
                Some("user-owned\n"),
            ),
        ] {
            if let Some(seed) = seed {
                fs::write(&path, seed).unwrap();
            }
            let report = DryRunReport::default();
            run(enabled, ReconcileMode::DryRun(&report)).unwrap();
            assert_eq!(fs::read(&path).ok().as_deref(), seed.map(str::as_bytes));
            let changes = report.changes.borrow();
            assert_eq!(changes.len(), 1);
            let change = &changes[0];
            assert_eq!((&change.path, change.action.as_str()), (&path, action));
            assert_eq!(
                change.before.as_deref(),
                seed.filter(|_| action != "unchanged")
            );
            assert_eq!(
                change.after.as_deref(),
                after.filter(|_| action != "unchanged")
            );
            run(enabled, ReconcileMode::Apply).unwrap();
            assert_eq!(fs::read(&path).ok().as_deref(), after.map(str::as_bytes));
        }
    }

    #[test]
    fn empty_configuration_renders_only_the_header_without_an_extra_newline() {
        let root = Scratch::new();
        let path = root.0.join("managed.toml");
        let helper = root.0.join("credential helper");
        let socket = root.0.join("agent.sock");
        let config = parse_daemon("programs: { codex: {} }").unwrap();
        let codex = config.programs.codex.as_ref().unwrap();
        let run = |mode: ReconcileMode<'_>| {
            apply(&path, &helper, &socket, None, Some((codex, None)), mode)
        };
        let report = DryRunReport::default();
        run(ReconcileMode::DryRun(&report)).unwrap();
        assert!(!path.exists());
        assert_eq!(
            report.changes.borrow()[0].after.as_deref(),
            Some(MANAGED_HEADER)
        );
        run(ReconcileMode::Apply).unwrap();
        assert_eq!(fs::read(&path).unwrap(), MANAGED_HEADER.as_bytes());
        let report = DryRunReport::default();
        run(ReconcileMode::DryRun(&report)).unwrap();
        assert_eq!(report.changes.borrow()[0].action, "unchanged");
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
    allowedClientIds: [codex]
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
        .expect("valid daemon configuration");
        let codex = config.programs.codex.as_ref().unwrap();
        let gateway = config.llm_gateway.as_ref().unwrap();

        let settings = managed_config(
            codex,
            Some(gateway),
            Path::new("/usr/local/bin/agentdesktop"),
            Path::new("/run/agentdesktop/agentdesktop.sock"),
            None,
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
        assert_eq!(provider["auth"]["args"].as_array().unwrap().len(), 5);
        assert_eq!(provider["auth"]["args"][3], "--client-id");
        assert_eq!(provider["auth"]["args"][4], "codex");
        assert_eq!(provider["auth"]["refresh_interval_ms"], 60000);

        let serialized = toml::to_string_pretty(&settings).expect("valid TOML");
        let parsed: toml::Value = toml::from_str(&serialized).expect("parse generated TOML");
        assert_eq!(parsed["model_provider"].as_str(), Some("agentdesktop"));
    }

    #[test]
    fn sandbox_configures_a_required_permission_profile() {
        let config = parse_daemon(
            r#"
sandbox:
  network:
    allowedDomains: [github.com]
  filesystem:
    writable: [/var/cache/company]
    denied: [/home/example/.ssh]
programs:
  codex: {}
"#,
        )
        .expect("valid daemon configuration");

        let settings = managed_config(
            config.programs.codex.as_ref().unwrap(),
            None,
            Path::new("agentdesktop"),
            Path::new("agentdesktop.sock"),
            config.sandbox.as_ref(),
        )
        .expect("sandbox settings");

        assert_eq!(settings["default_permissions"], "agentdesktop");
        assert_eq!(settings["features"]["network_proxy"], true);
        assert_eq!(
            settings["permissions"]["agentdesktop"]["filesystem"][":root"],
            "read"
        );
        assert!(settings["permissions"]["agentdesktop"]["filesystem"][":project_roots"] == "write");
        assert_eq!(
            settings["permissions"]["agentdesktop"]["filesystem"]["/var/cache/company"],
            "write"
        );
        assert_eq!(
            settings["permissions"]["agentdesktop"]["filesystem"]["/home/example/.ssh"],
            "deny"
        );
        assert_eq!(
            settings["permissions"]["agentdesktop"]["network"]["enabled"],
            true
        );
        assert_eq!(
            settings["permissions"]["agentdesktop"]["network"]["domains"]["github.com"],
            "allow"
        );
    }
}
