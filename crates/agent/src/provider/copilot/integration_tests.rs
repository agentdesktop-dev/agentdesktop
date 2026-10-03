use std::time::Duration;

use anyhow::ensure;
use serde_json::{Value, json};
use tracing::info;

use crate::common::{Container, Gateway};

const CONFIG: &str = "/tmp/agentdesktop.yaml";
const PROVIDERS: &str = "/home/tester/.copilot/providers.json";
const SIDECAR: &str = "/home/tester/.copilot/.providers.json.agentdesktop";
const PAIRING: &str = "/home/tester/.local/state/agentdesktop/llm-proxy-pairing";
const SOCKET: &str = "/run/user/1000/agentdesktop.sock";
const LISTEN: &str = "127.0.0.1:18095";
const VERSION: &str = "1.0.88";

/// The daemon runs as the user (`--user`, started through `runuser` because
/// the program manages a file in the user's home, unlike the system-mode
/// scenarios of the other providers), writes the user's providers file
/// pointed at its loopback proxy, and a client using that file reaches the
/// gateway through the proxy. Removing the program takes the entries out again.
#[tokio::test]
async fn user_mode_providers_lifecycle() -> anyhow::Result<()> {
    Container::run(
        "copilot",
        "crates/agent/src/provider/copilot/testdata/Dockerfile",
        async |container| {
            let gateway = Gateway::start().await?;
            let user_entry = json!({
                "providers": [{"name": "my-own", "type": "openai", "baseUrl": "https://example.invalid/v1", "apiKey": "sk-user"}],
                "models": [{"provider": "my-own", "id": "gpt-mine"}],
            });
            container
                .write(PROVIDERS, &serde_json::to_string_pretty(&user_entry)?)
                .await?;
            container
                .exec(&["chown", "-R", "tester:tester", "/home/tester/.copilot"])
                .await?;
            container.exec(&["chmod", "600", PROVIDERS]).await?;
            let config = |with_program: bool| {
                let mut document = json!({
                    "daemon": { "user": true, "llmProxy": { "listen": LISTEN } },
                    "llmGateway": { "url": gateway.url },
                    "programs": {},
                });
                if with_program {
                    document["programs"]["copilot"] =
                        json!({ "models": { "gpt-4.1": { "wireModel": "gpt-4.1-mini" } } });
                }
                serde_json::to_string_pretty(&document).unwrap()
            };
            container.write(CONFIG, &config(true)).await?;
            container.exec(&["chown", "tester:tester", CONFIG]).await?;

            // The proxy cannot run in one-shot mode, so a dry run with the
            // listener configured is refused rather than previewed.
            info!("Checking that dry run refuses the listener");
            let refusal = "daemon.llmProxy.listen cannot be combined with --once or --dry-run";
            let refused = container
                .exec_as(
                    "tester",
                    &["agentdesktop", "daemon", "--config", CONFIG, "--dry-run"],
                    Duration::from_secs(15),
                )
                .await
                .map(|output| output.contains(refusal))
                .unwrap_or_else(|error| format!("{error:#}").contains(refusal));
            ensure!(refused, "dry run with llmProxy.listen must be refused with the documented message");
            container.exec(&["test", "!", "-e", SIDECAR]).await?;

            container
                .start_process(
                    "daemon",
                    &[
                        "runuser",
                        "-u",
                        "tester",
                        "--",
                        "agentdesktop",
                        "daemon",
                        "--config",
                        CONFIG,
                    ],
                )
                .await?;
            container
                .wait_ready(&["agentdesktop", "--socket", SOCKET, "status"])
                .await?;

            info!("Checking discovery and the written providers file");
            let discovered = container
                .exec(&["agentdesktop", "--socket", SOCKET, "discover"])
                .await?;
            ensure!(
                discovered.contains(&format!("copilot\t{VERSION}\t")),
                "Copilot CLI discovery failed: {discovered}"
            );
            let pairing = container.exec(&["cat", PAIRING]).await?;
            let pairing = pairing.trim();
            ensure!(pairing.len() >= 32, "pairing not created: {pairing:?}");
            let written: Value = serde_json::from_str(&container.exec(&["cat", PROVIDERS]).await?)?;
            let providers = written["providers"].as_array().cloned().unwrap_or_default();
            ensure!(
                providers.iter().any(|provider| provider["name"] == "my-own"),
                "user provider lost: {written}"
            );
            let ours = providers
                .iter()
                .find(|provider| provider["name"] == "agentdesktop")
                .cloned()
                .unwrap_or_default();
            ensure!(
                ours["baseUrl"] == format!("http://{LISTEN}/copilot-cli/v1"),
                "unexpected managed provider: {written}"
            );
            ensure!(
                ours["headers"]["x-agentdesktop-pairing"] == pairing,
                "pairing header does not match the daemon's pairing: {written}"
            );
            ensure!(ours.get("apiKey").is_none(), "no apiKey may be written");
            let mode = container.exec(&["stat", "-c", "%a", PROVIDERS]).await?;
            ensure!(mode.trim() == "600", "providers file mode {mode}");
            container.exec(&["test", "-e", SIDECAR]).await?;

            info!("Checking that a client using the file reaches the gateway through the proxy");
            let status = container
                .exec_as(
                    "tester",
                    &[
                        "curl",
                        "-s",
                        "-o",
                        "/dev/null",
                        "-w",
                        "%{http_code}",
                        "-H",
                        &format!("x-agentdesktop-pairing: {pairing}"),
                        "-H",
                        "content-type: application/json",
                        "-X",
                        "POST",
                        &format!("http://{LISTEN}/copilot-cli/v1/chat/completions"),
                        "-d",
                        "{\"model\":\"gpt-4.1-mini\",\"messages\":[]}",
                    ],
                    Duration::from_secs(15),
                )
                .await?;
            // The stub gateway wants its own token, which no gateway identity is
            // configured to supply here; the request still has to arrive.
            ensure!(
                gateway
                    .requests()
                    .iter()
                    .any(|request| request["path"] == "/v1/chat/completions"),
                "no request reached the gateway (client saw {status}): {:?}",
                gateway.requests()
            );

            info!("Removing the program");
            container.write(CONFIG, &config(false)).await?;
            container.stop_process("daemon").await?;
            container
                .start_process(
                    "daemon",
                    &[
                        "runuser",
                        "-u",
                        "tester",
                        "--",
                        "agentdesktop",
                        "daemon",
                        "--config",
                        CONFIG,
                    ],
                )
                .await?;
            container
                .wait_ready(&["agentdesktop", "--socket", SOCKET, "status"])
                .await?;
            let remaining: Value = serde_json::from_str(&container.exec(&["cat", PROVIDERS]).await?)?;
            let providers = remaining["providers"].as_array().cloned().unwrap_or_default();
            ensure!(
                providers.len() == 1 && providers[0]["name"] == "my-own",
                "managed entries not removed: {remaining}"
            );
            let mode = container.exec(&["stat", "-c", "%a", PROVIDERS]).await?;
            ensure!(
                mode.trim() == "600",
                "removal must keep the user's file owner-only, got {mode}"
            );
            container.exec(&["test", "!", "-e", SIDECAR]).await?;
            Ok(())
        },
    )
    .await
}
