use std::path::PathBuf;

use agentdesktop_agent::{
    cli::{self, ClientCommand},
    daemon::{self, DaemonArgs},
};
use agentdesktop_core::DEFAULT_SOCKET_PATH;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Manage the local Agentdesktop daemon")]
struct Args {
    /// Local endpoint exposed by the daemon (Unix socket or Windows named pipe).
    #[arg(long, default_value = DEFAULT_SOCKET_PATH, global = true)]
    socket: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the privileged device daemon.
    Daemon(DaemonArgs),

    #[command(flatten)]
    Client(ClientCommand),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Daemon(daemon_args) => daemon::run(daemon_args, args.socket).await,
        Command::Client(command) => cli::run(command, args.socket).await,
    }
}
