use std::path::PathBuf;

use agentdesktop_controller::{application, database::Database, management::Extensions};
use agentdesktop_core::{DEFAULT_CONTROLLER_CONFIG_PATH, config, telemetry};
use clap::Parser;

#[derive(Parser)]
#[command(about = "Agentdesktop fleet controller")]
struct Args {
    /// Path to the controller YAML configuration file.
    #[arg(long, default_value = DEFAULT_CONTROLLER_CONFIG_PATH)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let _log_flush = telemetry::setup_logging("info", false, None)?;
    let config = config::load_controller(&args.config)?;
    let database = Database::connect(&config.database_url).await?;
    application::run(config, database, Extensions::default()).await
}
