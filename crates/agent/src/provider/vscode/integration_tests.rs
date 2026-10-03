use std::time::Duration;

use agentdesktop_core::{DEFAULT_SOCKET_PATH, model::Discovery};
use anyhow::{Context, ensure};
use tracing::info;

use crate::common::{Container, Gateway};

const VERSION: &str = "1.136.2";
const USER_MCP: &str = "/home/tester/.config/Code/User/mcp.json";
const PROFILE_MCP: &str = "/home/tester/.config/Code/User/profiles/test/mcp.json";
const WORKSPACE_MCP: &str = "/home/tester/project/.vscode/mcp.json";
const COPILOT_MCP: &str = "/home/tester/.copilot/mcp-config.json";
const USER_SKILL: &str = "/home/tester/.copilot/skills/test/SKILL.md";
const WORKSPACE_SKILL: &str = "/home/tester/project/.github/skills/test/SKILL.md";

#[tokio::test]
async fn headless_discovery() -> anyhow::Result<()> {
    Container::run("vscode", "crates/agent/src/provider/vscode/testdata/Dockerfile", async |container| {
        info!("Checking installed VS Code CLI without a display");
        let installed = container.exec_as("tester", &["code", "--version"], Duration::from_secs(15)).await?;
        ensure!(installed.lines().next() == Some(VERSION), "unexpected VS Code version: {installed}");
        let extensions = container.exec_as("tester", &["code", "--list-extensions"], Duration::from_secs(15)).await?;
        ensure!(extensions.trim().is_empty(), "fresh installation has unexpected extensions: {extensions}");

        let files = [
            (USER_MCP, r#"{
                // User MCP settings support comments and trailing commas.
                "servers": { "user-docs": { "type": "http", "url": "https://example.test/mcp", "headers": { "Authorization": "fixture-secret" } }, },
            }"#),
            (PROFILE_MCP, r#"{"servers":{"profile-events":{"type":"sse","url":"https://example.test/events","disabled":true}}}"#),
            (WORKSPACE_MCP, r#"{"servers":{"workspace-local":{"type":"stdio","command":"fixture-command","args":["fixture-secret"],"env":{"TOKEN":"fixture-secret"}}}}"#),
            (COPILOT_MCP, r#"{"mcpServers":{"copilot-docs":{"type":"streamable-http","url":"https://example.test/copilot"}}}"#),
            (USER_SKILL, "---\nname: user-skill\ndescription: User test skill\n---\nfixture-body-not-metadata\n"),
            (WORKSPACE_SKILL, "---\nname: workspace-skill\ndescription: Workspace test skill\n---\nfixture-body-not-metadata\n"),
        ];
        for (path, contents) in files { container.write(path, contents).await?; }
        container.exec(&["chown", "-R", "tester:tester", "/home/tester"]).await?;
        container.write("/tmp/agentdesktop.yaml", "programs: {}\n").await?;
        container.start_process("daemon", &["agentdesktop", "daemon", "--config", "/tmp/agentdesktop.yaml"]).await?;
        container.wait_ready(&["agentdesktop", "status"]).await?;

        info!("Checking VS Code version, MCP servers, and skills through the daemon");
        let output = container.exec(&["curl", "--fail", "--silent", "--unix-socket", DEFAULT_SOCKET_PATH, "http://localhost/v1/discovery"]).await?;
        let discovery: Discovery = serde_json::from_str(&output)?;
        let agent = discovery.agents.iter().find(|agent| agent.kind == "vscode").context("VS Code was not discovered")?;
        ensure!(agent.version.as_deref() == Some(VERSION), "incorrect discovered version: {:?}", agent.version);
        ensure!(agent.executable.to_string_lossy().ends_with("/code"), "incorrect executable: {:?}", agent.executable);
        ensure!(agent.mcp_servers.len() == 4, "unexpected MCP servers: {:?}", agent.mcp_servers);
        for (name, transport, enabled, source, command, url) in [
            ("user-docs", "http", true, USER_MCP, None, Some("https://example.test/mcp")),
            ("profile-events", "sse", false, PROFILE_MCP, None, Some("https://example.test/events")),
            ("workspace-local", "stdio", true, WORKSPACE_MCP, Some("fixture-command"), None),
            ("copilot-docs", "http", true, COPILOT_MCP, None, Some("https://example.test/copilot")),
        ] {
            let server = agent.mcp_servers.iter().find(|server| server.name == name).with_context(|| format!("missing MCP server {name}"))?;
            ensure!(server.transport == transport && server.enabled == enabled && server.source == std::path::Path::new(source)
                && server.command.as_deref() == command && server.url.as_deref() == url, "incorrect MCP discovery: {server:?}");
        }
        ensure!(agent.skills.len() == 2, "unexpected skills: {:?}", agent.skills);
        for (path, name) in [(USER_SKILL, "user-skill"), (WORKSPACE_SKILL, "workspace-skill")] {
            ensure!(agent.skills.iter().any(|skill| skill.path == std::path::Path::new(path) && skill.front_matter.get("name").and_then(|value| value.as_str()) == Some(name)), "missing skill {name}");
        }
        ensure!(!output.contains("fixture-secret") && !output.contains("fixture-body-not-metadata"), "discovery exposed non-metadata content");
        container.stop_process("daemon").await?;
        for (path, contents) in files {
            ensure!(container.read(path).await? == contents, "discovery changed {path}");
        }
        Ok(())
    }).await
}

const CONFIG: &str = "/tmp/agentdesktop-chat.yaml";
const CHAT_MODELS: &str = "/home/tester/.config/Code/User/chatLanguageModels.json";
const CHAT_SIDECAR: &str = "/home/tester/.config/Code/User/.chatLanguageModels.json.agentdesktop";
const PAIRING: &str = "/home/tester/.local/state/agentdesktop/llm-proxy-pairing";
const SOCKET: &str = "/run/user/1000/agentdesktop.sock";
const LISTEN: &str = "127.0.0.1:18096";

/// The daemon runs as the user (`--user` through `runuser`, since the file
/// lives in the user's VS Code profile), writes `chatLanguageModels.json`
/// pointed at its loopback proxy, a client using the file's `url` and
/// `requestHeaders` reaches the gateway through the proxy, and removing the
/// program leaves the user's own vendor entry at the file's mode.
#[tokio::test]
async fn managed_chat_models_lifecycle() -> anyhow::Result<()> {
    Container::run("vscode", "crates/agent/src/provider/vscode/testdata/Dockerfile", async |container| {
        let gateway = Gateway::start().await?;
        let user_vendor = serde_json::json!([{
            "name": "my-own", "vendor": "customendpoint", "apiKey": "sk-user", "apiType": "chat-completions",
            "models": [{"id": "gpt-mine", "name": "mine", "url": "https://example.invalid/v1/chat/completions"}],
        }]);
        container.write(CHAT_MODELS, &serde_json::to_string_pretty(&user_vendor)?).await?;
        container.exec(&["chown", "-R", "tester:tester", "/home/tester/.config"]).await?;
        // A mode the daemon does not write itself, so "mode kept" can fail.
        container.exec(&["chmod", "640", CHAT_MODELS]).await?;
        let config = |with_program: bool| {
            let mut document = serde_json::json!({
                "daemon": { "user": true, "llmProxy": { "listen": LISTEN } },
                "llmGateway": { "url": gateway.url },
                "programs": {},
            });
            if with_program {
                document["programs"]["vscode"] =
                    serde_json::json!({ "models": { "gpt-4.1-mini": { "maxInputTokens": 128000 } } });
            }
            serde_json::to_string_pretty(&document).unwrap()
        };
        container.write(CONFIG, &config(true)).await?;
        container.exec(&["chown", "tester:tester", CONFIG]).await?;
        let daemon = ["runuser", "-u", "tester", "--", "agentdesktop", "daemon", "--config", CONFIG];
        container.start_process("daemon", &daemon).await?;
        container.wait_ready(&["agentdesktop", "--socket", SOCKET, "status"]).await?;

        info!("Checking the written chat language models file");
        let pairing = container.exec(&["cat", PAIRING]).await?;
        let pairing = pairing.trim().to_owned();
        let written: serde_json::Value = serde_json::from_str(&container.exec(&["cat", CHAT_MODELS]).await?)?;
        let vendors = written.as_array().cloned().context("chatLanguageModels.json is not an array")?;
        ensure!(vendors.iter().any(|vendor| vendor["name"] == "my-own"), "user vendor entry lost: {written}");
        let ours = vendors.iter().find(|vendor| vendor["name"] == "agentdesktop").cloned().context("managed vendor entry missing")?;
        let model = &ours["models"][0];
        ensure!(model["id"] == "gpt-4.1-mini" && model["url"] == format!("http://{LISTEN}/vscode-copilot/v1/chat/completions"), "unexpected model: {ours}");
        ensure!(model["requestHeaders"]["x-agentdesktop-pairing"] == pairing, "pairing header mismatch: {ours}");
        ensure!(ours["apiKey"] == "unused", "no secret may be written as apiKey: {ours}");
        let mode = container.exec(&["stat", "-c", "%a", CHAT_MODELS]).await?;
        ensure!(mode.trim() == "600", "a file the daemon writes is owner-only, got {mode}");
        container.exec(&["test", "-e", CHAT_SIDECAR]).await?;

        info!("Checking that a client using the file reaches the gateway through the proxy");
        let url = model["url"].as_str().context("model url")?.to_owned();
        let status = container.exec_as("tester", &["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}", "-H", &format!("x-agentdesktop-pairing: {pairing}"), "-H", "authorization: Bearer client-token", "-H", "x-api-key: client-key", "-H", "content-type: application/json", "-X", "POST", &url, "-d", "{\"model\":\"gpt-4.1-mini\",\"messages\":[]}"], Duration::from_secs(15)).await?;
        // The stub knows no /v1/chat/completions route and answers 404, which
        // the proxy passes through; what matters is that the request arrived
        // upstream with the client's own Authorization and x-api-key stripped
        // (the curl sends both) and, with no gateway authentication configured
        // here, no Authorization added.
        let requests = gateway.requests();
        let upstream = requests.iter().find(|request| request["path"] == "/v1/chat/completions").with_context(|| format!("no request reached the gateway (client saw {status}): {requests:?}"))?;
        ensure!(upstream["authorization"].is_null() && upstream["apiKey"].is_null(), "client headers must not reach the gateway: {upstream}");

        info!("Removing the program");
        // The daemon wrote the file 600 while managed; the user then set 640
        // by hand, which removal must keep.
        container.exec(&["chmod", "640", CHAT_MODELS]).await?;
        container.write(CONFIG, &config(false)).await?;
        container.stop_process("daemon").await?;
        container.start_process("daemon", &daemon).await?;
        container.wait_ready(&["agentdesktop", "--socket", SOCKET, "status"]).await?;
        let remaining: serde_json::Value = serde_json::from_str(&container.exec(&["cat", CHAT_MODELS]).await?)?;
        let vendors = remaining.as_array().cloned().unwrap_or_default();
        ensure!(vendors.len() == 1 && vendors[0]["name"] == "my-own", "managed entry not removed: {remaining}");
        let mode = container.exec(&["stat", "-c", "%a", CHAT_MODELS]).await?;
        ensure!(mode.trim() == "640", "removal must keep the user's file mode, got {mode}");
        container.exec(&["test", "!", "-e", CHAT_SIDECAR]).await?;
        Ok(())
    })
    .await
}

const SETTINGS: &str = "/home/tester/.config/Code/User/settings.json";
const SETTINGS_SIDECAR: &str = "/home/tester/.config/Code/User/.settings.json.agentdesktop";
const SETTINGS_LISTEN: &str = "127.0.0.1:18097";
const OVERRIDE_KEY: &str = "github.copilot.advanced.debug.overrideCapiUrl";
const CAPI_ALIAS_KEY: &str = "github.copilot.internal.capiUrl";
const IGNORED_KEY: &str = "settingsSync.ignoredSettings";

/// A hand-rolled RFC 6455 echo on the host loopback (the container shares the
/// host network): answers the handshake, echoes the first frame unmasked, then
/// waits for the client to close. The request head is reported on the channel.
async fn websocket_echo() -> anyhow::Result<(String, tokio::sync::mpsc::UnboundedReceiver<String>)>
{
    use base64::Engine;
    use sha1::Digest;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/", listener.local_addr()?);
    let (heads, seen) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let heads = heads.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut byte = [0u8; 1];
                while !buffer.ends_with(b"\r\n\r\n") {
                    if socket
                        .read(&mut byte)
                        .await
                        .ok()
                        .filter(|read| *read == 1)
                        .is_none()
                    {
                        return;
                    }
                    buffer.push(byte[0]);
                }
                let head = String::from_utf8_lossy(&buffer).to_string();
                let key = head
                    .lines()
                    .find_map(|line| {
                        line.split_once(':')
                            .filter(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-key"))
                    })
                    .map(|(_, value)| value.trim().to_owned())
                    .unwrap_or_default();
                let _ = heads.send(head);
                let accept = base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(
                    format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
                ));
                let response = format!(
                    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                );
                if socket.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
                // One masked text frame with a payload under 126 bytes.
                let mut header = [0u8; 2];
                if socket.read_exact(&mut header).await.is_err() {
                    return;
                }
                let length = usize::from(header[1] & 0x7f);
                let mut mask = [0u8; 4];
                if header[1] & 0x80 != 0 && socket.read_exact(&mut mask).await.is_err() {
                    return;
                }
                let mut payload = vec![0u8; length];
                if socket.read_exact(&mut payload).await.is_err() {
                    return;
                }
                for (index, byte) in payload.iter_mut().enumerate() {
                    *byte ^= mask[index % 4];
                }
                let mut frame = vec![0x81, length as u8];
                frame.extend(payload);
                let _ = socket.write_all(&frame).await;
                let _ = socket.read(&mut byte).await;
            });
        }
    });
    Ok((url, seen))
}

