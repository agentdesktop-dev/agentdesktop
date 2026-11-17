use std::time::Duration;

use agentdesktop_core::{DEFAULT_SOCKET_PATH, model::Discovery};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use tracing::info;

use crate::common::{Container, Gateway};

const CONFIG: &str = "/tmp/agentdesktop.yaml";
const SETTINGS: &str = "/home/tester/.config/muse/settings.json";
const SIDECAR: &str = "/home/tester/.config/muse/.settings.json.agentdesktop";
const VERSION_STATE: &str = "/home/tester/.local/bin/.muse-version";
const WORKSPACE_MCP: &str = "/home/tester/project/.mcp.json";
const USER_SKILL: &str = "/home/tester/.config/muse/skills/user-skill/SKILL.md";
const WORKSPACE_SKILL: &str = "/home/tester/project/.agents/skills/workspace-skill/SKILL.md";

/// Discovery runs as the system daemon; managed configuration runs as the
/// user (`--user`), because Muse Code reads its transport from the user's
/// `settings.json`. The gateway is unauthenticated, so no credential helper
/// (which needs enrollment) is involved.
#[tokio::test]
async fn discovery_and_user_mode_settings_lifecycle() -> anyhow::Result<()> {
    Container::run("muse", "crates/agent/src/provider/muse/testdata/Dockerfile", async |container| {
        info!("Checking installed Muse Code CLI without a display");
        let reported = container.exec_as("tester", &["muse", "--version"], Duration::from_secs(30)).await?;
        let state = container.read(VERSION_STATE).await?;
        let version = state.trim().split("-R").next().unwrap_or_default().to_owned();
        ensure!(!version.is_empty() && reported.contains(&version), "launcher state {state:?} does not match {reported:?}");

        let user_settings = json!({
            "schema_version": 1,
            "model": "fixture-model",
            "mcpServers": {
                "user-docs": {"type": "streamable-http", "url": "https://example.test/mcp", "headers": {"Authorization": "Bearer fixture-secret"}},
                "user-off": {"type": "stdio", "command": "fixture-command", "args": ["fixture-secret"], "enabled": false},
            },
        });
        let user_settings = serde_json::to_string_pretty(&user_settings)?;
        let files = [
            (SETTINGS, user_settings.as_str()),
            (WORKSPACE_MCP, "{\"mcpServers\":{\"workspace-local\":{\"type\":\"stdio\",\"command\":\"fixture-command\",\"env\":{\"TOKEN\":\"fixture-secret\"}}}}"),
            (USER_SKILL, "---\nname: user-skill\ndescription: A user fixture skill.\n---\nfixture-body-not-metadata\n"),
            (WORKSPACE_SKILL, "---\nname: workspace-skill\ndescription: A workspace fixture skill.\n---\nfixture-body-not-metadata\n"),
        ];
        for (path, contents) in files { container.write(path, contents).await?; }
        container.exec(&["chown", "-R", "tester:tester", "/home/tester/.config", "/home/tester/project"]).await?;
        container.exec(&["chmod", "644", SETTINGS]).await?;

        info!("Checking that Muse Code reads the fixture settings");
        let listed = container.exec_as("tester", &["muse", "mcp", "list", "--json"], Duration::from_secs(30)).await?;
        ensure!(listed.contains("user-docs") && listed.contains("user-off"), "Muse Code did not load the user settings: {listed}");

        container.write(CONFIG, "programs: {}\n").await?;
        container.start_process("daemon", &["agentdesktop", "daemon", "--config", CONFIG]).await?;
        container.wait_ready(&["agentdesktop", "status"]).await?;

        info!("Checking Muse Code discovery and inventory");
        let output = container.exec(&["curl", "--fail", "--silent", "--unix-socket", DEFAULT_SOCKET_PATH, "http://localhost/v1/discovery"]).await?;
        let discovery: Discovery = serde_json::from_str(&output)?;
        let agent = discovery.agents.iter().find(|agent| agent.kind == "muse").context("Muse Code was not discovered")?;
        ensure!(agent.version.as_deref() == Some(version.as_str()), "incorrect discovered version: {:?}", agent.version);
        ensure!(agent.mcp_servers.len() == 3, "unexpected MCP servers: {:?}", agent.mcp_servers);
        for (name, transport, enabled, source, command, url) in [
            ("user-docs", "http", true, SETTINGS, None, Some("https://example.test/mcp")),
            ("user-off", "stdio", false, SETTINGS, Some("fixture-command"), None),
            ("workspace-local", "stdio", true, WORKSPACE_MCP, Some("fixture-command"), None),
        ] {
            let server = agent.mcp_servers.iter().find(|server| server.name == name).with_context(|| format!("missing MCP server {name}"))?;
            ensure!(server.transport == transport && server.enabled == enabled && server.source == std::path::Path::new(source)
                && server.command.as_deref() == command && server.url.as_deref() == url, "incorrect MCP discovery: {server:?}");
        }
        for (path, name) in [(USER_SKILL, "user-skill"), (WORKSPACE_SKILL, "workspace-skill")] {
            ensure!(agent.skills.iter().any(|skill| skill.path == std::path::Path::new(path) && skill.front_matter.get("name").and_then(|value| value.as_str()) == Some(name)), "missing skill {name}: {:?}", agent.skills);
        }
        ensure!(!output.contains("fixture-secret") && !output.contains("fixture-body-not-metadata"), "discovery exposed non-metadata content");
        container.stop_process("daemon").await?;
        for (path, contents) in files { ensure!(container.read(path).await? == contents, "discovery changed {path}"); }

        info!("Checking that a system daemon skips Muse Code");
        let gateway = Gateway::start().await?;
        let config = |with_program: bool| {
            let mut document = json!({"llmGateway": {"url": gateway.url}, "programs": {}});
            if with_program {
                document["programs"]["muse"] = json!({"managedConfig": {"reasoning_effort": "high"}});
            }
            serde_json::to_string_pretty(&document).unwrap()
        };
        container.write(CONFIG, &config(true)).await?;
        container.exec(&["chown", "tester:tester", CONFIG]).await?;
        let skipped = container.exec(&["agentdesktop", "daemon", "--config", CONFIG, "--dry-run"]).await?;
        ensure!(skipped.contains("Muse Code requires user mode"), "system mode did not report Muse Code inactive: {skipped}");

        info!("Checking the user-mode dry run");
        let user_daemon = |flag: &'static str| ["agentdesktop", "daemon", "--user", "--config", CONFIG, flag];
        let preview = container.exec_as("tester", &user_daemon("--dry-run"), Duration::from_secs(30)).await?;
        ensure!(preview.contains("UPDATE  Muse Code settings") && !preview.contains("fixture-secret"), "unexpected dry run: {preview}");
        ensure!(container.read(SETTINGS).await? == user_settings, "dry run changed the settings");
        container.exec(&["test", "!", "-e", SIDECAR]).await?;

        info!("Applying the managed gateway settings");
        container.exec_as("tester", &user_daemon("--once"), Duration::from_secs(30)).await?;
        let managed: Value = serde_json::from_str(&container.read(SETTINGS).await?)?;
        let base_url = format!("{}v1", gateway.url);
        ensure!(managed["endpoint_transport"] == json!({"base_url": base_url, "auth": "none"}), "unexpected transport: {managed}");
        ensure!(managed["reasoning_effort"] == "high" && managed["model"] == "fixture-model" && managed["schema_version"] == 1, "unexpected settings: {managed}");
        ensure!(managed["mcpServers"]["user-docs"]["headers"]["Authorization"] == "Bearer fixture-secret", "user MCP server lost: {managed}");
        let mode = container.exec(&["stat", "-c", "%a %U", SETTINGS]).await?;
        ensure!(mode.trim() == "600 tester", "settings are not owner-only: {mode}");
        container.exec(&["test", "-f", SIDECAR]).await?;

        info!("Checking that Muse Code accepts the managed settings and calls the gateway");
        let listed = container.exec_as("tester", &["muse", "mcp", "list", "--json"], Duration::from_secs(30)).await?;
        ensure!(listed.contains("user-docs"), "Muse Code rejected the managed settings: {listed}");
        // The fixture gateway has no Muse Code model catalog, so the run fails
        // after the request that shows which endpoint Muse Code uses.
        container.exec_as("tester", &["sh", "-c", "muse exec --no-session-log hi >/tmp/muse-exec.log 2>&1 || true"], Duration::from_secs(60)).await?;
        let requests = gateway.requests();
        ensure!(
            requests.iter().any(|request| request["path"] == "/muse-code/models" && request["authorization"].is_null()),
            "Muse Code did not fetch its catalog from the gateway: {requests:?}\n{}",
            container.read("/tmp/muse-exec.log").await.unwrap_or_default()
        );

        info!("Checking that a repeated apply changes nothing");
        let repeated = container.exec_as("tester", &user_daemon("--dry-run"), Duration::from_secs(30)).await?;
        ensure!(repeated.contains("Summary: 0 changes"), "apply is not idempotent: {repeated}");

        info!("Removing the program restores the user's settings");
        container.write(CONFIG, &config(false)).await?;
        container.exec(&["chown", "tester:tester", CONFIG]).await?;
        container.exec_as("tester", &user_daemon("--once"), Duration::from_secs(30)).await?;
        let restored: Value = serde_json::from_str(&container.read(SETTINGS).await?)?;
        ensure!(restored == serde_json::from_str::<Value>(&user_settings)?, "cleanup did not restore the settings: {restored}");
        container.exec(&["test", "!", "-e", SIDECAR]).await?;
        Ok(())
    }).await
}
