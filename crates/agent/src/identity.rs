use std::{fs, path::Path};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use crate::session::OAuthCredentials;
use crate::{secret_store::SecretStore, secure_fs, session};

const TLS_KEY_SERVICE: &str = "dev.agentdesktop.device-tls-key";

/// How the device enrolled. It decides what the OIDC session of the user
/// signed in on the device is for (see [`session`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Enrollment {
    /// A user signed in to enroll the device, which is theirs: every
    /// controller call carries their access token, and the enrollment ends
    /// with their session.
    UserSignIn,
    /// The device enrolled with an enrollment token, and its certificate
    /// alone authenticates it. Users sign in and out on it afterwards.
    EnrollmentToken,
}

impl Enrollment {
    /// Whether the device is the signed-in user's: its controller calls all
    /// carry the user's access token, and ending the user's session removes
    /// the device identity too.
    pub fn belongs_to_user(self) -> bool {
        self == Self::UserSignIn
    }
}

#[derive(Clone, Debug)]
pub struct Identity {
    pub device_id: String,
    pub client_certificate_pem: String,
    pub client_private_key_pem: String,
    pub client_certificate_expires_at_unix_seconds: u64,
    pub enrollment: Enrollment,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredIdentity {
    device_id: String,
    client_certificate_pem: String,
    client_certificate_expires_at_unix_seconds: u64,
    /// Missing from an identity stored before the user's OIDC session had a
    /// store of its own.
    #[serde(default)]
    enrollment: Option<Enrollment>,
    /// Where such an identity kept its session's token endpoint and client
    /// ID. Empty for a device enrolled with an enrollment token.
    #[serde(default, skip_serializing)]
    oauth_token_endpoint: String,
    #[serde(default, skip_serializing)]
    oauth_client_id: String,
}

impl StoredIdentity {
    fn enrollment(&self) -> Enrollment {
        self.enrollment
            .unwrap_or(if self.oauth_client_id.is_empty() {
                Enrollment::EnrollmentToken
            } else {
                Enrollment::UserSignIn
            })
    }
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
    Ok(load_metadata(path)?.map(|stored| stored.device_id))
}

/// The enrolled device ID and how the device enrolled. Like
/// [`load_device_id`], this does not open the secret store.
pub fn load_enrollment(path: &Path) -> anyhow::Result<Option<(String, Enrollment)>> {
    Ok(load_metadata(path)?.map(|stored| {
        let enrollment = stored.enrollment();
        (stored.device_id, enrollment)
    }))
}

fn load_metadata(path: &Path) -> anyhow::Result<Option<StoredIdentity>> {
    match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents)
            .context("parse device identity")
            .map(Some),
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
    let client_private_key_pem = secrets
        .get(TLS_KEY_SERVICE, &stored.device_id)
        .context("read device TLS private key")
        .context(IdentityUnreadable)?;
    let enrollment = stored.enrollment();
    let identity = Identity {
        device_id: stored.device_id,
        client_certificate_pem: stored.client_certificate_pem,
        client_private_key_pem,
        client_certificate_expires_at_unix_seconds: stored
            .client_certificate_expires_at_unix_seconds,
        enrollment,
    };
    if stored.enrollment.is_none() {
        // The session's secret is already where the session store keeps it.
        if enrollment == Enrollment::UserSignIn {
            let parent = path.parent().context("identity path has no parent")?;
            session::adopt_legacy(
                parent,
                &identity.device_id,
                &stored.oauth_token_endpoint,
                &stored.oauth_client_id,
            )?;
        }
        write_metadata(path, &identity)?;
    }
    Ok(Some(identity))
}

pub fn save(path: &Path, identity: &Identity) -> anyhow::Result<()> {
    let parent = path.parent().context("identity path has no parent")?;
    secure_fs::ensure_private_dir(parent)?;
    store_secret(
        &SecretStore::new(parent)?,
        TLS_KEY_SERVICE,
        &identity.device_id,
        &identity.client_private_key_pem,
        "store device TLS private key",
    )?;
    write_metadata(path, identity)
}

