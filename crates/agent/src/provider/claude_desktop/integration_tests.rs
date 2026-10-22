use std::time::Duration;

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tracing::info;

use crate::common::{Container, Gateway};

const CONFIG: &str = "/tmp/agentdesktop.yaml";
const SETTINGS: &str = "/etc/claude-desktop/managed-settings.json";
const OWNER: &str = "/etc/claude-desktop/.managed-settings.json.owner";
const SOCKET: &str = "/run/agentdesktop/agentdesktop.sock";
const CA: &str = "/var/lib/agentdesktop/intercept-ca.pem";

/// Claude Desktop has no Linux release, so curl stands in for it: it uses the
/// proxy that the daemon pins in the managed settings and trusts only the
/// daemon's intercept CA.
#[tokio::test]
async fn intercept_lifecycle() -> anyhow::Result<()> {
    Container::run(
        "claude-desktop",
        "crates/agent/src/provider/claude_desktop/testdata/Dockerfile",
        async |container| {
            let gateway = Gateway::start().await?;
            let gateway_address = gateway
                .url
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_owned();
            // The container shares the host network, so a port that is free
            // here is free for the daemon too.
            let listen = format!("127.0.0.1:{}", free_port()?);
            let proxy = format!("http://{listen}");
            container
                .write(
                    CONFIG,
                    &config(&listen, json!({ "url": gateway.url }), true)?,
                )
                .await?;

            info!("Checking dry run");
            let preview = container
                .exec(&["agentdesktop", "daemon", "--config", CONFIG, "--dry-run"])
                .await?;
            ensure!(
                preview.contains("CREATE  Claude Desktop"),
                "unexpected preview: {preview}"
            );
            container.exec(&["test", "!", "-e", SETTINGS]).await?;
            container.exec(&["test", "!", "-e", CA]).await?;

            container
                .start_process("daemon", &["agentdesktop", "daemon", "--config", CONFIG])
                .await?;
            container.wait_ready(&["agentdesktop", "status"]).await?;

            info!("Checking the proxy pin and the listener");
            let managed = container.read(SETTINGS).await?;
            let settings: Value = serde_json::from_str(&managed)?;
            ensure!(
                settings == json!({ "egressProxyUrl": proxy }),
                "managed settings must pin the proxy and set nothing else: {settings}"
            );
            ensure!(
                container.read(OWNER).await? == "Agentdesktop\n",
                "ownership marker missing"
            );
            let status = wait_bound(container).await?;
            ensure!(
                status["enabled"] == true && status["listen"] == listen.as_str(),
                "unexpected intercept status: {status}"
            );
            ensure!(
                status["caCertificate"] == CA,
                "unexpected CA path: {status}"
            );
            let fingerprint = status["caSha256"]
                .as_str()
                .context("daemon-info has no CA fingerprint")?
                .to_owned();

            info!("Sending intercepted and tunnelled requests through the listener");
            let body = through_listener(
                container,
                &proxy,
                &[
                    "--fail",
                    "-H",
                    "Cookie: sessionKey=integration",
                    "-H",
                    "x-agentdesktop-identity: Bearer spoofed",
                    "https://claude.ai/health",
                ],
            )
            .await?;
            ensure!(
                serde_json::from_str::<Value>(&body)?["ready"] == true,
                "unexpected claude.ai response: {body}"
            );
            through_listener(
                container,
                &proxy,
                &["--fail", "--http1.1", "https://api.anthropic.com/health"],
            )
            .await?;
            through_listener(
                container,
                &proxy,
                &[
                    "--fail",
                    "--proxytunnel",
                    &format!("http://{gateway_address}/health"),
                ],
            )
            .await?;
            let requests = gateway.requests();
            ensure!(
                requests.iter().any(|r| {
                    r["path"] == "/health"
                        && r["host"] == "claude.ai"
                        && r["cookie"] == "sessionKey=integration"
                        && r["identity"].is_null()
                }),
                "the gateway did not get the claude.ai request with its cookie and without the client's identity header: {requests:?}"
            );
            ensure!(
                requests
                    .iter()
                    .any(|r| r["path"] == "/health" && r["host"] == "api.anthropic.com"),
                "the gateway did not get the api.anthropic.com request: {requests:?}"
            );
            ensure!(
                requests
                    .iter()
                    .any(|r| r["path"] == "/health" && r["host"] == gateway_address.as_str()),
                "the tunnelled request did not arrive: {requests:?}"
            );
            let status = intercept_status(container).await?;
            ensure!(
                status["forwarded"].as_u64() >= Some(2)
                    && status["tunnels"].as_u64() >= Some(1)
                    && status["gatewayFailures"] == 0,
                "unexpected intercept counters: {status}"
            );

            info!("Checking repeat reconciliation");
            let preview = container
                .exec(&["agentdesktop", "daemon", "--config", CONFIG, "--dry-run"])
                .await?;
            ensure!(
                preview.contains("Summary: 0 changes"),
                "repeat apply was not stable: {preview}"
            );

            // interceptUrl takes precedence over url. Pointing it at a closed
            // port shows that requests fail at the listener and go nowhere else.
            info!("Checking that requests fail when the gateway is down");
            container.stop_process("daemon").await?;
            let closed = format!("http://127.0.0.1:{}/", free_port()?);
            container
                .write(
                    CONFIG,
                    &config(
                        &listen,
                        json!({ "url": gateway.url, "interceptUrl": closed }),
                        true,
                    )?,
                )
                .await?;
            container
                .start_process(
                    "daemon-gateway-down",
                    &["agentdesktop", "daemon", "--config", CONFIG],
                )
                .await?;
            container.wait_ready(&["agentdesktop", "status"]).await?;
            let status = wait_bound(container).await?;
            ensure!(
                status["caSha256"] == fingerprint.as_str(),
                "the daemon did not reuse its stored CA: {status}"
            );
            let before = gateway.requests().len();
            let code = through_listener(
                container,
                &proxy,
                &["-o", "/dev/null", "-w", "%{http_code}", "https://claude.ai/health"],
            )
            .await?;
            ensure!(code == "502", "expected 502 from the listener, got {code}");
            let status = intercept_status(container).await?;
            ensure!(
                status["gatewayFailures"].as_u64() >= Some(1)
                    && status["lastFailure"]
                        .as_str()
                        .is_some_and(|failure| failure.contains("Connect")),
                "daemon-info does not show the gateway failure: {status}"
            );
            ensure!(
                !gateway.requests()[before..]
                    .iter()
                    .any(|r| r["host"] == "claude.ai"),
                "a request reached the gateway at url instead of failing"
            );

            info!("Checking that turning the mode off removes the pin and the listener");
            container.stop_process("daemon-gateway-down").await?;
            container
                .write(
                    CONFIG,
                    &config(&listen, json!({ "url": gateway.url }), false)?,
                )
                .await?;
            container
                .start_process(
                    "daemon-off",
                    &["agentdesktop", "daemon", "--config", CONFIG],
                )
                .await?;
            container.wait_ready(&["agentdesktop", "status"]).await?;
            container.exec(&["test", "!", "-e", SETTINGS]).await?;
            container.exec(&["test", "!", "-e", OWNER]).await?;
            let status = intercept_status(container).await?;
            ensure!(
                status["enabled"] == false && status["bound"] == false,
                "the listener must not run without intercept mode: {status}"
            );
            ensure!(
                through_listener(container, &proxy, &["https://claude.ai/health"])
                    .await
                    .is_err(),
                "the listener still accepts connections"
            );
            container.stop_process("daemon-off").await?;
            Ok(())
        },
    )
    .await
}

