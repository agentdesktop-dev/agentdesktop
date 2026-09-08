use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use tracing::info;

use crate::common::{Container, Gateway, TOKEN};

const CONFIG: &str = "/tmp/agentdesktop.yaml";
const SETTINGS: &str = "/etc/opencode/opencode.jsonc";
const USER_SETTINGS: &str = "/home/tester/.config/opencode/opencode.json";
const COMPANY_SETTINGS: &str = "/etc/opencode/company.txt";
const VERSION: &str = "1.18.29";
const PROMPT: &str = "Return the provider integration response.";

#[tokio::test]
async fn managed_gateway_lifecycle() -> anyhow::Result<()> {
    Container::run(
        "opencode",
        "crates/agent/src/provider/opencode/testdata/Dockerfile",
        async |container| {
            let gateway = Gateway::start().await?;
            let user_settings = "{\"$schema\":\"https://opencode.ai/config.json\",\"model\":\"user/model\",\"autoupdate\":false}\n";
            let company_settings = "Company-owned file\n";
            container.write(USER_SETTINGS, user_settings).await?;
            container.write(COMPANY_SETTINGS, company_settings).await?;
            container
                .exec(&[
                    "chown",
                    "-R",
                    "tester:tester",
                    "/home/tester/.config/opencode",
                ])
                .await?;
            container
                .write(
                    CONFIG,
                    &serde_json::to_string_pretty(&json!({
                        "llmGateway": { "url": gateway.url },
                        "programs": { "openCode": {
                            "model": "gpt-5.4",
                            "models": { "gpt-5.4": {
                                "name": "Test model",
                                "limit": { "context": 128000, "output": 4096 },
                            } },
                            "managedConfig": { "autoupdate": false, "share": "disabled" },
                        } },
                    }))?,
                )
                .await?;

            info!("Checking dry run");
            let preview = container
                .exec(&["agentdesktop", "daemon", "--config", CONFIG, "--dry-run"])
                .await?;
            ensure!(
                preview.contains("CREATE  OpenCode"),
                "unexpected preview: {preview}"
            );
            container.exec(&["test", "!", "-e", SETTINGS]).await?;
            container
                .start_process("daemon", &["agentdesktop", "daemon", "--config", CONFIG])
                .await?;
            container.wait_ready(&["agentdesktop", "status"]).await?;

            info!("Checking installed OpenCode and managed configuration");
            let discovered = container.exec(&["agentdesktop", "discover"]).await?;
            ensure!(
                discovered.contains(&format!("opencode\t{VERSION}\t")),
                "OpenCode discovery failed: {discovered}"
            );
            let installed = container
                .exec_as(
                    "tester",
                    &["opencode", "--version"],
                    Duration::from_secs(15),
                )
                .await?;
            ensure!(
                installed.trim() == VERSION,
                "unexpected installed version: {installed}"
            );
            let managed = container.read(SETTINGS).await?;
            let settings: Value = json5::from_str(&managed)?;
            ensure!(
                settings["model"] == "agentdesktop/gpt-5.4",
                "managed model missing"
            );
            let provider = &settings["provider"]["agentdesktop"];
            ensure!(
                provider["options"]["baseURL"] == format!("{}v1", gateway.url),
                "managed gateway URL missing"
            );
            ensure!(
                provider["npm"] == "@ai-sdk/openai",
                "Responses provider missing"
            );
            ensure!(
                provider["options"]["apiKey"] == TOKEN,
                "managed placeholder key missing"
            );
            container
                .exec(&["test", "!", "-e", "/etc/opencode/plugins/agentdesktop.js"])
                .await?;

            // The real CLI must read the gateway, model, and API key from managed config.
            info!("Running OpenCode through the managed gateway");
            let started = Instant::now();
            let output = container
                .exec_as(
                    "tester",
                    &["opencode", "run", "--format", "json", PROMPT],
                    Duration::from_secs(90),
                )
                .await?;
            info!(elapsed = ?started.elapsed(), "OpenCode request finished");
            let events = output
                .lines()
                .map(serde_json::from_str::<Value>)
                .collect::<Result<Vec<_>, _>>()
                .context("decode OpenCode events")?;
            ensure!(
                events.iter().any(|event| event["type"] == "text"
                    && event["part"]["text"] == "provider-integration-ok"),
                "unexpected OpenCode output: {output}"
            );
            ensure!(
                events.iter().any(|event| event["type"] == "step_finish"),
                "OpenCode turn did not complete: {output}"
            );
            ensure!(
                gateway.requests().iter().any(|request| {
                    request["path"] == "/v1/responses"
                        && request["body"]["model"] == "gpt-5.4"
                        && request["body"]["input"].to_string().contains(PROMPT)
                        && request["authorization"] == format!("Bearer {TOKEN}")
                }),
                "OpenCode did not send the prompt with managed configuration"
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