/// Opens a WebSocket through the proxy from the host (same loopback as the
/// container) and returns the response head and the first echoed payload.
async fn websocket_through_proxy(url: &str) -> anyhow::Result<(String, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let uri: hyper::Uri = url.parse()?;
    let authority = uri.authority().context("proxy authority")?.to_string();
    let mut socket = tokio::net::TcpStream::connect(&authority).await?;
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {authority}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nAuthorization: Bearer tid=x\r\n\r\n",
        uri.path()
    );
    socket.write_all(request.as_bytes()).await?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        socket
            .read_exact(&mut byte)
            .await
            .context("response head")?;
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_string();
    ensure!(
        head.starts_with("HTTP/1.1 101"),
        "no 101 from the proxy: {head}"
    );
    // "hi", masked with a zero key (legal, keeps the bytes readable).
    socket
        .write_all(&[0x81, 0x82, 0, 0, 0, 0, b'h', b'i'])
        .await?;
    let mut frame = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(10), socket.read_exact(&mut frame))
        .await
        .context("echo frame")??;
    ensure!(
        frame[0] == 0x81 && frame[1] == 2,
        "unexpected frame header {:?}",
        &frame[..2]
    );
    Ok((head, String::from_utf8_lossy(&frame[2..]).to_string()))
}

