use std::path::PathBuf;

use agentdesktop_agent::{
    cli::{self, ClientCommand},
    daemon::{self, DaemonArgs},
};
use agentdesktop_core::{DEFAULT_SOCKET_PATH, VERSION, client_socket_path};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "Agent Desktop daemon and command-line tools", version = VERSION)]
struct Args {
    /// Override the local endpoint exposed by the daemon (Unix socket or Windows named pipe).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

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
    // Inside a sandboxed launch, git starts this executable as the
    // interpreter of its per-launch ssh launcher script; that call only runs
    // ssh (Windows only, never a CLI command line).
    if let Some(code) = agentdesktop_agent::launch::git_ssh::run_if_invoked() {
        std::process::exit(code);
    }
    let args = Args::parse();
    match args.command {
        Command::Daemon(daemon) => {
            let socket = args.socket.unwrap_or_else(|| DEFAULT_SOCKET_PATH.into());
            daemon::run(*daemon, socket).await
        }
        Command::Client(command) => {
            let socket = args.socket.unwrap_or_else(client_socket_path);
            cli::run(command, socket).await
        }
    }
}
