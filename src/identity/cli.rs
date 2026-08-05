use std::path::PathBuf;

use clap::Subcommand;
use url::Url;

use super::enrollment::{
    EnrollmentClient, delete_enrollment_for, load_enrollment_for, save_enrollment_for,
};
use super::oauth::{LoginConfig, ManagedIdentity, delete_session_for, load_session_for, login};
use super::storage::{self, CredentialStorageMode, CredentialStore, default_storage_root};

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
pub enum IdentityCommand {
    /// Validate credential storage and persist the selected backend.
    StorageCheck {
        #[arg(
            long,
            env = "AGENTDESKTOP_CREDENTIAL_STORAGE",
            value_enum,
            default_value = "auto"
        )]
        credential_storage: CredentialStorageMode,
        #[arg(long, env = "AGENTDESKTOP_IDENTITY_DIR")]
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
            env = "AGENTDESKTOP_CREDENTIAL_STORAGE",
            value_enum,
            default_value = "auto"
        )]
        credential_storage: CredentialStorageMode,
        #[arg(long, env = "AGENTDESKTOP_IDENTITY_DIR")]
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
        #[arg(long, env = "AGENTDESKTOP_IDENTITY_DIR")]
        storage_dir: Option<PathBuf>,
    },
    /// Generate a device key and request managed mTLS enrollment.
    EnrollRequest {
        #[arg(long)]
        issuer: Url,
        #[arg(long)]
        enrollment_url: Url,
        #[arg(long)]
        gateway_origin: Url,
        #[arg(long, env = "AGENTDESKTOP_IDENTITY_DIR")]
        storage_dir: Option<PathBuf>,
    },
    /// Read the current managed mTLS enrollment status.
    EnrollStatus {
        #[arg(long)]
        issuer: Url,
        #[arg(long)]
        enrollment_url: Url,
        #[arg(long)]
        gateway_origin: Url,
        #[arg(long, env = "AGENTDESKTOP_IDENTITY_DIR")]
        storage_dir: Option<PathBuf>,
    },
}

pub async fn run(command: IdentityCommand) -> anyhow::Result<()> {
    match command {
        IdentityCommand::StorageCheck {
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
        IdentityCommand::Login {
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
        IdentityCommand::Logout {
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
        IdentityCommand::EnrollRequest {
            issuer,
            enrollment_url,
            gateway_origin,
            storage_dir,
        } => {
            let (identity, store) = load_identity(&issuer, &gateway_origin, storage_dir)?;
            let client = EnrollmentClient::new(&enrollment_url)?;
            let enrollment = client.request(&identity).await?;
            save_enrollment_for(&issuer, &gateway_origin, &store, &enrollment)?;
            println!(
                "enrollment {} is {:?}",
                enrollment.enrollment_id, enrollment.status
            );
        }
        IdentityCommand::EnrollStatus {
            issuer,
            enrollment_url,
            gateway_origin,
            storage_dir,
        } => {
            let (identity, store) = load_identity(&issuer, &gateway_origin, storage_dir)?;
            let current = load_enrollment_for(&issuer, &gateway_origin, &store)?;
            let client = EnrollmentClient::new(&enrollment_url)?;
            let enrollment = client.status(&identity, &current).await?;
            save_enrollment_for(&issuer, &gateway_origin, &store, &enrollment)?;
            println!(
                "enrollment {} is {:?}",
                enrollment.enrollment_id, enrollment.status
            );
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
