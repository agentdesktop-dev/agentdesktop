use std::time::Duration;

use agentdesktop_core::{DEFAULT_SOCKET_PATH, model::Discovery};
use anyhow::{Context, ensure};
use tracing::info;

use crate::common::Container;

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
