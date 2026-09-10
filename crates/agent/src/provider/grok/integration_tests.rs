use std::time::Duration;

use agentdesktop_core::{DEFAULT_SOCKET_PATH, model::Discovery};
use anyhow::{Context, ensure};
use tracing::info;

use crate::common::Container;

const VERSION: &str = "1.0.13";
const USER_MCP: &str = "/home/tester/.grok/config.toml";
const WORKSPACE_MCP: &str = "/home/tester/project/.grok/config.toml";
const USER_SKILL: &str = "/home/tester/.grok/skills/test/SKILL.md";
const WORKSPACE_SKILL: &str = "/home/tester/project/.grok/skills/test/SKILL.md";

#[tokio::test]
async fn headless_discovery() -> anyhow::Result<()> {
    Container::run("grok", "crates/agent/src/provider/grok/testdata/Dockerfile", async |container| {
        info!("Checking installed Grok Build CLI without a display");
        let version = container.exec_as("tester", &["grok", "--version"], Duration::from_secs(15)).await?;
        ensure!(version.split_whitespace().nth(1) == Some(VERSION), "unexpected Grok Build version: {version}");
        let files = [
            (USER_MCP, "[mcp_servers.user-docs]\nurl = \"https://example.test/mcp\"\nheaders = { Authorization = \"fixture-secret\" }\n"),
            (WORKSPACE_MCP, "disabled_mcp_servers = [\"workspace-local\"]\n[mcp_servers.workspace-local]\ncommand = \"fixture-command\"\nargs = [\"fixture-secret\"]\n"),
            (USER_SKILL, "---\nname: user-skill\n---\nfixture-body-not-metadata\n"),
            (WORKSPACE_SKILL, "---\nname: workspace-skill\n---\nfixture-body-not-metadata\n"),
        ];
        for (path, contents) in files { container.write(path, contents).await?; }
        container.write("/tmp/agentdesktop.yaml", "programs: {}\n").await?;
        container.start_process("daemon", &["agentdesktop", "daemon", "--config", "/tmp/agentdesktop.yaml"]).await?;
        container.wait_ready(&["agentdesktop", "status"]).await?;

        info!("Checking Grok Build discovery and inventory");
        let output = container.exec(&["curl", "--fail", "--silent", "--unix-socket", DEFAULT_SOCKET_PATH, "http://localhost/v1/discovery"]).await?;
        let discovery: Discovery = serde_json::from_str(&output)?;
        let agent = discovery.agents.iter().find(|agent| agent.kind == "grok").context("Grok Build was not discovered")?;
        ensure!(agent.version.is_none(), "fresh installation unexpectedly has version metadata: {:?}", agent.version);
        ensure!(agent.mcp_servers.len() == 2, "unexpected MCP servers: {:?}", agent.mcp_servers);
        for (name, transport, enabled, source, command, url) in [
            ("user-docs", "http", true, USER_MCP, None, Some("https://example.test/mcp")),
            ("workspace-local", "stdio", false, WORKSPACE_MCP, Some("fixture-command"), None),
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
        for (path, contents) in files { ensure!(container.read(path).await? == contents, "discovery changed {path}"); }

        // Grok's update cache is optional; the native installer does not create it.
        info!("Checking discovery with Grok version metadata");
        container.write("/home/tester/.grok/version.json", &serde_json::json!({"version": VERSION}).to_string()).await?;
        container.start_process("daemon-version", &["agentdesktop", "daemon", "--config", "/tmp/agentdesktop.yaml"]).await?;
        container.wait_ready(&["agentdesktop", "status"]).await?;
        let output = container.exec(&["curl", "--fail", "--silent", "--unix-socket", DEFAULT_SOCKET_PATH, "http://localhost/v1/discovery"]).await?;
        let discovery: Discovery = serde_json::from_str(&output)?;
        let agent = discovery.agents.iter().find(|agent| agent.kind == "grok").context("Grok Build was not discovered")?;
        ensure!(agent.version.as_deref() == Some(VERSION), "incorrect discovered version: {:?}", agent.version);
        container.stop_process("daemon-version").await?;
        Ok(())
    }).await
}
