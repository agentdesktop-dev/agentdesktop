use std::path::PathBuf;

use agentdesktop_client as client;
use agentdesktop_core::{
    DEFAULT_SOCKET_PATH,
    config::Config,
    model::{Discovery, Health, InferenceGatewayCredential},
};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Client for the AgentDesktop daemon")]
struct Args {
    #[arg(long, default_value = DEFAULT_SOCKET_PATH, global = true)]
    socket: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Check whether the daemon is reachable.
    Status,
    /// Discover locally installed agents.
    Discover,
    /// Print the daemon's active configuration.
    Config,
    /// Print a short-lived credential for an inference gateway.
    Credential {
        /// Name of the inference gateway in desired configuration.
        gateway: String,
    },
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
            let config: Config = client::get(&args.socket, "/v1/config").await?;
            print!(
                "{}",
                agentdesktop_core::serdes::yamlviajson::to_string(&config)?
            );
        }
        Command::Credential { gateway } => {
            if gateway.is_empty()
                || !gateway
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
            {
                anyhow::bail!("gateway name must contain only letters, numbers, '.', '-', or '_'");
            }
            let path = format!("/v1/inference-gateways/{gateway}/credential");
            let response: InferenceGatewayCredential = client::get(&args.socket, &path).await?;
            println!("{}", response.credential);
        }
    }

    Ok(())
}
