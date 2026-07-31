use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use url::Url;

use super::enrollment::{
    EnrollmentClient, delete_enrollment_for, load_enrollment_for, save_enrollment_for,
};
use super::oauth::{LoginConfig, ManagedIdentity, delete_session_for, load_session_for, login};
use super::storage::{self, CredentialStorageMode, CredentialStore, default_storage_root};

#[derive(Debug, Parser)]
#[command(version, about = "Configure managed identity for the edge connector")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
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
    /// Authenticate in the system browser using Authorization Code with PKCE.
    Login {
        #[arg(long)]
        issuer: Url,
        #[arg(long)]
        client_id: String,
        #[arg(long)]
        audience: String,
        #[arg(long)]
        scope: String,
        #[arg(long)]
        gateway_origin: Url,
        #[arg(
            long,
            env = "AGENTGATEWAY_EDGE_CREDENTIAL_STORAGE",
            value_enum,
            default_value = "auto"
        )]
        credential_storage: CredentialStorageMode,
        #[arg(long, env = "AGENTGATEWAY_EDGE_IDENTITY_DIR")]
        storage_dir: Option<PathBuf>,
        /// Print the authorization URL instead of opening a browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Delete a persisted managed identity session.
    Logout {
        #[arg(long)]
        issuer: Url,
        #[arg(long)]
        gateway_origin: Url,
        #[arg(long, env = "AGENTGATEWAY_EDGE_IDENTITY_DIR")]
        storage_dir: Option<PathBuf>,
    },
    /// Request approval for the current session's DPoP key.
    EnrollRequest {
        #[arg(long)]
        issuer: Url,
        #[arg(long)]
        gateway_origin: Url,
        #[arg(long, env = "AGENTGATEWAY_EDGE_IDENTITY_DIR")]
        storage_dir: Option<PathBuf>,
    },
    /// Read an existing enrollment and device revocation status.
    EnrollStatus {
        #[arg(long)]
        issuer: Url,
        #[arg(long)]
        gateway_origin: Url,
        #[arg(long)]
        enrollment_id: Option<String>,
        #[arg(long, env = "AGENTGATEWAY_EDGE_IDENTITY_DIR")]
        storage_dir: Option<PathBuf>,
    },
}

pub async fn run_from(arguments: impl IntoIterator<Item = OsString>) -> anyhow::Result<()> {
    match Cli::parse_from(arguments).command {
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
        Command::Login {
            issuer,
            client_id,
            audience,
            scope,
            gateway_origin,
            credential_storage,
            storage_dir,
            no_open,
        } => {
            let storage_dir = storage_dir.map_or_else(storage::default_storage_root, Ok)?;
            let store = CredentialStore::setup(credential_storage, &storage_dir)?;
            let session = login(
                &LoginConfig {
                    issuer,
                    client_id,
                    audience,
                    scope,
                    gateway_origin,
                },
                &store,
                |authorization_url| {
                    if no_open {
                        println!("authorization URL: {authorization_url}");
                        Ok(())
                    } else {
                        open::that(authorization_url.as_str()).map_err(Into::into)
                    }
                },
            )
            .await?;
            println!(
                "managed login complete for {} using {} storage",
                session.issuer,
                store.backend_name()
            );
        }
        Command::Logout {
            issuer,
            gateway_origin,
            storage_dir,
        } => {
            let storage_dir = storage_dir.map_or_else(storage::default_storage_root, Ok)?;
            let store = CredentialStore::load(&storage_dir)?;
            delete_session_for(&issuer, &gateway_origin, &store)?;
            delete_enrollment_for(&issuer, &gateway_origin, &store)?;
            println!("managed identity session deleted");
        }
        Command::EnrollRequest {
            issuer,
            gateway_origin,
            storage_dir,
        } => {
            let (identity, store) = load_identity(&issuer, &gateway_origin, storage_dir)?;
            let client = EnrollmentClient::discover(&issuer).await?;
            let enrollment = client.request(&identity).await?;
            save_enrollment_for(&issuer, &gateway_origin, &store, &enrollment)?;
            println!("{}", serde_json::to_string_pretty(&enrollment)?);
        }
        Command::EnrollStatus {
            issuer,
            gateway_origin,
            enrollment_id,
            storage_dir,
        } => {
            let (identity, store) = load_identity(&issuer, &gateway_origin, storage_dir)?;
            let enrollment_id = match enrollment_id {
                Some(enrollment_id) => enrollment_id,
                None => {
                    load_enrollment_for(
                        &issuer,
                        &gateway_origin,
                        &identity.dpop_thumbprint().await?,
                        &store,
                    )?
                    .enrollment_id
                }
            };
            let client = EnrollmentClient::discover(&issuer).await?;
            let enrollment = client.status(&identity, &enrollment_id).await?;
            save_enrollment_for(&issuer, &gateway_origin, &store, &enrollment)?;
            println!("{}", serde_json::to_string_pretty(&enrollment)?);
        }
    }
    Ok(())
}

fn load_identity(
    issuer: &Url,
    gateway_origin: &Url,
    storage_dir: Option<PathBuf>,
) -> anyhow::Result<(ManagedIdentity, CredentialStore)> {
    let storage_dir = storage_dir.map_or_else(storage::default_storage_root, Ok)?;
    let store = CredentialStore::load(&storage_dir)?;
    let session = load_session_for(issuer, gateway_origin, &store)?;
    Ok((ManagedIdentity::new(session, store.clone()), store))
}
