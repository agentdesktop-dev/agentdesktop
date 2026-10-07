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

/// Marks a [`load`] failure where identity metadata exists but the identity
/// itself cannot be used: the metadata is corrupt, or a secret is missing or
/// unreadable. On macOS the usual cause is a change of the app's code
/// signature: Keychain items stay bound to the old signature, and the daemon
/// can no longer read them. Waiting or retrying does not help, so callers
/// should [`discard`] the identity and enroll again.
#[derive(Debug)]
pub struct IdentityUnreadable;

impl std::fmt::Display for IdentityUnreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stored device identity is unreadable")
    }
}

pub fn is_unreadable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<IdentityUnreadable>().is_some()
}

/// The enrolled device ID alone, without opening the secret store. Used where
/// only the identity's *name* matters (cache tagging), so a keyring that is
/// locked or unavailable does not turn a lookup into a failure.
pub fn load_device_id(path: &Path) -> anyhow::Result<Option<String>> {
    match fs::read(path) {
        Ok(contents) => {
            let stored: StoredIdentity =
                serde_json::from_slice(&contents).context("parse device identity")?;
            Ok(Some(stored.device_id))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read identity from {}", path.display())),
    }
}

pub fn load(path: &Path) -> anyhow::Result<Option<Identity>> {
    let stored: StoredIdentity = match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .context("parse device identity")
            .context(IdentityUnreadable)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read identity from {}", path.display()));
        }
    };
    let secrets = secret_store(path)?;
    #[cfg(target_os = "linux")]
    migrate_legacy_linux_secrets(path, &secrets, &stored.device_id)?;
    let (client_private_key_pem, oauth) =
        read_secrets(&secrets, &stored.device_id).context(IdentityUnreadable)?;
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

fn read_secrets(
    secrets: &SecretStore,
    device_id: &str,
) -> anyhow::Result<(String, OAuthCredentials)> {
    let client_private_key_pem = secrets
        .get(TLS_KEY_SERVICE, device_id)
        .context("read device TLS private key")?;
    let oauth = secrets
        .get(OAUTH_SERVICE, device_id)
        .context("read OAuth credentials")?;
    let oauth = serde_json::from_str(&oauth).context("decode stored OAuth credentials")?;
    Ok((client_private_key_pem, oauth))
}

pub fn save(path: &Path, identity: &Identity) -> anyhow::Result<()> {
    let parent = path.parent().context("identity path has no parent")?;
    secure_fs::ensure_private_dir(parent)?;
    let secrets = SecretStore::new(parent)?;
    let oauth = serde_json::to_string(&identity.oauth)?;
    for (service, secret, operation) in [
        (
            TLS_KEY_SERVICE,
            identity.client_private_key_pem.as_str(),
            "store device TLS private key",
        ),
        (OAUTH_SERVICE, oauth.as_str(), "store OAuth credentials"),
    ] {
        // The macOS Keychain adds a new item whenever looking up the existing
        // one fails, including when the item is there but unreadable (after a
        // code-signature change), and leaves the unreadable copy behind as a
        // duplicate. Remove such an item first. Readable items are updated in
        // place, so a healthy identity is never deleted. If the item cannot be
        // removed, fail rather than store a duplicate next to it.
        if secrets.get_optional(service, &identity.device_id).is_err() {
            secrets
                .delete(service, &identity.device_id)
                .with_context(|| format!("remove unreadable item before: {operation}"))?;
        }
        secrets
            .set(service, &identity.device_id, secret)
            .context(operation)?;
    }
    write_metadata(
        path,
        &identity.device_id,
        &identity.client_certificate_pem,
        identity.client_certificate_expires_at_unix_seconds,
        &identity.oauth_token_endpoint,
        &identity.oauth_client_id,
    )
}

/// Removes the identity's secrets and metadata.
///
/// Only failing to remove the metadata is an error. Once it is gone the device
/// is unenrolled and nothing reads the old secrets again, so a secret that
/// cannot be removed (for example a Keychain item the app can no longer
/// access after a code-signature change) is logged rather than leaving the
/// device stuck with an identity it cannot use or sign out of.
pub fn delete(path: &Path, device_id: &str) -> anyhow::Result<()> {
    let secrets_result = delete_secrets(path, device_id);
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("remove device identity {}", path.display()));
        }
    }
    if let Err(error) = secrets_result {
        tracing::warn!(
            device_id,
            error = %format!("{error:#}"),
            "removed device identity, but some of its secrets could not be removed"
        );
    }
    Ok(())
}

