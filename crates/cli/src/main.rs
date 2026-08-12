use std::path::PathBuf;

use agentdesktop_client as client;
use agentdesktop_core::{
    DEFAULT_SOCKET_PATH,
    config::DaemonConfig,
    model::{Discovery, Health, InferenceGatewayCredential},
};
use clap::{Parser, Subcommand};

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
    }

    Ok(())
}
