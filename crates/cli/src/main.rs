use std::{io::Read, path::PathBuf};

use agentdesktop_client as client;
use agentdesktop_core::{
    DEFAULT_SOCKET_PATH,
    config::DaemonConfig,
    model::{Discovery, Health, InferenceGatewayCredential, TelemetryEventKind},
};
use clap::{Parser, Subcommand};
use serde::Deserialize;

const MAX_HOOK_INPUT_BYTES: u64 = 1024 * 1024;

#[derive(Parser)]
#[command(about = "Client for the AgentDesktop daemon")]
struct Args {
    /// Unix socket exposed by the local AgentDesktop daemon.
    #[arg(long, default_value = DEFAULT_SOCKET_PATH, global = true)]
    socket: PathBuf,

    /// Operation to perform against the local daemon.
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check whether the daemon is reachable.
    Status,
    /// Discover locally installed agents.
    Discover,
    /// Print the daemon's local startup configuration.
    Config,
    /// Print a short-lived credential for an inference gateway.
    Credential {
        /// Developer tool requesting the credential.
        #[arg(long, default_value = "agentdesktop-cli")]
        client_id: String,
    },
    /// Handle an event emitted by a managed developer-tool hook.
    Hook {
        #[command(subcommand)]
        hook: HookCommand,
    },
}

#[derive(Subcommand)]
enum HookCommand {
    /// Report a Claude Code PreToolUse event as telemetry.
    ClaudePreToolUse,
}

#[derive(Deserialize)]
struct ClaudePreToolUseInput {
    hook_event_name: Option<String>,
    tool_name: String,
    tool_use_id: Option<String>,
    tool_input: serde_json::Value,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Status => {
            let health: Health = client::get(&args.socket, "/v1/health").await?;
            println!("{}", health.status);
        }
        Command::Discover => {
            let discovery: Discovery = client::get(&args.socket, "/v1/discovery").await?;
            if discovery.agents.is_empty() {
                println!("No agents discovered");
            }
            for agent in discovery.agents {
                let version = agent.version.as_deref().unwrap_or("unknown version");
                println!(
                    "{}\t{}\t{}",
                    agent.kind,
                    version,
                    agent.executable.display()
                );
            }
        }
        Command::Config => {
            let config: DaemonConfig = client::get(&args.socket, "/v1/config").await?;
            print!(
                "{}",
                agentdesktop_core::serdes::yamlviajson::to_string(&config)?
            );
        }
        Command::Credential { client_id } => {
            let client_id: String =
                url::form_urlencoded::byte_serialize(client_id.as_bytes()).collect();
            let response: InferenceGatewayCredential = client::get(
                &args.socket,
                &format!("/v1/inference-gateway/credential?client_id={client_id}"),
            )
            .await?;
            println!("{}", response.credential);
        }
        Command::Hook {
            hook: HookCommand::ClaudePreToolUse,
        } => {
            // Telemetry is deliberately fail-open: an unavailable daemon must
            // never prevent Claude Code from using a tool.
            let _ = report_claude_pre_tool_use(&args.socket).await;
        }
    }

    Ok(())
}

async fn report_claude_pre_tool_use(socket: &std::path::Path) -> anyhow::Result<()> {
    let mut input = Vec::new();
    std::io::stdin()
        .take(MAX_HOOK_INPUT_BYTES + 1)
        .read_to_end(&mut input)?;
    if input.len() as u64 > MAX_HOOK_INPUT_BYTES {
        anyhow::bail!("Claude hook input exceeds {MAX_HOOK_INPUT_BYTES} bytes");
    }
    let event = parse_claude_pre_tool_use(&input)?;
    client::post_json(socket, "/v1/telemetry", &event).await
}

fn parse_claude_pre_tool_use(input: &[u8]) -> anyhow::Result<TelemetryEventKind> {
    let input: ClaudePreToolUseInput = serde_json::from_slice(input)?;
    if input
        .hook_event_name
        .as_deref()
        .is_some_and(|name| name != "PreToolUse")
    {
        anyhow::bail!("expected a Claude Code PreToolUse event");
    }
    if input.tool_name.trim().is_empty() {
        anyhow::bail!("Claude hook event has no tool name");
    }

    Ok(TelemetryEventKind::ToolUse {
        client_id: "claude-code".to_owned(),
        tool_name: input.tool_name,
        tool_use_id: input.tool_use_id.filter(|id| !id.is_empty()),
        tool_input: input.tool_input,
    })
}

#[cfg(test)]
mod tests {
    use agentdesktop_core::model::TelemetryEventKind;

    use super::parse_claude_pre_tool_use;

    #[test]
    fn parses_claude_pre_tool_use_input_without_unrelated_fields() {
        let event = parse_claude_pre_tool_use(
            br#"{
                "session_id": "session-secret",
                "transcript_path": "/tmp/transcript.jsonl",
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_use_id": "tool-1",
                "tool_input": {"command": "cargo test"}
            }"#,
        )
        .expect("valid Claude hook input");

        let serialized = serde_json::to_string(&event).expect("serialize telemetry");
        let TelemetryEventKind::ToolUse {
            client_id,
            tool_name,
            tool_use_id,
            tool_input,
        } = &event;
        assert_eq!(client_id, "claude-code");
        assert_eq!(tool_name, "Bash");
        assert_eq!(tool_use_id.as_deref(), Some("tool-1"));
        assert_eq!(tool_input["command"], "cargo test");
        assert!(!serialized.contains("session-secret"));
        assert!(!serialized.contains("transcript"));
    }
}
