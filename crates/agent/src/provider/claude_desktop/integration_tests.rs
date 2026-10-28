use std::time::Duration;

use anyhow::{Context, ensure};
use serde_json::{Value, json};
use tokio::time::{sleep, timeout};
use tracing::info;

use crate::common::{
    Agentgateway, Container, Gateway, Issuer,
    issuer::{AUDIENCE, CLIENT_ID},
};

const CONFIG: &str = "/tmp/agentdesktop.yaml";
const SETTINGS: &str = "/etc/claude-desktop/managed-settings.json";
const OWNER: &str = "/etc/claude-desktop/.managed-settings.json.owner";
const SOCKET: &str = "/run/agentdesktop/agentdesktop.sock";
const CA: &str = "/var/lib/agentdesktop/intercept-ca.pem";
const DOCKERFILE: &str = "crates/agent/src/provider/claude_desktop/testdata/Dockerfile";

/// Claude Desktop has no Linux release, so curl stands in for it: it uses the
/// proxy that the daemon pins in the managed settings and trusts only the
/// daemon's intercept CA.
#[tokio::test]
async fn intercept_lifecycle() -> anyhow::Result<()> {
    Container::run("claude-desktop", DOCKERFILE, async |container| {
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

/// The listener in front of a real agentgateway that requires the identity
/// header. A fake OIDC provider signs the daemon in with device
/// authorization, and the test origin stands in for Anthropic.
#[tokio::test]
async fn identity_through_agentgateway() -> anyhow::Result<()> {
    Container::run("claude-desktop", DOCKERFILE, async |container| {
        let origin = Gateway::start().await?;
        let origin_address = origin
            .url
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_owned();
        let issuer = Issuer::start().await?;
        let gateway_port = free_port()?;
        let agentgateway = Agentgateway::start(
            &agentgateway_config(gateway_port, free_port()?, &origin_address, &issuer.url)?,
            gateway_port,
        )
        .await?;
        let listen = format!("127.0.0.1:{}", free_port()?);
        let proxy = format!("http://{listen}");
        container
            .write(
                CONFIG,
                &config(
                    &listen,
                    json!({
                        "url": format!("http://127.0.0.1:{gateway_port}/"),
                        "authentication": {
                            "type": "oidc",
                            "issuer": issuer.url,
                            "clientId": CLIENT_ID,
                            "allowInsecure": true,
                            "deviceAuthorization": true,
                        },
                    }),
                    true,
                )?,
            )
            .await?;
        container
            .start_process("daemon", &["agentdesktop", "daemon", "--config", CONFIG])
            .await?;
        container.wait_ready(&["agentdesktop", "status"]).await?;
        wait_bound(container).await?;

        info!("Checking that requests fail until the user signs in");
        let login = container.exec(&["agentdesktop", "login"]).await?;
        ensure!(
            login.starts_with("awaitingAuthentication"),
            "unexpected login status: {login}"
        );
        let code = through_listener(
            container,
            &proxy,
            &["-o", "/dev/null", "-w", "%{http_code}", "https://claude.ai/health"],
        )
        .await?;
        ensure!(code == "502", "expected 502 before sign-in, got {code}");
        let status = intercept_status(container).await?;
        ensure!(
            status["lastFailure"]
                .as_str()
                .is_some_and(|failure| failure.contains("sign-in required")),
            "daemon-info does not show that sign-in is required: {status}"
        );
        ensure!(
            origin.requests().is_empty(),
            "a request reached the origin before sign-in"
        );

        info!("Approving the sign-in");
        issuer.approve();
        container
            .wait_ready(&["sh", "-c", "agentdesktop login | grep -qx signedIn"])
            .await?;

        info!("Sending requests through the listener and agentgateway");
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
            &[
                "--fail",
                "--http1.1",
                "-H",
                "Authorization: Bearer client-token",
                "https://api.anthropic.com/health",
            ],
        )
        .await?;
        ensure!(issuer.issued() >= 1, "the daemon never got a token");
        let requests = origin.requests();
        ensure!(
            requests.iter().any(|r| {
                r["path"] == "/health"
                    && r["host"] == "claude.ai"
                    && r["cookie"] == "sessionKey=integration"
                    && r["identity"].is_null()
            }),
            "the origin did not get the claude.ai request with its cookie and without the identity header: {requests:?}"
        );
        ensure!(
            requests.iter().any(|r| {
                r["host"] == "api.anthropic.com"
                    && r["authorization"] == "Bearer client-token"
                    && r["identity"].is_null()
            }),
            "the origin did not get the api.anthropic.com request with its own Authorization: {requests:?}"
        );

        info!("Checking that agentgateway refuses requests without a valid identity");
        let token = format!("Bearer {}", issuer.token());
        for (identity, expected) in [(None, "401"), (Some("Bearer spoofed"), "401"), (Some(token.as_str()), "200")] {
            let code = direct(container, gateway_port, identity).await?;
            ensure!(
                code == expected,
                "agentgateway answered {code} instead of {expected} for identity {identity:?}\n{}",
                agentgateway.logs().await
            );
        }

        container.stop_process("daemon").await?;
        agentgateway.stop().await
    })
    .await
}

fn agentgateway_config(
    port: u16,
    admin_port: u16,
    origin: &str,
    issuer: &str,
) -> anyhow::Result<String> {
    // The daemon sends `x-agentdesktop-identity: Bearer <token>`. jwtAuth
    // removes the header after it validates the token.
    let identity = json!({
        "mode": "strict",
        "location": { "header": { "name": "x-agentdesktop-identity", "prefix": "Bearer " } },
        "issuer": issuer,
        "audiences": [AUDIENCE],
        "jwks": { "url": format!("{issuer}/keys") },
    });
    let route = |name: &str, host: &str| {
        json!({
            "name": name,
            "gateways": "intercept",
            "hostnames": [host],
            "policies": { "jwtAuth": identity },
            "backends": [{ "host": origin }],
        })
    };
    Ok(serde_json::to_string_pretty(&json!({
        "config": {
            "adminAddr": format!("127.0.0.1:{admin_port}"),
            "statsAddr": "off",
            "readinessAddr": "off",
        },
        "gateways": {
            "intercept": { "port": port, "bindAddress": "127.0.0.1", "protocol": "HTTP" },
        },
        "routes": [route("claude-ai", "claude.ai"), route("anthropic-api", "api.anthropic.com")],
    }))?)
}

/// A request for claude.ai straight to agentgateway, bypassing the listener.
async fn direct(
    container: &Container,
    gateway_port: u16,
    identity: Option<&str>,
) -> anyhow::Result<String> {
    let url = format!("http://127.0.0.1:{gateway_port}/health");
    let header = identity.map(|identity| format!("x-agentdesktop-identity: {identity}"));
    let mut command = vec![
        "curl",
        "-sS",
        "--max-time",
        "10",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "-H",
        "Host: claude.ai",
    ];
    if let Some(header) = &header {
        command.extend(["-H", header.as_str()]);
    }
    command.push(&url);
    container.exec(&command).await
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
