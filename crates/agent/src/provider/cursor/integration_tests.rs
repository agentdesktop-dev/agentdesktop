use std::time::Duration;

use agentdesktop_core::{DEFAULT_SOCKET_PATH, model::Discovery};
use anyhow::{Context, ensure};
use tracing::info;

use crate::common::Container;

const VERSION: &str = "3.19.13";
const USER_MCP: &str = "/home/tester/.cursor/mcp.json";
const WORKSPACE_MCP: &str = "/home/tester/project/.cursor/mcp.json";
const USER_SKILL: &str = "/home/tester/.cursor/skills/test/SKILL.md";
const WORKSPACE_SKILL: &str = "/home/tester/project/.cursor/skills/test/SKILL.md";
const VENDOR_SKILL: &str = "/home/tester/.cursor/skills-cursor/vendor/SKILL.md";

#[tokio::test]
async fn headless_discovery() -> anyhow::Result<()> {
    Container::run("cursor", "crates/agent/src/provider/cursor/testdata/Dockerfile", async |container| {
        info!("Checking installed Cursor CLI without a display");
        let version = container.exec_as("tester", &["cursor", "--version"], Duration::from_secs(15)).await?;
        ensure!(version.lines().next() == Some(VERSION), "unexpected Cursor version: {version}");
        // A code alias to the real Cursor launcher must not be reported as VS Code.
        container.exec(&["sh", "-c", "test -f \"$(command -v code)\""]).await?;
        let files = [
            (USER_MCP, r#"{"mcpServers":{"user-docs":{"type":"streamable-http","url":"https://example.test/mcp","headers":{"Authorization":"fixture-secret"}}}}"#),
            (WORKSPACE_MCP, r#"{
                // Cursor accepts the VS Code MCP schema too.
                "servers": { "workspace-local": { "command": "fixture-command", "args": ["fixture-secret"], "env": { "TOKEN": "fixture-secret" }, "disabled": true, }, },
            }"#),
            (USER_SKILL, "---\nname: user-skill\n---\nfixture-body-not-metadata\n"),
            (WORKSPACE_SKILL, "---\nname: workspace-skill\n---\nfixture-body-not-metadata\n"),
            (VENDOR_SKILL, "---\nname: vendor-skill\n---\nVendor default\n"),
        ];
        for (path, contents) in files { container.write(path, contents).await?; }
        container.write("/tmp/agentdesktop.yaml", "programs: {}\n").await?;
        container.start_process("daemon", &["agentdesktop", "daemon", "--config", "/tmp/agentdesktop.yaml"]).await?;
        container.wait_ready(&["agentdesktop", "status"]).await?;

        info!("Checking Cursor discovery and VS Code fork rejection");
        let output = container.exec(&["curl", "--fail", "--silent", "--unix-socket", DEFAULT_SOCKET_PATH, "http://localhost/v1/discovery"]).await?;
        let discovery: Discovery = serde_json::from_str(&output)?;
        ensure!(!discovery.agents.iter().any(|agent| agent.kind == "vscode"), "Cursor was also reported as VS Code");
        let agent = discovery.agents.iter().find(|agent| agent.kind == "cursor").context("Cursor was not discovered")?;
        ensure!(agent.version.as_deref() == Some(VERSION), "incorrect discovered version: {:?}", agent.version);
        ensure!(agent.mcp_servers.len() == 2, "unexpected MCP servers: {:?}", agent.mcp_servers);
        for (name, transport, enabled, source, command, url) in [
            ("user-docs", "http", true, USER_MCP, None, Some("https://example.test/mcp")),
            ("workspace-local", "stdio", false, WORKSPACE_MCP, Some("fixture-command"), None),
        ] {
            let server = agent.mcp_servers.iter().find(|server| server.name == name).with_context(|| format!("missing MCP server {name}"))?;
            ensure!(server.transport == transport && server.enabled == enabled && server.source == std::path::Path::new(source)
                && server.command.as_deref() == command && server.url.as_deref() == url, "incorrect MCP discovery: {server:?}");
        }
        ensure!(agent.skills.len() == 2, "unexpected skills (vendor defaults must be omitted): {:?}", agent.skills);
        for (path, name) in [(USER_SKILL, "user-skill"), (WORKSPACE_SKILL, "workspace-skill")] {
            ensure!(agent.skills.iter().any(|skill| skill.path == std::path::Path::new(path) && skill.front_matter.get("name").and_then(|value| value.as_str()) == Some(name)), "missing skill {name}");
        }
        ensure!(!output.contains("fixture-secret") && !output.contains("fixture-body-not-metadata"), "discovery exposed non-metadata content");
        container.stop_process("daemon").await?;
        for (path, contents) in files { ensure!(container.read(path).await? == contents, "discovery changed {path}"); }
        Ok(())
    }).await
}
