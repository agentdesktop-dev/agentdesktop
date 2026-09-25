use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

use anyhow::Context;
#[cfg(windows)]
use anyhow::bail;
#[cfg(target_os = "macos")]
use apple_native_keyring_store::keychain::{Cred, MacKeychainDomain};

#[cfg(target_os = "linux")]
use crate::secure_fs;

#[cfg(windows)]
const WINDOWS_SECRET_CHUNK_SIZE: usize = 1_200;
#[cfg(windows)]
const WINDOWS_CHUNK_MANIFEST_PREFIX: &str = "agentdesktop-secret-chunks-v1:";
#[cfg(windows)]
const WINDOWS_CHUNK_ACCOUNT_SEPARATOR: &str = ":agentdesktop-secret-chunk:";

/// Platform-native durable storage for daemon secrets.
pub struct SecretStore {
    #[cfg(target_os = "linux")]
    directory: PathBuf,
    #[cfg(target_os = "macos")]
    keychain: MacKeychainDomain,
}

impl SecretStore {
    pub fn new(state_dir: &Path) -> anyhow::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let directory = state_dir.join("secrets");
            secure_fs::ensure_private_dir(&directory)?;
            Ok(Self { directory })
        }

        #[cfg(target_os = "macos")]
        {
            let _ = state_dir;
            // SAFETY: geteuid has no preconditions.
            let effective_uid = unsafe { libc::geteuid() };
            Ok(Self {
                keychain: macos_keychain_domain(effective_uid),
            })
        }

        #[cfg(windows)]
        {
            let _ = state_dir;
            Ok(Self {})
        }
    }

    pub fn get(&self, service: &str, account: &str) -> anyhow::Result<String> {
        self.get_optional(service, account)?
            .context("secret was not found")
    }

    pub fn get_optional(&self, service: &str, account: &str) -> anyhow::Result<Option<String>> {
        #[cfg(target_os = "linux")]
        {
            let path = self.entry_path(service, account);
            match std::fs::read(&path) {
                Ok(secret) => String::from_utf8(secret)
                    .context("stored secret is not UTF-8")
                    .map(Some),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => {
                    Err(error).with_context(|| format!("read secret from {}", path.display()))
                }
            }
        }

        #[cfg(target_os = "macos")]
        {
            let entry = Cred::build(self.keychain.clone(), service, account)
                .context("open operating system credential store")?;
            match entry.get_password() {
                Ok(secret) => Ok(Some(secret)),
                Err(keyring_core::Error::NoEntry) => Ok(None),
                Err(error) => {
                    Err(error).context("read secret from operating system credential store")
                }
            }
        }

        #[cfg(windows)]
        {
            let entry = keyring::Entry::new(service, account)
                .context("open operating system credential store")?;
            match entry.get_password() {
                Ok(secret) => match windows_manifest_chunk_count(&secret)? {
                    Some(chunk_count) => Ok(Some(
                        read_windows_chunks(service, account, chunk_count)?.concat(),
                    )),
                    None => Ok(Some(secret)),
                },
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => {
                    Err(error).context("read secret from operating system credential store")
                }
            }
        }
    }

    pub fn set(&self, service: &str, account: &str, secret: &str) -> anyhow::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let path = self.entry_path(service, account);
            secure_fs::atomic_write(&path, secret.as_bytes(), 0o600)
                .with_context(|| format!("store secret in {}", path.display()))
        }

        #[cfg(target_os = "macos")]
        {
            Cred::build(self.keychain.clone(), service, account)
                .context("open operating system credential store")?
                .set_password(secret)
                .context("store secret in operating system credential store")
        }

        #[cfg(windows)]
        {
            let entry = keyring::Entry::new(service, account)
                .context("open operating system credential store")?;
            let existing_chunks = match entry.get_password() {
                Ok(existing_secret) => match windows_manifest_chunk_count(&existing_secret)? {
                    Some(chunk_count) => Some(read_windows_chunks(service, account, chunk_count)?),
                    None => None,
                },
                Err(keyring::Error::NoEntry) => None,
                Err(error) => {
                    return Err(error)
                        .context("read secret from operating system credential store");
                }
            };

            if secret.len() <= WINDOWS_SECRET_CHUNK_SIZE {
                entry
                    .set_password(secret)
                    .context("store secret in operating system credential store")?;
                if let Some(existing_chunks) = &existing_chunks {
                    delete_windows_chunks(service, account, 0..existing_chunks.len())?;
                }
                Ok(())
            } else {
                let chunks = utf8_chunks(secret, WINDOWS_SECRET_CHUNK_SIZE).collect::<Vec<_>>();
                let mut stored_chunk_count = 0;
                for (chunk_index, chunk) in chunks.iter().enumerate() {
                    let chunk_account = windows_chunk_account(account, chunk_index);
                    let chunk_entry = match keyring::Entry::new(service, &chunk_account)
                        .context("open operating system credential store")
                    {
                        Ok(entry) => entry,
                        Err(error) => {
                            return Err(rollback_windows_chunks(
                                service,
                                account,
                                existing_chunks.as_deref(),
                                stored_chunk_count,
                                error,
                            ));
                        }
                    };
                    if let Err(error) = chunk_entry.set_password(chunk).with_context(|| {
                        format!(
                            "store secret chunk {} in operating system credential store",
                            chunk_index + 1
                        )
                    }) {
                        return Err(rollback_windows_chunks(
                            service,
                            account,
                            existing_chunks.as_deref(),
                            stored_chunk_count,
                            error,
                        ));
                    }
                    stored_chunk_count = chunk_index + 1;
                }

                if let Err(error) = entry
                    .set_password(&format!("{WINDOWS_CHUNK_MANIFEST_PREFIX}{}", chunks.len()))
                    .context("store secret in operating system credential store")
                {
                    return Err(rollback_windows_chunks(
                        service,
                        account,
                        existing_chunks.as_deref(),
                        stored_chunk_count,
                        error,
                    ));
                }

                if let Some(existing_chunks) = &existing_chunks {
                    if existing_chunks.len() > chunks.len() {
                        delete_windows_chunks(
                            service,
                            account,
                            chunks.len()..existing_chunks.len(),
                        )?;
                    }
                }
                Ok(())
            }
        }
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

        #[cfg(target_os = "macos")]
        {
            let entry = Cred::build(self.keychain.clone(), service, account)
                .context("open operating system credential store")?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
                Err(error) => {
                    Err(error).context("delete secret from operating system credential store")
                }
            }
        }

        #[cfg(windows)]
        {
            let entry = keyring::Entry::new(service, account)
                .context("open operating system credential store")?;
            let chunk_count = match entry.get_password() {
                Ok(secret) => windows_manifest_chunk_count(&secret)?,
                Err(keyring::Error::NoEntry) => return Ok(()),
                Err(error) => {
                    return Err(error)
                        .context("read secret from operating system credential store");
                }
            };

            if let Some(chunk_count) = chunk_count {
                delete_windows_chunks(service, account, 0..chunk_count)?;
            }

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

#[cfg(windows)]
fn windows_chunk_account(account: &str, chunk_index: usize) -> String {
    format!("{account}{WINDOWS_CHUNK_ACCOUNT_SEPARATOR}{chunk_index}")
}

#[cfg(windows)]
fn read_windows_chunks(
    service: &str,
    account: &str,
    chunk_count: usize,
) -> anyhow::Result<Vec<String>> {
    let mut chunks = Vec::with_capacity(chunk_count);
    for chunk_index in 0..chunk_count {
        let chunk_account = windows_chunk_account(account, chunk_index);
        let chunk = keyring::Entry::new(service, &chunk_account)
            .context("open operating system credential store")?
            .get_password()
            .with_context(|| {
                format!(
                    "read secret chunk {} from operating system credential store",
                    chunk_index + 1
                )
            })?;
        chunks.push(chunk);
    }
    Ok(chunks)
}

#[cfg(windows)]
fn windows_manifest_chunk_count(secret: &str) -> anyhow::Result<Option<usize>> {
    let Some(chunk_count) = secret.strip_prefix(WINDOWS_CHUNK_MANIFEST_PREFIX) else {
        return Ok(None);
    };
    let chunk_count = chunk_count
        .parse::<usize>()
        .context("parse Windows secret chunk manifest")?;
    if chunk_count < 2 {
        bail!("Windows secret chunk manifest must contain at least two chunks");
    }
    Ok(Some(chunk_count))
}

#[cfg(windows)]
fn delete_windows_chunks(
    service: &str,
    account: &str,
    chunk_indices: std::ops::Range<usize>,
) -> anyhow::Result<()> {
    for chunk_index in chunk_indices {
        let chunk_account = windows_chunk_account(account, chunk_index);
        let entry = keyring::Entry::new(service, &chunk_account)
            .context("open operating system credential store")?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "delete secret chunk {} from operating system credential store",
                        chunk_index + 1
                    )
                });
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn rollback_windows_chunks(
    service: &str,
    account: &str,
    existing_chunks: Option<&[String]>,
    written_chunk_count: usize,
    error: anyhow::Error,
) -> anyhow::Error {
    let rollback_result = match existing_chunks {
        Some(existing_chunks) => {
            restore_windows_chunks(service, account, existing_chunks, written_chunk_count)
        }
        None => delete_windows_chunks(service, account, 0..written_chunk_count),
    };

    match rollback_result {
        Ok(()) => error,
        Err(rollback_error) => error.context(format!(
            "restore prior Windows secret chunks after a failed update: {rollback_error:#}"
        )),
    }
}

