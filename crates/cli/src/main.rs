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
#[command(about = "Client for the Agentdesktop daemon")]
struct Args {
    /// Unix socket exposed by the local Agentdesktop daemon.
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
    /// Report a Claude Code SessionStart event as telemetry.
    ClaudeSessionStart,
    /// Report a Claude Code PreToolUse event as telemetry.
    ClaudePreToolUse {
        /// Include tool input in the event.
        #[arg(long)]
        include_input: bool,
    },
}

#[derive(Deserialize)]
struct ClaudePreToolUseInput {
    hook_event_name: Option<String>,
    tool_name: String,
    tool_use_id: Option<String>,
    tool_input: serde_json::Value,
}

#[derive(Deserialize)]
struct ClaudeSessionStartInput {
    hook_event_name: Option<String>,
    session_id: String,
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
            hook: HookCommand::ClaudePreToolUse { include_input },
        } => {
            // Telemetry is deliberately fail-open: an unavailable daemon must
            // never prevent Claude Code from using a tool.
            let _ = report_claude_pre_tool_use(&args.socket, include_input).await;
        }
        Command::Hook {
            hook: HookCommand::ClaudeSessionStart,
        } => {
            // Telemetry is deliberately fail-open: an unavailable daemon must
            // never prevent Claude Code from starting a session.
            let _ = report_claude_session_start(&args.socket).await;
        }
    }

    Ok(())
}

async fn report_claude_pre_tool_use(
    socket: &std::path::Path,
    include_input: bool,
) -> anyhow::Result<()> {
    let input = read_hook_input()?;
    let event = parse_claude_pre_tool_use(&input, include_input)?;
    client::post_json(socket, "/v1/telemetry", &event).await
}

async fn report_claude_session_start(socket: &std::path::Path) -> anyhow::Result<()> {
    let input = read_hook_input()?;
    let input: ClaudeSessionStartInput = serde_json::from_slice(&input)?;
    if input
        .hook_event_name
        .as_deref()
        .is_some_and(|name| name != "SessionStart")
    {
        anyhow::bail!("expected a Claude Code SessionStart event");
    }
    if input.session_id.trim().is_empty() {
        anyhow::bail!("Claude hook event has no session ID");
    }
    client::post_json(
        socket,
        "/v1/telemetry",
        &TelemetryEventKind::SessionNew {
            client_id: claude_client_id(),
            session_id: input.session_id,
        },
    )
    .await
}

fn read_hook_input() -> anyhow::Result<Vec<u8>> {
    let mut input = Vec::new();
    std::io::stdin()
        .take(MAX_HOOK_INPUT_BYTES + 1)
        .read_to_end(&mut input)?;
    if input.len() as u64 > MAX_HOOK_INPUT_BYTES {
        anyhow::bail!("Claude hook input exceeds {MAX_HOOK_INPUT_BYTES} bytes");
    }
    Ok(input)
}

fn parse_claude_pre_tool_use(
    input: &[u8],
    include_input: bool,
) -> anyhow::Result<TelemetryEventKind> {
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
        client_id: claude_client_id(),
        tool_name: input.tool_name,
        tool_use_id: input.tool_use_id.filter(|id| !id.is_empty()),
        tool_input: include_input.then_some(input.tool_input),
    })
}

fn claude_client_id() -> String {
    claude_client_id_for_entrypoint(std::env::var("CLAUDE_CODE_ENTRYPOINT").ok().as_deref())
        .to_owned()
}

fn claude_client_id_for_entrypoint(entrypoint: Option<&str>) -> &'static str {
    match entrypoint {
        Some("claude-desktop" | "claude-desktop-3p") => "claude-desktop",
        _ => "claude-code",
    }
}

#[cfg(test)]
mod tests {
    use agentdesktop_core::model::TelemetryEventKind;

    use super::{claude_client_id_for_entrypoint, parse_claude_pre_tool_use};

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
            true,
        )
        .expect("valid Claude hook input");

        let serialized = serde_json::to_string(&event).expect("serialize telemetry");
        let TelemetryEventKind::ToolUse {
            client_id,
            tool_name,
            tool_use_id,
            tool_input,
        } = &event
        else {
            panic!("expected tool-use telemetry");
        };
        assert_eq!(client_id, "claude-code");
        assert_eq!(tool_name, "Bash");
        assert_eq!(tool_use_id.as_deref(), Some("tool-1"));
        assert_eq!(tool_input.as_ref().unwrap()["command"], "cargo test");
        assert!(!serialized.contains("session-secret"));
        assert!(!serialized.contains("transcript"));
    }

    #[test]
    fn attributes_desktop_entrypoints_to_claude_desktop() {
        assert_eq!(
            claude_client_id_for_entrypoint(Some("claude-desktop")),
            "claude-desktop"
        );
        assert_eq!(
            claude_client_id_for_entrypoint(Some("claude-desktop-3p")),
            "claude-desktop"
        );
        assert_eq!(claude_client_id_for_entrypoint(Some("cli")), "claude-code");
    }
}
