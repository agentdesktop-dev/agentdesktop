use std::process::{ExitCode, ExitStatus};

use clap::{Parser, Subcommand};

use crate::config::Config;
use crate::identity::cli::IdentityCommand;

#[derive(Debug, Parser)]
#[command(version, about = "Route AI application traffic through Agent Gateway")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the application-facing forwarding service.
    Serve(Config),
    /// Connect supported AI agents to the installed service.
    ConnectAgents {
        #[arg(long, help = "Connect supported agents without prompting")]
        yes: bool,
    },
    /// Configure managed identity for Agent Desktop.
    Identity {
        #[command(subcommand)]
        command: IdentityCommand,
    },
    /// Relay redirected Linux TCP flows over HBONE.
    #[cfg(target_os = "linux")]
    Capture(crate::capture::CaptureArgs),
    /// Run a command tree in an Agent Desktop execution scope.
    #[cfg(target_os = "linux")]
    Launch(crate::launch::LaunchArgs),
    /// Install or remove trust for local Agent Gateway inspection.
    #[cfg(target_os = "linux")]
    Trust {
        #[arg(value_enum)]
        action: crate::trust::Action,
    },
    #[cfg(target_os = "linux")]
    #[command(name = "_launch-child", hide = true)]
    LaunchChild(crate::launch::LaunchChildArgs),
}

pub async fn run() -> anyhow::Result<ExitCode> {
    let status = match Cli::parse().command {
        Command::Serve(config) => {
            crate::service::run(config.validate()?).await?;
            None
        }
        Command::ConnectAgents { yes } => {
            crate::connection::run(yes).await?;
            None
        }
        Command::Identity { command } => {
            crate::identity::cli::run(command).await?;
            None
        }
        #[cfg(target_os = "linux")]
        Command::Capture(args) => {
            let _telemetry = crate::telemetry::init()?;
            crate::capture::run(args).await?;
            None
        }
        #[cfg(target_os = "linux")]
        Command::Launch(args) => Some(crate::launch::run(args)?),
        #[cfg(target_os = "linux")]
        Command::Trust { action } => {
            crate::trust::run(action)?;
            None
        }
        #[cfg(target_os = "linux")]
        Command::LaunchChild(args) => {
            crate::launch::run_child(args)?;
            None
        }
    };
    Ok(status.map_or(ExitCode::SUCCESS, exit_code))
}

fn exit_code(status: ExitStatus) -> ExitCode {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from)
}
