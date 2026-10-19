use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::info;

use crate::common::{Container, Gateway, TOKEN};

const CONFIG: &str = "/tmp/agentdesktop.yaml";
const SETTINGS: &str = "/etc/codex/managed_config.toml";
const USER_SETTINGS: &str = "/home/tester/.codex/config.toml";
const COMPANY_SETTINGS: &str = "/etc/codex/config.toml";
const VERSION: &str = "0.153.4";
const PROMPT: &str = "Return the provider integration response.";

#[tokio::test]
async fn managed_chatgpt_subscription_lifecycle() -> anyhow::Result<()> {
    Container::run(
        "codex",
        "crates/agent/src/provider/codex/testdata/Dockerfile",
        async |container| {
            let gateway = Gateway::start().await?;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let proxy_address = listener.local_addr()?;
            drop(listener);
            let state_dir = "/home/tester/.local/state/codex-subscription";
            let socket = format!("{state_dir}/agentdesktop.sock");
            let issuer = "https://codex-fixture.example/";
            let client_id = "codex-fixture";
            let account = URL_SAFE_NO_PAD.encode(Sha256::digest(format!("{issuer}\0{client_id}")));
            let entry = Sha256::digest(format!("dev.agentdesktop.gateway-oidc\0{account}"));
            let entry = entry.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
            container.write(
                &format!("{state_dir}/secrets/{entry}"),
                &json!({
                    "accessToken": TOKEN,
                    "refreshToken": null,
                    "expiresAtUnixSeconds": 4_000_000_000u64,
                    "tokenEndpoint": "https://codex-fixture.example/token",
                }).to_string(),
            ).await?;
            let claims = URL_SAFE_NO_PAD.encode(json!({
                "email": "fixture@example.com",
                "exp": 4_000_000_000u64,
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "fixture-account",
                    "chatgpt_user_id": "fixture-user",
                    "chatgpt_plan_type": "plus",
                },
            }).to_string());
            container.write(
                "/home/tester/.codex/auth.json",
                &json!({
                    "auth_mode": "chatgpt",
                    "tokens": {
                        "id_token": format!("e30.{claims}.fixture"),
                        "access_token": "codex-subscription-fixture",
                        "refresh_token": "fixture-refresh-token",
                        "account_id": "fixture-account",
                    },
                    "last_refresh": "2090-01-01T00:00:00Z",
                }).to_string(),
            ).await?;
            container.exec(&["chown", "-R", "tester:tester", state_dir, "/home/tester/.codex"]).await?;
            container.exec(&["chmod", "700", state_dir, &format!("{state_dir}/secrets")]).await?;
            container.write(CONFIG, &json!({
                "daemon": {
                    "user": true,
                    "stateDir": state_dir,
                    "llmProxy": { "listen": proxy_address.to_string() },
                },
                "llmGateway": {
                    "url": gateway.url,
                    "proxyUrl": format!("{}backend-api/codex", gateway.url),
                    "authentication": { "type": "oidc", "issuer": issuer, "clientId": client_id },
                },
                "programs": { "codex": {
                    "auth": "subscription",
                    "managedConfig": {
                        "model": "gpt-5.4",
                        "cli_auth_credentials_store": "file",
                        "model_providers": { "agentdesktop": { "request_max_retries": 0, "stream_max_retries": 0 } },
                    },
                } },
            }).to_string()).await?;
            container.start_process("subscription-daemon", &[
                "runuser", "--preserve-environment", "-u", "tester", "--",
                "agentdesktop", "daemon", "--user", "--config", CONFIG,
            ]).await?;
            container.wait_ready(&["agentdesktop", "--socket", &socket, "status"]).await?;
            let managed = container.read(USER_SETTINGS).await?;
            let parsed: toml::Value = toml::from_str(&managed)?;
            let provider = &parsed["model_providers"]["agentdesktop"];
            ensure!(provider["requires_openai_auth"].as_bool() == Some(true), "ChatGPT authentication missing");
            ensure!(provider.get("auth").is_none(), "gateway credential command conflicts with ChatGPT auth");
            let output = container.exec_as("tester", &[
                "env", "HTTP_PROXY=http://127.0.0.1:9", "HTTPS_PROXY=http://127.0.0.1:9", "NO_PROXY=127.0.0.1,localhost",
                "codex", "exec", "--skip-git-repo-check", "--ephemeral", "--json", PROMPT,
            ], Duration::from_secs(90)).await?;
            ensure!(output.contains("provider-integration-ok"), "native Codex subscription request failed: {output}");
            ensure!(gateway.requests().iter().any(|request| {
                request["path"] == "/backend-api/codex/responses"
                    && request["authorization"] == format!("Bearer {TOKEN}")
                    && request["llmToken"] == "codex-subscription-fixture"
                    && request["chatgptAccount"] == "fixture-account"
                    && request["pairing"].is_null()
                    && request["body"]["input"].to_string().contains(PROMPT)
            }), "Codex subscription headers did not reach the mock gateway correctly");
            container.stop_process("subscription-daemon").await?;
            container.write(CONFIG, &json!({"daemon": {"user": true, "stateDir": state_dir}, "programs": {}}).to_string()).await?;
            container.exec_as("tester", &["agentdesktop", "daemon", "--user", "--config", CONFIG, "--once"], Duration::from_secs(15)).await?;
            container.exec(&["test", "!", "-e", USER_SETTINGS]).await?;
            container.exec(&["test", "-f", "/home/tester/.codex/auth.json"]).await?;
            Ok(())
        },
    ).await
}

