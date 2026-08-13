use std::{fs, path::Path};

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::secure_fs;

const CREDENTIAL_SERVICE: &str = "dev.agentdesktop.device";

#[derive(Clone, Debug)]
pub struct Identity {
    pub device_id: String,
    pub credential: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredIdentity {
    device_id: String,
    #[serde(default, skip_serializing)]
    credential: Option<String>,
}

pub fn load(path: &Path) -> anyhow::Result<Option<Identity>> {
    let stored: StoredIdentity = match fs::read(path) {
        Ok(contents) => serde_json::from_slice(&contents).context("parse device identity")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read identity from {}", path.display()));
        }
    };
    let entry = credential_entry(&stored.device_id)?;
    let credential = match stored.credential {
        Some(credential) => {
            entry
                .set_password(&credential)
                .context("migrate device credential to operating system credential store")?;
            write_metadata(path, &stored.device_id)?;
            credential
        }
        None => entry
            .get_password()
            .context("read device credential from operating system credential store")?,
    };
    Ok(Some(Identity {
        device_id: stored.device_id,
        credential,
    }))
}

pub fn save(path: &Path, identity: &Identity) -> anyhow::Result<()> {
    credential_entry(&identity.device_id)?
        .set_password(&identity.credential)
        .context("store device credential in operating system credential store")?;
    write_metadata(path, &identity.device_id)
}

pub fn delete(path: &Path, device_id: &str) -> anyhow::Result<()> {
    let entry = credential_entry(device_id)?;
    if let Err(error) = entry.delete_credential()
        && !matches!(error, keyring::Error::NoEntry)
    {
        return Err(error)
            .context("delete device credential from operating system credential store");
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove rejected identity {}", path.display()))
        }
    }
}

fn credential_entry(device_id: &str) -> anyhow::Result<keyring::Entry> {
    keyring::Entry::new(CREDENTIAL_SERVICE, device_id)
        .context("open operating system credential store")
}

fn write_metadata(path: &Path, device_id: &str) -> anyhow::Result<()> {
    let parent = path.parent().context("identity path has no parent")?;
    secure_fs::ensure_private_dir(parent)?;
    let stored = StoredIdentity {
        device_id: device_id.to_owned(),
        credential: None,
    };
    secure_fs::atomic_write(path, &serde_json::to_vec_pretty(&stored)?, 0o600)
}

#[cfg(test)]
mod tests {
    use super::StoredIdentity;

    #[test]
    fn stored_identity_never_serializes_a_legacy_credential() {
        let stored = StoredIdentity {
            device_id: "device-123".to_owned(),
            credential: Some("secret".to_owned()),
        };
        let json = serde_json::to_value(stored).expect("serialize stored identity");
        assert_eq!(json["deviceId"], "device-123");
        assert!(json.get("credential").is_none());
    }
}
