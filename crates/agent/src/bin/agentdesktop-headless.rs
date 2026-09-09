use std::path::PathBuf;

use agentdesktop_agent::{
    cli::{self, ClientCommand},
    daemon::{self, DaemonArgs},
};
use agentdesktop_core::{DEFAULT_SOCKET_PATH, VERSION};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Agent Desktop daemon and command-line tools", version = VERSION)]
struct Args {
    /// Local daemon endpoint (Unix socket or Windows named pipe).
    #[arg(long, global = true, default_value = DEFAULT_SOCKET_PATH)]
    socket: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the device daemon.
    Daemon(Box<DaemonArgs>),

    #[command(flatten)]
    Client(ClientCommand),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Daemon(daemon) => daemon::run(*daemon, args.socket).await,
        Command::Client(command) => cli::run(command, args.socket).await,
    }
}