#[tokio::test]
async fn managed_gateway_lifecycle() -> anyhow::Result<()> {
    Container::run(
        "codex",
        "crates/agent/src/provider/codex/testdata/Dockerfile",
        async |container| {
            let gateway = Gateway::start().await?;
            let user_settings = "# User settings\nmodel = \"user-model\"\n";
            let company_settings = "# Company settings\n";
            container.write(USER_SETTINGS, user_settings).await?;
            container.write(COMPANY_SETTINGS, company_settings).await?;
            container
                .exec(&["chown", "-R", "tester:tester", "/home/tester/.codex"])
                .await?;
            container
                .write(
                    CONFIG,
                    &serde_json::to_string_pretty(&json!({
                        "llmGateway": { "url": gateway.url },
                        "programs": { "codex": { "managedConfig": {
                            "model": "gpt-5.4",
                            "model_providers": { "agentdesktop": {
                                "experimental_bearer_token": TOKEN,
                                "request_max_retries": 0,
                                "stream_max_retries": 0,
                            } },
                        } } },
                    }))?,
                )
                .await?;

            info!("Checking dry run");
            let preview = container
                .exec(&["agentdesktop", "daemon", "--config", CONFIG, "--dry-run"])
                .await?;
            ensure!(
                preview.contains("CREATE  Codex"),
                "unexpected preview: {preview}"
            );
            container.exec(&["test", "!", "-e", SETTINGS]).await?;
            container
                .start_process("daemon", &["agentdesktop", "daemon", "--config", CONFIG])
                .await?;
            container.wait_ready(&["agentdesktop", "status"]).await?;

            info!("Checking installed Codex and managed configuration");
            let discovered = container.exec(&["agentdesktop", "discover"]).await?;
            ensure!(
                discovered.contains(&format!("codex\t{VERSION}\t")),
                "Codex discovery failed: {discovered}"
            );
            let installed = container
                .exec_as("tester", &["codex", "--version"], Duration::from_secs(15))
                .await?;
            ensure!(
                installed.trim() == format!("codex-cli {VERSION}"),
                "unexpected installed version: {installed}"
            );
            let managed = container.read(SETTINGS).await?;
            let settings: toml::Value = toml::from_str(&managed)?;
            ensure!(
                settings["model_provider"].as_str() == Some("agentdesktop"),
                "managed provider missing"
            );
            let provider = &settings["model_providers"]["agentdesktop"];
            ensure!(
                provider["base_url"].as_str() == Some(format!("{}v1", gateway.url).as_str()),
                "managed gateway URL missing"
            );
            ensure!(
                provider["wire_api"].as_str() == Some("responses"),
                "Responses API missing"
            );
            ensure!(
                provider["experimental_bearer_token"].as_str() == Some(TOKEN),
                "managed test API key missing"
            );

            // The real CLI must read the gateway, model, and API key from managed config.
            info!("Running Codex through the managed gateway");
            let started = Instant::now();
            let output = container
                .exec_as(
                    "tester",
                    &[
                        "codex",
                        "exec",
                        "--skip-git-repo-check",
                        "--ephemeral",
                        "--json",
                        PROMPT,
                    ],
                    Duration::from_secs(90),
                )
                .await?;
            info!(elapsed = ?started.elapsed(), "Codex request finished");
            let events = output
                .lines()
                .map(serde_json::from_str::<Value>)
                .collect::<Result<Vec<_>, _>>()
                .context("decode Codex events")?;
            ensure!(
                events.iter().any(|event| event["type"] == "item.completed"
                    && event["item"]["type"] == "agent_message"
                    && event["item"]["text"] == "provider-integration-ok"),
                "unexpected Codex output: {output}"
            );
            ensure!(
                events.iter().any(|event| event["type"] == "turn.completed"),
                "Codex turn did not complete: {output}"
            );
            ensure!(
                gateway.requests().iter().any(|request| {
                    request["path"] == "/v1/responses"
                        && request["body"]["model"] == "gpt-5.4"
                        && request["body"]["input"].to_string().contains(PROMPT)
                        && request["authorization"] == format!("Bearer {TOKEN}")
                }),
                "Codex did not send the prompt with managed configuration"
            );

            info!("Checking repeat reconciliation");
            container.stop_process("daemon").await?;
            container
                .start_process(
                    "daemon-repeat",
                    &["agentdesktop", "daemon", "--config", CONFIG],
                )
                .await?;
            container.wait_ready(&["agentdesktop", "status"]).await?;
            ensure!(
                container.read(SETTINGS).await? == managed,
                "repeat apply changed managed settings"
            );
            let preview = container
                .exec(&["agentdesktop", "daemon", "--config", CONFIG, "--dry-run"])
                .await?;
            ensure!(
                preview.contains("Summary: 0 changes"),
                "repeat apply was not stable: {preview}"
            );
            container.stop_process("daemon-repeat").await?;

            info!("Checking cleanup preserves user and company settings");
            container.write(CONFIG, "programs: {}\n").await?;
            container
                .exec(&["agentdesktop", "daemon", "--config", CONFIG, "--once"])
                .await?;
            container.exec(&["test", "!", "-e", SETTINGS]).await?;
            ensure!(
                container.read(USER_SETTINGS).await? == user_settings,
                "cleanup changed user settings"
            );
            ensure!(
                container.read(COMPANY_SETTINGS).await? == company_settings,
                "cleanup changed company settings"
            );
            Ok(())
        },
    )
    .await
}
