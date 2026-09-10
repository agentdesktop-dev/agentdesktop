use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use tracing::info;

use crate::common::{Container, Gateway, TOKEN};

const CONFIG: &str = "/tmp/agentdesktop.yaml";
const SETTINGS: &str = "/etc/codex/managed_config.toml";
const USER_SETTINGS: &str = "/home/tester/.codex/config.toml";
const COMPANY_SETTINGS: &str = "/etc/codex/config.toml";
const VERSION: &str = "0.153.4";
const PROMPT: &str = "Return the provider integration response.";

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