#[cfg(windows)]
fn restore_windows_chunks(
    service: &str,
    account: &str,
    existing_chunks: &[String],
    written_chunk_count: usize,
) -> anyhow::Result<()> {
    for (chunk_index, chunk) in existing_chunks.iter().enumerate() {
        let chunk_account = windows_chunk_account(account, chunk_index);
        keyring::Entry::new(service, &chunk_account)
            .context("open operating system credential store")?
            .set_password(chunk)
            .with_context(|| {
                format!(
                    "restore secret chunk {} in operating system credential store",
                    chunk_index + 1
                )
            })?;
    }
    if written_chunk_count > existing_chunks.len() {
        delete_windows_chunks(service, account, existing_chunks.len()..written_chunk_count)?;
    }
    Ok(())
}

#[cfg(windows)]
fn utf8_chunks(secret: &str, chunk_size: usize) -> impl Iterator<Item = &str> {
    assert!(chunk_size > 0, "chunk_size must be greater than zero");

    let mut start = 0;
    std::iter::from_fn(move || {
        if start >= secret.len() {
            return None;
        }

        let mut end = (start + chunk_size).min(secret.len());
        while end > start && !secret.is_char_boundary(end) {
            end -= 1;
        }
        if end == start {
            end = start + secret[start..].chars().next()?.len_utf8();
        }

        let chunk = &secret[start..end];
        start = end;
        Some(chunk)
    })
}