/// Removes an identity that [`load`] reported as [unreadable](is_unreadable).
/// Its secrets are removed too when the metadata still names the device.
pub fn discard(path: &Path) -> anyhow::Result<()> {
    let device_id = fs::read(path)
        .ok()
        .and_then(|contents| serde_json::from_slice::<StoredIdentity>(&contents).ok())
        .map(|stored| stored.device_id);
    match device_id {
        Some(device_id) => delete(path, &device_id),
        None => match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => {
                Err(error).with_context(|| format!("remove device identity {}", path.display()))
            }
        },
    }
}

fn delete_secrets(path: &Path, device_id: &str) -> anyhow::Result<()> {
    let secrets = secret_store(path)?;
    let tls_key = secrets
        .delete(TLS_KEY_SERVICE, device_id)
        .context("delete device TLS private key");
    let oauth = secrets
        .delete(OAUTH_SERVICE, device_id)
        .context("delete OAuth credentials");
    #[cfg(target_os = "linux")]
    delete_legacy_linux_secrets(path)?;
    tls_key.and(oauth)
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
    // Legacy secrets that cannot be read leave the identity unusable, just like
    // an unreadable secret in the store. Failing to migrate them is not.
    let contents = match fs::read(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read legacy device secrets from {}", path.display()))
                .context(IdentityUnreadable);
        }
    };
    let secrets: StoredSecrets = serde_json::from_slice(&contents)
        .context("parse legacy stored device secrets")
        .context(IdentityUnreadable)?;
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

    use super::{Identity, OAuthCredentials, delete, discard, is_unreadable, load, save};

    fn temp_directory() -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "agentdesktop-identity-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir(&directory).unwrap();
        directory
    }

    fn test_identity() -> Identity {
        Identity {
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
        }
    }

    fn secret_count(directory: &std::path::Path) -> usize {
        fs::read_dir(directory.join("secrets")).unwrap().count()
    }

    #[test]
    fn linux_secrets_round_trip_outside_identity_metadata() {
        let directory = temp_directory();
        let path = directory.join("identity.json");
        let identity = test_identity();

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

    #[test]
    fn missing_secret_is_unreadable_and_discard_unenrolls() {
        let directory = temp_directory();
        let path = directory.join("identity.json");
        save(&path, &test_identity()).unwrap();
        let secret = fs::read_dir(directory.join("secrets"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::remove_file(secret).unwrap();

        let error = load(&path).unwrap_err();
        assert!(is_unreadable(&error), "{error:#}");

        discard(&path).unwrap();
        assert!(!path.exists());
        assert_eq!(secret_count(&directory), 0);
        assert!(load(&path).unwrap().is_none());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn corrupt_metadata_is_unreadable_and_discard_unenrolls() {
        let directory = temp_directory();
        let path = directory.join("identity.json");
        fs::write(&path, b"not json").unwrap();

        let error = load(&path).unwrap_err();
        assert!(is_unreadable(&error), "{error:#}");

        discard(&path).unwrap();
        assert!(!path.exists());
        assert!(load(&path).unwrap().is_none());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn corrupt_legacy_secrets_are_unreadable_and_discard_unenrolls() {
        let directory = temp_directory();
        let path = directory.join("identity.json");
        save(&path, &test_identity()).unwrap();
        let legacy = directory.join("identity-secrets.json");
        fs::write(&legacy, b"not json").unwrap();

        let error = load(&path).unwrap_err();
        assert!(is_unreadable(&error), "{error:#}");

        discard(&path).unwrap();
        assert!(!path.exists());
        assert!(!legacy.exists());
        assert_eq!(secret_count(&directory), 0);
        assert!(load(&path).unwrap().is_none());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn save_replaces_an_unreadable_secret() {
        let directory = temp_directory();
        let path = directory.join("identity.json");
        let identity = test_identity();
        save(&path, &identity).unwrap();
        for entry in fs::read_dir(directory.join("secrets")).unwrap() {
            fs::write(entry.unwrap().path(), [0xff, 0xfe]).unwrap();
        }
        assert!(is_unreadable(&load(&path).unwrap_err()));

        save(&path, &identity).unwrap();

        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.client_private_key_pem, "private-key");
        assert_eq!(loaded.oauth.refresh_token, "refresh-token");
        assert_eq!(secret_count(&directory), 2);
        fs::remove_dir_all(directory).unwrap();
    }
}
