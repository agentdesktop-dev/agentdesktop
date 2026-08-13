use std::{fs, path::Path};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::secure_fs;

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

pub fn load(path: &Path) -> anyhow::Result<Option<Identity>> {
    let stored: StoredIdentity = match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents).context("parse device identity")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read identity from {}", path.display()));
        }
    };
    let client_private_key_pem = tls_key_entry(&stored.device_id)?
        .get_password()
        .context("read device TLS private key from operating system credential store")?;
    let oauth = oauth_entry(&stored.device_id)?
        .get_password()
        .context("read OAuth credentials from operating system credential store")?;
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
    tls_key_entry(&identity.device_id)?
        .set_password(&identity.client_private_key_pem)
        .context("store device TLS private key in operating system credential store")?;
    oauth_entry(&identity.device_id)?
        .set_password(&serde_json::to_string(&identity.oauth)?)
        .context("store OAuth credentials in operating system credential store")?;
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
    let key_entry = tls_key_entry(device_id)?;
    if let Err(error) = key_entry.delete_credential()
        && !matches!(error, keyring::Error::NoEntry)
    {
        return Err(error).context("delete device TLS private key from credential store");
    }
    let oauth_entry = oauth_entry(device_id)?;
    if let Err(error) = oauth_entry.delete_credential()
        && !matches!(error, keyring::Error::NoEntry)
    {
        return Err(error).context("delete OAuth credentials from credential store");
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove rejected identity {}", path.display()))
        }
    }
}

fn tls_key_entry(device_id: &str) -> anyhow::Result<keyring::Entry> {
    keyring::Entry::new(TLS_KEY_SERVICE, device_id)
        .context("open operating system credential store")
}

fn oauth_entry(device_id: &str) -> anyhow::Result<keyring::Entry> {
    keyring::Entry::new(OAUTH_SERVICE, device_id).context("open operating system credential store")
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