#[tokio::test]
async fn managed_settings_github_models_lifecycle() -> anyhow::Result<()> {
    Container::run("vscode", "crates/agent/src/provider/vscode/testdata/Dockerfile", async |container| {
        let gateway = Gateway::start().await?;
        let (echo_url, mut echo_heads) = websocket_echo().await?;
        // A commented file with a trailing comma, as VS Code users write it:
        // the daemon edits it in place and the comment must survive.
        let user_settings = format!(
            "{{\n  // keep this comment\n  \"editor.fontSize\": 14,\n  \"{IGNORED_KEY}\": [\"workbench.colorTheme\"],\n}}\n"
        );
        container.write(SETTINGS, &user_settings).await?;
        container.exec(&["chown", "-R", "tester:tester", "/home/tester/.config"]).await?;
        // VS Code's own default for a settings file it created.
        container.exec(&["chmod", "644", SETTINGS]).await?;
        let config = |with_program: bool, proxy_url: &str| {
            let mut document = serde_json::json!({
                "daemon": { "user": true, "llmProxy": { "listen": SETTINGS_LISTEN } },
                "llmGateway": { "url": gateway.url, "proxyUrl": proxy_url },
                "programs": {},
            });
            if with_program {
                document["programs"]["vscode"] = serde_json::json!({ "copilotChat": "githubModels" });
            }
            serde_json::to_string_pretty(&document).unwrap()
        };
        container.write(CONFIG, &config(true, &gateway.url)).await?;
        container.exec(&["chown", "tester:tester", CONFIG]).await?;
        let daemon = ["runuser", "-u", "tester", "--", "agentdesktop", "daemon", "--config", CONFIG];
        container.start_process("daemon", &daemon).await?;
        container.wait_ready(&["agentdesktop", "--socket", SOCKET, "status"]).await?;

        info!("Checking the written settings file");
        let pairing = container.exec(&["cat", PAIRING]).await?;
        let pairing = pairing.trim().to_owned();
        let written_text = container.exec(&["cat", SETTINGS]).await?;
        ensure!(written_text.contains("// keep this comment"), "comment lost on apply: {written_text}");
        let written = jsonc(&written_text)?;
        let override_url = format!("http://{SETTINGS_LISTEN}/vscode-copilot-capi/{pairing}");
        ensure!(written[OVERRIDE_KEY] == override_url, "override URL: {written}");
        let ignored = written[IGNORED_KEY].as_array().cloned().context("ignoredSettings")?;
        for entry in ["workbench.colorTheme", OVERRIDE_KEY, CAPI_ALIAS_KEY] {
            ensure!(ignored.iter().any(|value| value == entry), "{entry} missing from {ignored:?}");
        }
        ensure!(written["editor.fontSize"] == 14, "user setting lost: {written}");
        let mode = container.exec(&["stat", "-c", "%a", SETTINGS]).await?;
        ensure!(mode.trim() == "600", "a file carrying the pairing is tightened to owner-only, got {mode}");
        container.exec(&["test", "-e", SETTINGS_SIDECAR]).await?;

        info!("Checking that a request with the URL's pairing segment reaches the gateway with the client token moved");
        let url = format!("{override_url}/v1/messages");
        let status = container.exec_as("tester", &["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}", "-H", "authorization: Bearer tid=x", "-H", "content-type: application/json", "-X", "POST", &url, "-d", "{\"model\":\"gpt-4.1\"}"], Duration::from_secs(15)).await?;
        // The stub rejects any token but its own with 401, which the proxy
        // passes through (a client token was forwarded, so no retry).
        ensure!(status.trim() == "401", "expected the stub's 401 passed through, got {status}");
        let requests = gateway.requests();
        let upstream = requests.iter().find(|request| request["path"] == "/v1/messages").with_context(|| format!("no request reached the gateway (client saw {status}): {requests:?}"))?;
        ensure!(upstream["authorization"].is_null(), "the client's Authorization must not reach the gateway as is: {upstream}");
        ensure!(upstream["llmToken"] == "tid=x", "the client token goes to x-llm-token: {upstream}");
        ensure!(requests.iter().all(|request| !request["path"].as_str().unwrap_or_default().contains(&pairing)), "the pairing must never be forwarded: {requests:?}");

        info!("Checking the WebSocket tunnel against a host-side echo behind proxyUrl");
        container.write(CONFIG, &config(true, &echo_url)).await?;
        container.stop_process("daemon").await?;
        container.start_process("daemon", &daemon).await?;
        container.wait_ready(&["agentdesktop", "--socket", SOCKET, "status"]).await?;
        let (head, echoed) = websocket_through_proxy(&format!("{override_url}/responses")).await?;
        ensure!(echoed == "hi", "echo payload: {echoed:?}");
        ensure!(head.to_ascii_lowercase().contains("sec-websocket-accept:"), "101 without the accept header: {head}");
        let seen = tokio::time::timeout(Duration::from_secs(5), echo_heads.recv()).await.context("echo saw no request")?.context("echo closed")?;
        let seen_lower = seen.to_ascii_lowercase();
        ensure!(seen_lower.contains("x-llm-token: tid=x"), "client token not moved on the upgrade: {seen}");
        ensure!(!seen.contains(&pairing), "pairing forwarded on the upgrade: {seen}");

        info!("Removing the program");
        // A mode the daemon does not write itself, so "mode kept" can fail.
        container.exec(&["chmod", "640", SETTINGS]).await?;
        container.write(CONFIG, &config(false, &gateway.url)).await?;
        container.stop_process("daemon").await?;
        container.start_process("daemon", &daemon).await?;
        container.wait_ready(&["agentdesktop", "--socket", SOCKET, "status"]).await?;
        let remaining_text = container.exec(&["cat", SETTINGS]).await?;
        ensure!(remaining_text.contains("// keep this comment"), "comment lost on removal: {remaining_text}");
        let remaining = jsonc(&remaining_text)?;
        ensure!(remaining.get(OVERRIDE_KEY).is_none(), "override not removed: {remaining}");
        ensure!(remaining[IGNORED_KEY] == serde_json::json!(["workbench.colorTheme"]), "user ignoredSettings entry not kept alone: {remaining}");
        ensure!(remaining["editor.fontSize"] == 14, "user setting lost on removal: {remaining}");
        let mode = container.exec(&["stat", "-c", "%a", SETTINGS]).await?;
        ensure!(mode.trim() == "640", "removal must keep the user's file mode, got {mode}");
        container.exec(&["test", "!", "-e", SETTINGS_SIDECAR]).await?;
        Ok(())
    })
    .await
}

