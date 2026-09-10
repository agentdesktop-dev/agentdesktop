use std::time::{Duration, Instant};

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use tracing::info;

use crate::common::{Container, Gateway, TOKEN};

const CONFIG: &str = "/tmp/agentdesktop.yaml";
const SETTINGS: &str = "/etc/claude-code/managed-settings.d/50-agentdesktop.json";
const OWNER: &str = "/etc/claude-code/managed-settings.d/.50-agentdesktop.json.owner";
const USER_SETTINGS: &str = "/home/tester/.claude/settings.json";
const COMPANY_SETTINGS: &str = "/etc/claude-code/managed-settings.d/10-company.json";
const VERSION: &str = "2.1.237";

#[tokio::test]
async fn managed_gateway_lifecycle() -> anyhow::Result<()> {
    Container::run(
        "claude-code",
        "crates/agent/src/provider/claude_code/testdata/Dockerfile",
        async |container| {
            let gateway = Gateway::start().await?;
            let user_settings = "{\"env\":{\"PROVIDER_TEST_USER\":\"keep\"}}\n";
            let company_settings = "{\"env\":{\"PROVIDER_TEST_COMPANY\":\"keep\"}}\n";
            container.write(USER_SETTINGS, user_settings).await?;
            container.write(COMPANY_SETTINGS, company_settings).await?;
            container
                .write(
                    "/home/tester/.claude.json",
                    "{\"hasCompletedOnboarding\":true}\n",
                )
                .await?;
            container
                .exec(&[
                    "chown",
                    "-R",
                    "tester:tester",
                    "/home/tester/.claude",
                    "/home/tester/.claude.json",
                ])
                .await?;
            container
                .write(
                    CONFIG,
                    &serde_json::to_string_pretty(&json!({
                        "llmGateway": {
                            "url": gateway.url,
                        },
                        "programs": { "claudeCode": { "env": { "ANTHROPIC_API_KEY": TOKEN } } },
                    }))?,
                )
                .await?;

            // Dry run must include the settings without creating them or ownership markers.
            info!("Checking dry run");
            let preview = container
                .exec(&["agentdesktop", "daemon", "--config", CONFIG, "--dry-run"])
                .await?;
            ensure!(
                preview.contains("CREATE  Claude Code"),
                "unexpected preview: {preview}"
            );
            container.exec(&["test", "!", "-e", SETTINGS]).await?;
            container.exec(&["test", "!", "-e", OWNER]).await?;

            container
                .start_process("daemon", &["agentdesktop", "daemon", "--config", CONFIG])
                .await?;
            container.wait_ready(&["agentdesktop", "status"]).await?;

            let discovered = container.exec(&["agentdesktop", "discover"]).await?;
            info!("Checking installed Claude Code and managed settings");
            ensure!(
                discovered.contains(&format!("claude-code\t{VERSION}\t")),
                "Claude Code discovery failed: {discovered}"
            );
            let installed = container
                .exec_as("tester", &["claude", "--version"], Duration::from_secs(15))
                .await?;
            ensure!(
                installed.starts_with(VERSION),
                "unexpected installed version: {installed}"
            );

            let managed = container.read(SETTINGS).await?;
            let settings: Value = serde_json::from_str(&managed)?;
            ensure!(
                settings["env"]["ANTHROPIC_BASE_URL"] == gateway.url,
                "gateway URL missing from managed settings"
            );
            ensure!(
                settings["env"]["ANTHROPIC_API_KEY"] == TOKEN,
                "test API key missing from managed settings"
            );
            ensure!(
                container.read(OWNER).await? == "Agentdesktop\n",
                "ownership marker missing"
            );

            // No key or gateway overrides are passed to Claude. It must obtain both
            // from the production managed-settings location.
            info!("Running Claude Code through the managed gateway");
            let started = Instant::now();
            let output = container
                .exec_as(
                    "tester",
                    &[
                        "claude",
                        "--print",
                        "Return the provider integration response.",
                        "--model",
                        "claude-sonnet-4-6",
                        "--output-format",
                        "json",
                        "--tools",
                        "",
                        "--max-turns",
                        "1",
                        "--no-session-persistence",
                    ],
                    Duration::from_secs(90),
                )
                .await?;
            info!(elapsed = ?started.elapsed(), "Claude Code request finished");
            let response: Value =
                serde_json::from_str(&output).context("decode Claude Code result")?;
            ensure!(
                response["is_error"] == false && response["result"] == "provider-integration-ok",
                "unexpected Claude Code response: {response}"
            );
            let requests = gateway.requests();
            ensure!(
                requests.iter().any(|r| {
                    r["path"] == "/v1/messages"
                        && r["body"]["messages"]
                            .to_string()
                            .contains("Return the provider integration response.")
                        && (r["authorization"] == format!("Bearer {TOKEN}") || r["apiKey"] == TOKEN)
                }),
                "Claude Code did not send the prompt with the managed test API key"
            );

            // Restarting exercises normal reconciliation again, including its
            // ownership checks, rather than calling the provider directly.
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
            container.exec(&["test", "!", "-e", OWNER]).await?;
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
