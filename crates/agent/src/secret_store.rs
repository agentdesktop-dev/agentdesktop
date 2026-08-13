use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

use anyhow::Context;

#[cfg(target_os = "linux")]
use crate::secure_fs;

/// Platform-native durable storage for daemon secrets.
pub struct SecretStore {
    #[cfg(target_os = "linux")]
    directory: PathBuf,
}

impl SecretStore {
    pub fn new(state_dir: &Path) -> anyhow::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let directory = state_dir.join("secrets");
            secure_fs::ensure_private_dir(&directory)?;
            Ok(Self { directory })
        }

        #[cfg(any(target_os = "macos", windows))]
        {
            let _ = state_dir;
            Ok(Self {})
        }
    }

    pub fn get(&self, service: &str, account: &str) -> anyhow::Result<String> {
        #[cfg(target_os = "linux")]
        {
            let path = self.entry_path(service, account);
            let secret = std::fs::read(&path)
                .with_context(|| format!("read secret from {}", path.display()))?;
            String::from_utf8(secret).context("stored secret is not UTF-8")
        }

        #[cfg(any(target_os = "macos", windows))]
        keyring::Entry::new(service, account)
            .context("open operating system credential store")?
            .get_password()
            .context("read secret from operating system credential store")
    }

    pub fn set(&self, service: &str, account: &str, secret: &str) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let path = self.entry_path(service, account);
            secure_fs::atomic_write(&path, secret.as_bytes(), 0o600)
                .with_context(|| format!("store secret in {}", path.display()))
        }

        #[cfg(any(target_os = "macos", windows))]
        keyring::Entry::new(service, account)
            .context("open operating system credential store")?
            .set_password(secret)
            .context("store secret in operating system credential store")
    }

    pub fn delete(&self, service: &str, account: &str) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let path = self.entry_path(service, account);
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => {
                    Err(error).with_context(|| format!("remove secret {}", path.display()))
                }
            }
        }

        #[cfg(any(target_os = "macos", windows))]
        {
            let entry = keyring::Entry::new(service, account)
                .context("open operating system credential store")?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => {
                    Err(error).context("delete secret from operating system credential store")
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn entry_path(&self, service: &str, account: &str) -> PathBuf {
        use sha2::{Digest, Sha256};

        let mut digest = Sha256::new();
        digest.update(service.as_bytes());
        digest.update([0]);
        digest.update(account.as_bytes());
        let name = digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        self.directory.join(name)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use super::SecretStore;

    #[test]
    fn linux_store_round_trips_private_entries() {
        let directory = std::env::temp_dir().join(format!(
            "agentdesktop-secret-store-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir(&directory).unwrap();
        let store = SecretStore::new(&directory).unwrap();

        store.set("service", "account", "secret").unwrap();
        assert_eq!(store.get("service", "account").unwrap(), "secret");

        let entries: Vec<_> = fs::read_dir(directory.join("secrets"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            fs::metadata(&entries[0]).unwrap().permissions().mode() & 0o777,
            0o600
        );

        store.delete("service", "account").unwrap();
        assert!(!entries[0].exists());
        fs::remove_dir_all(directory).unwrap();
    }
}