/// Parses VS Code's JSONC (comments, trailing commas) into a value.
fn jsonc(text: &str) -> anyhow::Result<serde_json::Value> {
    let options = jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    };
    let root = jsonc_parser::cst::CstRootNode::parse(text, &options)?;
    root.value()
        .and_then(|value| value.to_serde_value())
        .context("settings.json holds no JSON value")
}

#[tokio::test]
async fn program_status_reports_inactive_without_the_proxy() -> anyhow::Result<()> {
    Container::run(
        "vscode",
        "crates/agent/src/provider/vscode/testdata/Dockerfile",
        async |container| {
            let gateway = Gateway::start().await?;
            // A gateway and the VS Code program, but no daemon.llmProxy: the
            // program uses the gateway without a listener, so it is inactive.
            let document = serde_json::json!({
                "daemon": { "user": true },
                "llmGateway": { "url": gateway.url },
                "programs": { "vscode": { "models": { "gpt-4.1-mini": {} } } },
            });
            container
                .write(CONFIG, &serde_json::to_string_pretty(&document)?)
                .await?;
            container.exec(&["chown", "tester:tester", CONFIG]).await?;
            container
                .exec(&["chown", "-R", "tester:tester", "/home/tester"])
                .await?;
            let daemon = [
                "runuser",
                "-u",
                "tester",
                "--",
                "agentdesktop",
                "daemon",
                "--config",
                CONFIG,
            ];
            container.start_process("daemon", &daemon).await?;
            container
                .wait_ready(&["agentdesktop", "--socket", SOCKET, "status"])
                .await?;

            info!("Checking the program outcome line in the daemon log");
            let log = container.exec(&["cat", "/tmp/daemon.log"]).await?;
            let line = log
                .lines()
                .find(|line| {
                    line.contains("program configuration outcome") && line.contains("vscode")
                })
                .with_context(|| format!("no outcome line for vscode in the daemon log:\n{log}"))?;
            ensure!(
                line.contains("inactive"),
                "vscode must be inactive without the proxy: {line}"
            );
            ensure!(
                line.contains("local LLM proxy not available"),
                "the reason is in the detail: {line}"
            );
            Ok(())
        },
    )
    .await
}
