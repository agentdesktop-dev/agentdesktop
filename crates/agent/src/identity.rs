use std::{fs, path::Path};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::{secret_store::SecretStore, secure_fs};

const TLS_KEY_SERVICE: &str = "dev.agentdesktop.device-tls-key";
const OAUTH_SERVICE: &str = "dev.agentdesktop.device-oauth";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OAuthCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_unix_seconds: u64,
}

#[derive(Clone, Debug)]
pub struct Identity {
    pub device_id: String,
    pub client_certificate_pem: String,
    pub client_private_key_pem: String,
    pub client_certificate_expires_at_unix_seconds: u64,
    pub oauth: OAuthCredentials,
    pub oauth_token_endpoint: String,
    pub oauth_client_id: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredIdentity {
    device_id: String,
    client_certificate_pem: String,
    client_certificate_expires_at_unix_seconds: u64,
    oauth_token_endpoint: String,
    oauth_client_id: String,
}

#[cfg(target_os = "linux")]
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredSecrets {
    client_private_key_pem: String,
    oauth: OAuthCredentials,
}

pub fn load(path: &Path) -> anyhow::Result<Option<Identity>> {
    let stored: StoredIdentity = match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents).context("parse device identity")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read identity from {}", path.display()));
        }
    };
    let secrets = secret_store(path)?;
    #[cfg(target_os = "linux")]
    migrate_legacy_linux_secrets(path, &secrets, &stored.device_id)?;
    let client_private_key_pem = secrets
        .get(TLS_KEY_SERVICE, &stored.device_id)
        .context("read device TLS private key")?;
    let oauth = secrets
        .get(OAUTH_SERVICE, &stored.device_id)
        .context("read OAuth credentials")?;
    let oauth = serde_json::from_str(&oauth).context("decode stored OAuth credentials")?;
    Ok(Some(Identity {
        device_id: stored.device_id,
        client_certificate_pem: stored.client_certificate_pem,
        client_private_key_pem,
        client_certificate_expires_at_unix_seconds: stored
            .client_certificate_expires_at_unix_seconds,
        oauth,
        oauth_token_endpoint: stored.oauth_token_endpoint,
        oauth_client_id: stored.oauth_client_id,
    }))
}

pub fn save(path: &Path, identity: &Identity) -> anyhow::Result<()> {
    let parent = path.parent().context("identity path has no parent")?;
    secure_fs::ensure_private_dir(parent)?;
    let secrets = SecretStore::new(parent)?;
    secrets
        .set(
            TLS_KEY_SERVICE,
            &identity.device_id,
            &identity.client_private_key_pem,
        )
        .context("store device TLS private key")?;
    secrets
        .set(
            OAUTH_SERVICE,
            &identity.device_id,
            &serde_json::to_string(&identity.oauth)?,
        )
        .context("store OAuth credentials")?;
    write_metadata(
        path,
        &identity.device_id,
        &identity.client_certificate_pem,
        identity.client_certificate_expires_at_unix_seconds,
        &identity.oauth_token_endpoint,
        &identity.oauth_client_id,
    )
}

pub fn delete(path: &Path, device_id: &str) -> anyhow::Result<()> {
    let secrets = secret_store(path)?;
    secrets
        .delete(TLS_KEY_SERVICE, device_id)
        .context("delete device TLS private key")?;
    secrets
        .delete(OAUTH_SERVICE, device_id)
        .context("delete OAuth credentials")?;
    #[cfg(target_os = "linux")]
    delete_legacy_linux_secrets(path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove rejected identity {}", path.display()))
        }
    }
}

fn secret_store(identity_path: &Path) -> anyhow::Result<SecretStore> {
    let parent = identity_path
        .parent()
        .context("identity path has no parent")?;
    SecretStore::new(parent)
}

#[cfg(target_os = "linux")]
fn migrate_legacy_linux_secrets(
    identity_path: &Path,
    store: &SecretStore,
    device_id: &str,
) -> anyhow::Result<()> {
    let path = legacy_linux_secrets_path(identity_path)?;
    let contents = match fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read legacy device secrets from {}", path.display()));
        }
    };
    let secrets: StoredSecrets =
        serde_json::from_slice(&contents).context("parse legacy stored device secrets")?;
    store.set(TLS_KEY_SERVICE, device_id, &secrets.client_private_key_pem)?;
    store.set(
        OAUTH_SERVICE,
        device_id,
        &serde_json::to_string(&secrets.oauth)?,
    )?;
    fs::remove_file(&path)
        .with_context(|| format!("remove migrated device secrets {}", path.display()))
}

#[cfg(target_os = "linux")]
fn delete_legacy_linux_secrets(identity_path: &Path) -> anyhow::Result<()> {
    let path = legacy_linux_secrets_path(identity_path)?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove legacy device secrets {}", path.display()))
        }
    }
}

#[cfg(target_os = "linux")]
fn legacy_linux_secrets_path(identity_path: &Path) -> anyhow::Result<std::path::PathBuf> {
    let parent = identity_path
        .parent()
        .context("identity path has no parent")?;
    Ok(parent.join("identity-secrets.json"))
}

fn write_metadata(
    path: &Path,
    device_id: &str,
    client_certificate_pem: &str,
    client_certificate_expires_at_unix_seconds: u64,
    oauth_token_endpoint: &str,
    oauth_client_id: &str,
) -> anyhow::Result<()> {
    let parent = path.parent().context("identity path has no parent")?;
    secure_fs::ensure_private_dir(parent)?;
    let stored = StoredIdentity {
        device_id: device_id.to_owned(),
        client_certificate_pem: client_certificate_pem.to_owned(),
        client_certificate_expires_at_unix_seconds,
        oauth_token_endpoint: oauth_token_endpoint.to_owned(),
        oauth_client_id: oauth_client_id.to_owned(),
    };
    secure_fs::atomic_write(path, &serde_json::to_vec_pretty(&stored)?, 0o600)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::{Identity, OAuthCredentials, delete, load, save};

    #[test]
    fn linux_secrets_round_trip_outside_identity_metadata() {
        let directory = std::env::temp_dir().join(format!(
            "agentdesktop-identity-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("identity.json");
        let identity = Identity {
            device_id: "device-1".to_owned(),
            client_certificate_pem: "certificate".to_owned(),
            client_private_key_pem: "private-key".to_owned(),
            client_certificate_expires_at_unix_seconds: 123,
            oauth: OAuthCredentials {
                access_token: "access-token".to_owned(),
                refresh_token: "refresh-token".to_owned(),
                expires_at_unix_seconds: 456,
            },
            oauth_token_endpoint: "https://issuer.example/token".to_owned(),
            oauth_client_id: "client-1".to_owned(),
        };

        save(&path, &identity).unwrap();

        let metadata = fs::read_to_string(&path).unwrap();
        assert!(!metadata.contains("private-key"));
        assert!(!metadata.contains("access-token"));
        assert!(!metadata.contains("refresh-token"));
        let secret_paths: Vec<_> = fs::read_dir(directory.join("secrets"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(secret_paths.len(), 2);
        assert!(
            secret_paths
                .iter()
                .all(|path| { fs::metadata(path).unwrap().permissions().mode() & 0o777 == 0o600 })
        );

        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.client_private_key_pem, "private-key");
        assert_eq!(loaded.oauth.access_token, "access-token");
        assert_eq!(loaded.oauth.refresh_token, "refresh-token");

        delete(&path, "device-1").unwrap();
        assert!(!path.exists());
        assert!(secret_paths.iter().all(|path| !path.exists()));
        fs::remove_dir_all(directory).unwrap();
    }
}