/// Stores `secret`, replacing an item that is there but cannot be read.
pub(crate) fn store_secret(
    secrets: &SecretStore,
    service: &str,
    account: &str,
    secret: &str,
    operation: &'static str,
) -> anyhow::Result<()> {
    // The macOS Keychain adds a new item whenever looking up the existing
    // one fails, including when the item is there but unreadable (after a
    // code-signature change), and leaves the unreadable copy behind as a
    // duplicate. Remove such an item first. Readable items are updated in
    // place, so a healthy identity is never deleted. If the item cannot be
    // removed, fail rather than store a duplicate next to it.
    if secrets.get_optional(service, account).is_err() {
        secrets
            .delete(service, account)
            .with_context(|| format!("remove unreadable item before: {operation}"))?;
    }
    secrets.set(service, account, secret).context(operation)
}

/// Removes the identity's secrets and metadata, and the session of the user
/// signed in on it.
///
/// Only failing to remove the metadata is an error. Once it is gone the device
/// is unenrolled and nothing reads the old secrets again, so a secret that
/// cannot be removed (for example a Keychain item the app can no longer
/// access after a code-signature change) is logged rather than leaving the
/// device stuck with an identity it cannot use or sign out of.
pub async fn delete(path: &Path, device_id: &str) -> anyhow::Result<()> {
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
    if let Some(state_dir) = path.parent()
        && let Err(error) = session::forget(state_dir, device_id).await
    {
        tracing::warn!(
            device_id,
            error = %format!("{error:#}"),
            "removed device identity, but the session of the user signed in on it could not be removed"
        );
    }
    Ok(())
}