#[cfg(target_os = "macos")]
fn macos_keychain_domain(effective_uid: u32) -> MacKeychainDomain {
    if effective_uid == 0 {
        MacKeychainDomain::System
    } else {
        MacKeychainDomain::User
    }
}

#[cfg(all(test, target_os = "macos"))]
mod macos_tests {
    use apple_native_keyring_store::keychain::MacKeychainDomain;

    use super::macos_keychain_domain;

    #[test]
    fn keychain_domain_follows_daemon_privilege() {
        assert_eq!(macos_keychain_domain(0), MacKeychainDomain::System);
        assert_eq!(macos_keychain_domain(501), MacKeychainDomain::User);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::{
        WINDOWS_SECRET_CHUNK_SIZE, utf8_chunks, windows_chunk_account, windows_manifest_chunk_count,
    };

    #[test]
    fn windows_secret_helpers_preserve_utf8_boundaries() {
        let secret = format!("{}🙂tail", "a".repeat(WINDOWS_SECRET_CHUNK_SIZE - 1));
        let chunks = utf8_chunks(&secret, WINDOWS_SECRET_CHUNK_SIZE).collect::<Vec<_>>();

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "a".repeat(WINDOWS_SECRET_CHUNK_SIZE - 1));
        assert_eq!(chunks[0].len(), WINDOWS_SECRET_CHUNK_SIZE - 1);
        assert_eq!(chunks[1], "🙂tail");
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.is_char_boundary(chunk.len()))
        );
        assert_eq!(chunks.concat(), secret);
        assert_eq!(
            windows_chunk_account("account", 2),
            "account:agentdesktop-secret-chunk:2"
        );
    }

    #[test]
    fn windows_secret_manifest_validation_rejects_invalid_counts() {
        assert_eq!(windows_manifest_chunk_count("plain-secret").unwrap(), None);
        assert_eq!(
            windows_manifest_chunk_count("agentdesktop-secret-chunks-v1:2").unwrap(),
            Some(2)
        );

        let too_small = windows_manifest_chunk_count("agentdesktop-secret-chunks-v1:1")
            .expect_err("one chunk should not be treated as a valid manifest");
        assert!(too_small.to_string().contains("at least two chunks"));

        let invalid_number =
            windows_manifest_chunk_count("agentdesktop-secret-chunks-v1:not-a-number")
                .expect_err("non-numeric manifests should fail to parse");
        assert!(
            invalid_number
                .to_string()
                .contains("parse Windows secret chunk manifest")
        );
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
