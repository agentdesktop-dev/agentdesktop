use std::path::PathBuf;

use agentgateway_edge_connector::identity::storage::{
    CredentialStorageMode, CredentialStore, default_storage_root,
};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about = "Configure managed identity for the edge connector")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Validate credential storage and persist the selected backend.
    StorageCheck {
        #[arg(
            long,
            env = "AGENTGATEWAY_EDGE_CREDENTIAL_STORAGE",
            value_enum,
            default_value = "auto"
        )]
        credential_storage: CredentialStorageMode,

        #[arg(long, env = "AGENTGATEWAY_EDGE_IDENTITY_DIR")]
        storage_dir: Option<PathBuf>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::StorageCheck {
            credential_storage,
            storage_dir,
        } => {
            let storage_dir = storage_dir.map_or_else(default_storage_root, Ok)?;
            let store = CredentialStore::setup(credential_storage, &storage_dir)?;
            println!(
                "credential storage is ready: {} ({})",
                store.backend_name(),
                storage_dir.display()
            );
        }
    }
    Ok(())
}