/// Removes an identity that [`load`] reported as [unreadable](is_unreadable).
/// Its secrets are removed too when the metadata still names the device.
pub async fn discard(path: &Path) -> anyhow::Result<()> {
    let device_id = fs::read(path)
        .ok()
        .and_then(|contents| serde_json::from_slice::<StoredIdentity>(&contents).ok())
        .map(|stored| stored.device_id);
    match device_id {
        Some(device_id) => delete(path, &device_id).await,
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
    #[cfg(target_os = "linux")]
    delete_legacy_linux_secrets(path)?;
    tls_key
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
        session::OAUTH_SERVICE,
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

fn write_metadata(path: &Path, identity: &Identity) -> anyhow::Result<()> {
    let parent = path.parent().context("identity path has no parent")?;
    secure_fs::ensure_private_dir(parent)?;
    let stored = StoredIdentity {
        device_id: identity.device_id.clone(),
        client_certificate_pem: identity.client_certificate_pem.clone(),
        client_certificate_expires_at_unix_seconds: identity
            .client_certificate_expires_at_unix_seconds,
        enrollment: Some(identity.enrollment),
        oauth_token_endpoint: String::new(),
        oauth_client_id: String::new(),
    };
    secure_fs::atomic_write(path, &serde_json::to_vec_pretty(&stored)?, 0o600)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::{Enrollment, Identity, delete, discard, is_unreadable, load, save};

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
            enrollment: Enrollment::UserSignIn,
        }
    }

    fn secret_count(directory: &std::path::Path) -> usize {
        fs::read_dir(directory.join("secrets")).unwrap().count()
    }

    #[tokio::test]
    async fn linux_secrets_round_trip_outside_identity_metadata() {
        let directory = temp_directory();
        let path = directory.join("identity.json");
        let identity = test_identity();

        save(&path, &identity).unwrap();

        let metadata = fs::read_to_string(&path).unwrap();
        assert!(!metadata.contains("private-key"));
        let secret_paths: Vec<_> = fs::read_dir(directory.join("secrets"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(secret_paths.len(), 1);
        assert!(
            secret_paths
                .iter()
                .all(|path| { fs::metadata(path).unwrap().permissions().mode() & 0o777 == 0o600 })
        );

        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.client_private_key_pem, "private-key");
        assert_eq!(loaded.enrollment, Enrollment::UserSignIn);

        delete(&path, "device-1").await.unwrap();
        assert!(!path.exists());
        assert!(secret_paths.iter().all(|path| !path.exists()));
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn token_enrolled_identity_round_trips() {
        let directory = temp_directory();
        let path = directory.join("identity.json");
        let identity = Identity {
            enrollment: Enrollment::EnrollmentToken,
            ..test_identity()
        };

        save(&path, &identity).unwrap();
        assert_eq!(secret_count(&directory), 1);

        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.client_private_key_pem, "private-key");
        assert_eq!(loaded.enrollment, Enrollment::EnrollmentToken);

        delete(&path, "device-1").await.unwrap();
        assert_eq!(secret_count(&directory), 0);
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn an_identity_stored_with_its_session_moves_the_session_out() {
        let directory = temp_directory();
        let path = directory.join("identity.json");
        save(&path, &test_identity()).unwrap();
        // How an identity was stored while it held its user's OIDC session.
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "deviceId": "device-1",
                "clientCertificatePem": "certificate",
                "clientCertificateExpiresAtUnixSeconds": 123,
                "oauthTokenEndpoint": "https://issuer.example/token",
                "oauthClientId": "client-1",
            }))
            .unwrap(),
        )
        .unwrap();
        crate::secret_store::SecretStore::new(&directory)
            .unwrap()
            .set(
                crate::session::OAUTH_SERVICE,
                "device-1",
                r#"{"accessToken":"access-token","refreshToken":"refresh-token","expiresAtUnixSeconds":18446744073709551615}"#,
            )
            .unwrap();
        // A token-enrolled identity stored then had empty OAuth fields.
        let token_directory = temp_directory();
        let token_path = token_directory.join("identity.json");
        save(&token_path, &test_identity()).unwrap();
        fs::write(
            &token_path,
            br#"{"deviceId":"device-1","clientCertificatePem":"certificate","clientCertificateExpiresAtUnixSeconds":123,"oauthTokenEndpoint":"","oauthClientId":""}"#,
        )
        .unwrap();

        let loaded = load(&path).unwrap().unwrap();
        assert_eq!(loaded.enrollment, Enrollment::UserSignIn);
        let metadata = fs::read_to_string(&path).unwrap();
        assert!(!metadata.contains("oauth"), "{metadata}");
        let session = crate::session::current(&directory, "device-1", None)
            .await
            .unwrap()
            .expect("the session moves with the identity");
        assert_eq!(session.oauth.access_token, "access-token");
        assert_eq!(session.token_endpoint, "https://issuer.example/token");
        assert_eq!(session.client_id, "client-1");
        assert_eq!(
            load(&path).unwrap().unwrap().enrollment,
            Enrollment::UserSignIn
        );

        let loaded = load(&token_path).unwrap().unwrap();
        assert_eq!(loaded.enrollment, Enrollment::EnrollmentToken);
        assert!(!token_directory.join("session.json").exists());

        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(token_directory).unwrap();
    }

    #[tokio::test]
    async fn missing_secret_is_unreadable_and_discard_unenrolls() {
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

        discard(&path).await.unwrap();
        assert!(!path.exists());
        assert_eq!(secret_count(&directory), 0);
        assert!(load(&path).unwrap().is_none());
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn corrupt_metadata_is_unreadable_and_discard_unenrolls() {
        let directory = temp_directory();
        let path = directory.join("identity.json");
        fs::write(&path, b"not json").unwrap();

        let error = load(&path).unwrap_err();
        assert!(is_unreadable(&error), "{error:#}");

        discard(&path).await.unwrap();
        assert!(!path.exists());
        assert!(load(&path).unwrap().is_none());
        fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn corrupt_legacy_secrets_are_unreadable_and_discard_unenrolls() {
        let directory = temp_directory();
        let path = directory.join("identity.json");
        save(&path, &test_identity()).unwrap();
        let legacy = directory.join("identity-secrets.json");
        fs::write(&legacy, b"not json").unwrap();

        let error = load(&path).unwrap_err();
        assert!(is_unreadable(&error), "{error:#}");

        discard(&path).await.unwrap();
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
        assert_eq!(secret_count(&directory), 1);
        fs::remove_dir_all(directory).unwrap();
    }
}