fn config(listen: &str, gateway: Value, intercept: bool) -> anyhow::Result<String> {
    let mut config = json!({
        "daemon": { "intercept": { "listen": listen } },
        "llmGateway": gateway,
        "programs": {},
    });
    if intercept {
        config["programs"]["claudeDesktop"] = json!({ "gatewayMode": "intercept" });
    }
    Ok(serde_json::to_string_pretty(&config)?)
}

fn free_port() -> anyhow::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

async fn through_listener(
    container: &Container,
    proxy: &str,
    args: &[&str],
) -> anyhow::Result<String> {
    let mut command = vec![
        "curl",
        "-sS",
        "--max-time",
        "10",
        "--proxy",
        proxy,
        "--cacert",
        CA,
    ];
    command.extend_from_slice(args);
    container.exec(&command).await
}

async fn intercept_status(container: &Container) -> anyhow::Result<Value> {
    let info = container
        .exec(&[
            "curl",
            "-sS",
            "--fail",
            "--unix-socket",
            SOCKET,
            "http://localhost/v1/daemon-info",
        ])
        .await?;
    let info: Value = serde_json::from_str(&info).context("decode daemon-info")?;
    Ok(info["intercept"].clone())
}

/// The listener starts after the first configuration is applied, so wait for it.
async fn wait_bound(container: &Container) -> anyhow::Result<Value> {
    timeout(Duration::from_secs(30), async {
        loop {
            if let Ok(status) = intercept_status(container).await
                && status["bound"] == true
            {
                return status;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("the intercept listener did not start")
}
