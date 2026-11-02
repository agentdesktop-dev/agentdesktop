use std::path::Path;
#[cfg(any(target_os = "linux", windows))]
use std::path::PathBuf;

use anyhow::Context;
#[cfg(target_os = "macos")]
use apple_native_keyring_store::keychain::{Cred, MacKeychainDomain};

#[cfg(any(target_os = "linux", windows))]
use crate::secure_fs;

/// Platform-native durable storage for daemon secrets.
///
/// On Windows a secret is stored as an encrypted file in the daemon's private
/// state directory, and only its small random key goes into Credential
/// Manager, which limits a credential to 2560 bytes. OAuth credentials with
/// refresh tokens exceed that.
pub struct SecretStore {
    #[cfg(any(target_os = "linux", windows))]
    directory: PathBuf,
    #[cfg(target_os = "macos")]
    keychain: MacKeychainDomain,
}

impl SecretStore {
    pub fn new(state_dir: &Path) -> anyhow::Result<Self> {
        #[cfg(any(target_os = "linux", windows))]
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
            envelope::get(
                &self.entry_path(service, account),
                &CredentialManager,
                service,
                account,
            )
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
            envelope::set(
                &self.entry_path(service, account),
                &CredentialManager,
                service,
                account,
                secret,
            )
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
            envelope::delete(
                &self.entry_path(service, account),
                &CredentialManager,
                service,
                account,
            )
        }
    }

    #[cfg(any(target_os = "linux", windows))]
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

/// Windows Credential Manager entries, used for the per-secret file keys and
/// for reading secrets stored before the files existed.
#[cfg(windows)]
struct CredentialManager;

#[cfg(windows)]
impl envelope::KeyBackend for CredentialManager {
    fn get(&self, service: &str, account: &str) -> anyhow::Result<Option<String>> {
        let entry = keyring::Entry::new(service, account)
            .context("open operating system credential store")?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("read from operating system credential store"),
        }
    }

    fn set(&self, service: &str, account: &str, value: &str) -> anyhow::Result<()> {
        keyring::Entry::new(service, account)
            .context("open operating system credential store")?
            .set_password(value)
            .context("store in operating system credential store")
    }

    fn delete(&self, service: &str, account: &str) -> anyhow::Result<()> {
        let entry = keyring::Entry::new(service, account)
            .context("open operating system credential store")?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error).context("delete from operating system credential store"),
        }
    }
}

/// Secrets of any size kept as AES-256-GCM files whose keys live in a small
/// key store. Compiled on every platform for tests; used on Windows.
#[cfg(any(windows, test))]
mod envelope {
    use std::{fs, io::ErrorKind, path::Path};

    use anyhow::{Context, bail};
    use aws_lc_rs::aead::{AES_256_GCM, Aad, NONCE_LEN, Nonce, RandomizedNonceKey};
    use base64::{Engine, engine::general_purpose::STANDARD};

    use crate::secure_fs;

    const KEY_LEN: usize = 32;
    const TAG_LEN: usize = 16;
    const FORMAT_VERSION: u8 = 1;
    /// Appended to the secret's service to name the entry that holds its key
    /// (its account is `<store id>/<account>`). The secret's own service and
    /// account keep any value stored before the encrypted files existed, so it
    /// can still be read until it is replaced.
    const KEY_SERVICE_SUFFIX: &str = ".file-key";

    /// Small values in the operating system credential store. Deleting an
    /// entry that does not exist succeeds.
    pub(super) trait KeyBackend {
        fn get(&self, service: &str, account: &str) -> anyhow::Result<Option<String>>;
        fn set(&self, service: &str, account: &str, value: &str) -> anyhow::Result<()>;
        fn delete(&self, service: &str, account: &str) -> anyhow::Result<()>;
    }

    /// Locks the store across threads and processes (a second daemon instance
    /// runs secret-using tasks before it finds the first one). Writes and
    /// deletes take it exclusively, so two first writes cannot leave one
    /// write's key next to the other's file; reads take it shared, so a read
    /// never pairs a file with a key that a delete and a new write replaced
    /// in between. Released when the returned file is dropped.
    fn lock_store(path: &Path, exclusive: bool) -> anyhow::Result<fs::File> {
        let lock_path = path
            .parent()
            .context("secret path has no parent")?
            .join(".lock");
        let lock = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open secret lock {}", lock_path.display()))?;
        if exclusive {
            lock.lock()
        } else {
            lock.lock_shared()
        }
        .with_context(|| format!("lock {}", lock_path.display()))?;
        Ok(lock)
    }

    pub(super) fn get(
        path: &Path,
        keys: &impl KeyBackend,
        service: &str,
        account: &str,
    ) -> anyhow::Result<Option<String>> {
        let _reading = lock_store(path, false)?;
        let Some(sealed) = read_sealed(path)? else {
            // A secret stored directly in the credential store by an earlier
            // version, or none at all.
            return keys.get(service, account);
        };
        let stored_key = match store_id(path)? {
            Some(id) => read_key(keys, &key_entry(&id, service, account))?,
            None => StoredKey::Missing,
        };
        match stored_key {
            StoredKey::Key(key) => open(&key, service, account, sealed).map(Some),
            // Nothing can open the file any more (the credential store was
            // reset, or the state directory restored without it), so the secret
            // is gone. Reporting it as absent lets callers sign in again; the
            // next write replaces the file.
            StoredKey::Missing => {
                tracing::warn!(
                    service,
                    path = %path.display(),
                    "the key of a stored secret is missing; treating the secret as absent"
                );
                Ok(None)
            }
            StoredKey::Malformed => bail!("the key of a stored secret is malformed"),
        }
    }

    pub(super) fn set(
        path: &Path,
        keys: &impl KeyBackend,
        service: &str,
        account: &str,
        secret: &str,
    ) -> anyhow::Result<()> {
        let _writes = lock_store(path, true)?;
        let entry = key_entry(&create_store_id(path)?, service, account);
        // The key is created once and reused, so replacing the file is the only
        // step that changes the stored secret. A key that cannot be decoded
        // cannot open the current file either, so it is replaced.
        let key = match read_key(keys, &entry)? {
            StoredKey::Key(key) => key,
            StoredKey::Missing | StoredKey::Malformed => {
                // Nothing can open a file left from before, so remove it first:
                // if a later step fails, the secret reads as absent rather than
                // as a file next to the wrong key.
                remove_file(path)?;
                let mut key = [0; KEY_LEN];
                aws_lc_rs::rand::fill(&mut key)
                    .map_err(|_| anyhow::anyhow!("generate secret file key"))?;
                keys.set(&entry.service, &entry.account, &STANDARD.encode(key))
                    .context("store secret file key")?;
                key
            }
        };
        // Only the first write can replace a value an earlier version stored
        // directly; later writes skip the credential store lookup.
        let first_write = !path.try_exists().unwrap_or(true);
        write_sealed(path, &seal(&key, service, account, secret)?)?;
        // The file now shadows that value; remove it so no stale copy is left.
        if first_write && let Err(error) = keys.delete(service, account) {
            tracing::warn!(%error, service, "could not remove the replaced credential store entry");
        }
        Ok(())
    }

    pub(super) fn delete(
        path: &Path,
        keys: &impl KeyBackend,
        service: &str,
        account: &str,
    ) -> anyhow::Result<()> {
        let _writes = lock_store(path, true)?;
        // An earlier version's value goes first: it would be read again once
        // the file is gone, so a delete that stops halfway must not leave it.
        keys.delete(service, account)
            .context("delete secret from operating system credential store")?;
        remove_file(path)?;
        let Some(id) = store_id(path)? else {
            return Ok(());
        };
        let entry = key_entry(&id, service, account);
        keys.delete(&entry.service, &entry.account)
            .context("delete secret file key")
    }

    #[cfg(test)]
    thread_local! {
        /// Makes the next secret file write on this thread fail.
        static FAIL_NEXT_FILE_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    fn write_sealed(path: &Path, sealed: &[u8]) -> anyhow::Result<()> {
        #[cfg(test)]
        if FAIL_NEXT_FILE_WRITE.with(|fail| fail.replace(false)) {
            bail!("injected file write failure");
        }
        secure_fs::atomic_write(path, sealed, 0o600)
            .with_context(|| format!("store secret in {}", path.display()))
    }

    fn remove_file(path: &Path) -> anyhow::Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove secret {}", path.display())),
        }
    }

    /// Identifies the secrets directory. Credential Manager entries are shared
    /// by everything running as one Windows account, the files are per state
    /// directory, so the key entries are scoped to this identity.
    fn store_id(path: &Path) -> anyhow::Result<Option<String>> {
        let id_path = store_id_path(path)?;
        match fs::read(&id_path) {
            // Anything else, including bytes that are not UTF-8, is malformed.
            Ok(id) => Ok(String::from_utf8(id).ok().filter(|id| is_store_id(id))),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read {}", id_path.display())),
        }
    }

    /// Called with the write lock held. A malformed identity is replaced; the
    /// files it keyed then read as absent and are rewritten on the next write.
    fn create_store_id(path: &Path) -> anyhow::Result<String> {
        if let Some(id) = store_id(path)? {
            return Ok(id);
        }
        let mut bytes = [0; 16];
        aws_lc_rs::rand::fill(&mut bytes)
            .map_err(|_| anyhow::anyhow!("generate secret store identity"))?;
        let id = bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let id_path = store_id_path(path)?;
        secure_fs::atomic_write(&id_path, id.as_bytes(), 0o600)
            .with_context(|| format!("store {}", id_path.display()))?;
        Ok(id)
    }

    fn store_id_path(path: &Path) -> anyhow::Result<std::path::PathBuf> {
        Ok(path
            .parent()
            .context("secret path has no parent")?
            .join(".store-id"))
    }

    fn is_store_id(id: &str) -> bool {
        id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
    }

    fn read_sealed(path: &Path) -> anyhow::Result<Option<Vec<u8>>> {
        match fs::read(path) {
            Ok(sealed) => Ok(Some(sealed)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("read secret from {}", path.display()))
            }
        }
    }

    /// The credential store entry that holds one secret's file key.
    struct KeyEntry {
        service: String,
        account: String,
    }

    fn key_entry(store_id: &str, service: &str, account: &str) -> KeyEntry {
        KeyEntry {
            service: key_service(service),
            account: format!("{store_id}/{account}"),
        }
    }

    fn key_service(service: &str) -> String {
        format!("{service}{KEY_SERVICE_SUFFIX}")
    }

    enum StoredKey {
        Missing,
        Malformed,
        Key([u8; KEY_LEN]),
    }

    fn read_key(keys: &impl KeyBackend, entry: &KeyEntry) -> anyhow::Result<StoredKey> {
        let Some(encoded) = keys
            .get(&entry.service, &entry.account)
            .context("read secret file key")?
        else {
            return Ok(StoredKey::Missing);
        };
        Ok(STANDARD
            .decode(encoded)
            .ok()
            .and_then(|key| key.try_into().ok())
            .map_or(StoredKey::Malformed, StoredKey::Key))
    }

    /// Binds the file to its entry, so a file moved to another entry's path
    /// does not open.
    fn aad(service: &str, account: &str) -> Vec<u8> {
        let mut aad = b"agentdesktop secret v1\0".to_vec();
        aad.extend_from_slice(service.as_bytes());
        aad.push(0);
        aad.extend_from_slice(account.as_bytes());
        aad
    }

    /// `version || nonce || ciphertext || tag`
    fn seal(
        key: &[u8; KEY_LEN],
        service: &str,
        account: &str,
        secret: &str,
    ) -> anyhow::Result<Vec<u8>> {
        let key = RandomizedNonceKey::new(&AES_256_GCM, key)
            .map_err(|_| anyhow::anyhow!("load secret file key"))?;
        let mut ciphertext = secret.as_bytes().to_vec();
        let nonce = key
            .seal_in_place_append_tag(Aad::from(aad(service, account)), &mut ciphertext)
            .map_err(|_| anyhow::anyhow!("encrypt secret"))?;
        let mut sealed = Vec::with_capacity(1 + NONCE_LEN + ciphertext.len());
        sealed.push(FORMAT_VERSION);
        sealed.extend_from_slice(nonce.as_ref());
        sealed.extend_from_slice(&ciphertext);
        Ok(sealed)
    }

    fn open(
        key: &[u8; KEY_LEN],
        service: &str,
        account: &str,
        mut sealed: Vec<u8>,
    ) -> anyhow::Result<String> {
        if sealed.len() < 1 + NONCE_LEN + TAG_LEN {
            bail!("stored secret is truncated");
        }
        if sealed[0] != FORMAT_VERSION {
            bail!("stored secret has unknown format version {}", sealed[0]);
        }
        let nonce = Nonce::try_assume_unique_for_key(&sealed[1..1 + NONCE_LEN])
            .map_err(|_| anyhow::anyhow!("read stored secret nonce"))?;
        let key = RandomizedNonceKey::new(&AES_256_GCM, key)
            .map_err(|_| anyhow::anyhow!("load secret file key"))?;
        let plaintext = key
            .open_in_place(
                nonce,
                Aad::from(aad(service, account)),
                &mut sealed[1 + NONCE_LEN..],
            )
            .map_err(|_| anyhow::anyhow!("stored secret does not match its key"))?;
        String::from_utf8(plaintext.to_vec()).context("stored secret is not UTF-8")
    }

    #[cfg(test)]
    mod tests {
        use std::{
            cell::{Cell, RefCell},
            collections::BTreeMap,
            fs,
            path::{Path, PathBuf},
        };

        use super::{
            FAIL_NEXT_FILE_WRITE, KEY_SERVICE_SUFFIX, KeyBackend, delete, get, key_entry,
            key_service, set, store_id,
        };

        const SERVICE: &str = "dev.agentdesktop.test";
        const ACCOUNT: &str = "device";

        /// In-memory credential store with injectable failures.
        #[derive(Default)]
        struct FakeKeys {
            entries: RefCell<BTreeMap<(String, String), String>>,
            fail_sets: Cell<bool>,
            fail_deletes_of: RefCell<Option<String>>,
            deletes: Cell<usize>,
            /// When a file key is looked up, records whether a writer could take
            /// the lock in this directory right then.
            writer_could_lock_during_key_read: RefCell<Option<(PathBuf, Option<bool>)>>,
            /// Records whether this file exists when a new key is stored.
            file_at_key_write: RefCell<Option<(PathBuf, bool)>>,
        }

        impl FakeKeys {
            /// The file key of the secret at `path`.
            fn key_of(&self, path: &Path, service: &str) -> Option<String> {
                let entry = key_entry(&store_id(path).unwrap().unwrap(), service, ACCOUNT);
                self.entries
                    .borrow()
                    .get(&(entry.service, entry.account))
                    .cloned()
            }

            fn put_key(&self, path: &Path, service: &str, value: &str) {
                let entry = key_entry(&store_id(path).unwrap().unwrap(), service, ACCOUNT);
                self.entries
                    .borrow_mut()
                    .insert((entry.service, entry.account), value.to_owned());
            }

            fn value(&self, service: &str) -> Option<String> {
                self.entries
                    .borrow()
                    .get(&(service.to_owned(), ACCOUNT.to_owned()))
                    .cloned()
            }

            fn insert(&self, service: &str, value: &str) {
                self.entries
                    .borrow_mut()
                    .insert((service.to_owned(), ACCOUNT.to_owned()), value.to_owned());
            }
        }

        impl KeyBackend for FakeKeys {
            fn get(&self, service: &str, account: &str) -> anyhow::Result<Option<String>> {
                if service.ends_with(KEY_SERVICE_SUFFIX)
                    && let Some((directory, could_lock)) =
                        self.writer_could_lock_during_key_read.borrow_mut().as_mut()
                {
                    // A separate handle, as another process's writer would use.
                    let lock = fs::OpenOptions::new()
                        .write(true)
                        .open(directory.join(".lock"))
                        .unwrap();
                    *could_lock = Some(match lock.try_lock() {
                        Ok(()) => true,
                        Err(fs::TryLockError::WouldBlock) => false,
                        Err(fs::TryLockError::Error(error)) => panic!("{error}"),
                    });
                }
                Ok(self
                    .entries
                    .borrow()
                    .get(&(service.to_owned(), account.to_owned()))
                    .cloned())
            }

            fn set(&self, service: &str, account: &str, value: &str) -> anyhow::Result<()> {
                if self.fail_sets.get() {
                    anyhow::bail!("injected set failure");
                }
                if service.ends_with(KEY_SERVICE_SUFFIX)
                    && let Some((path, seen)) = self.file_at_key_write.borrow_mut().as_mut()
                {
                    *seen = path.exists();
                }
                // Windows Credential Manager limit; keyring stores UTF-16.
                let size = value.encode_utf16().count() * 2;
                assert!(size <= 2560, "credential too large: {size} bytes");
                self.entries
                    .borrow_mut()
                    .insert((service.to_owned(), account.to_owned()), value.to_owned());
                Ok(())
            }

            fn delete(&self, service: &str, account: &str) -> anyhow::Result<()> {
                self.deletes.set(self.deletes.get() + 1);
                if self.fail_deletes_of.borrow().as_deref() == Some(service) {
                    anyhow::bail!("injected delete failure");
                }
                self.entries
                    .borrow_mut()
                    .remove(&(service.to_owned(), account.to_owned()));
                Ok(())
            }
        }

        fn entry(directory: &tempfile::TempDir, name: &str) -> PathBuf {
            directory.path().join(name)
        }

        #[test]
        fn a_secret_larger_than_a_credential_round_trips() {
            let directory = tempfile::tempdir().unwrap();
            let path = entry(&directory, "entry");
            let keys = FakeKeys::default();
            let secret = "x".repeat(20_000);

            set(&path, &keys, SERVICE, ACCOUNT, &secret).unwrap();
            assert_eq!(get(&path, &keys, SERVICE, ACCOUNT).unwrap(), Some(secret));
            assert!(
                !fs::read(&path)
                    .unwrap()
                    .windows(16)
                    .any(|w| w == b"xxxxxxxxxxxxxxxx")
            );
            assert_eq!(keys.entries.borrow().len(), 1);

            set(&path, &keys, SERVICE, ACCOUNT, "second").unwrap();
            assert_eq!(
                get(&path, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("second")
            );
        }

        #[test]
        fn the_key_is_created_once_and_reused() {
            let directory = tempfile::tempdir().unwrap();
            let path = entry(&directory, "entry");
            let keys = FakeKeys::default();

            set(&path, &keys, SERVICE, ACCOUNT, "first").unwrap();
            let key = keys.key_of(&path, SERVICE).unwrap();
            set(&path, &keys, SERVICE, ACCOUNT, "second").unwrap();
            assert_eq!(keys.key_of(&path, SERVICE).unwrap(), key);
        }

        #[test]
        fn a_value_stored_by_an_earlier_version_is_read_and_replaced() {
            let directory = tempfile::tempdir().unwrap();
            let path = entry(&directory, "entry");
            let keys = FakeKeys::default();
            keys.insert(SERVICE, "legacy");

            assert_eq!(
                get(&path, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("legacy")
            );
            set(&path, &keys, SERVICE, ACCOUNT, "new").unwrap();
            assert_eq!(keys.value(SERVICE), None);
            assert_eq!(
                get(&path, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("new")
            );
        }

        #[test]
        fn a_read_holds_off_writers_until_it_has_the_key() {
            // Otherwise a delete and a new write between reading the file and
            // reading its key would pair the old file with the new key.
            let directory = tempfile::tempdir().unwrap();
            let path = entry(&directory, "entry");
            let keys = FakeKeys::default();
            set(&path, &keys, SERVICE, ACCOUNT, "secret").unwrap();

            *keys.writer_could_lock_during_key_read.borrow_mut() =
                Some((directory.path().to_owned(), None));
            assert_eq!(
                get(&path, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("secret")
            );
            assert_eq!(
                keys.writer_could_lock_during_key_read
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .1,
                Some(false)
            );
        }

        /// Thread-safe store whose key writes are slow, so unserialized first
        /// writes would interleave.
        #[derive(Default)]
        struct SlowKeys(std::sync::Mutex<BTreeMap<(String, String), String>>);

        impl KeyBackend for SlowKeys {
            fn get(&self, service: &str, account: &str) -> anyhow::Result<Option<String>> {
                Ok(self
                    .0
                    .lock()
                    .unwrap()
                    .get(&(service.to_owned(), account.to_owned()))
                    .cloned())
            }

            fn set(&self, service: &str, account: &str, value: &str) -> anyhow::Result<()> {
                std::thread::sleep(std::time::Duration::from_millis(20));
                self.0
                    .lock()
                    .unwrap()
                    .insert((service.to_owned(), account.to_owned()), value.to_owned());
                Ok(())
            }

            fn delete(&self, service: &str, account: &str) -> anyhow::Result<()> {
                self.0
                    .lock()
                    .unwrap()
                    .remove(&(service.to_owned(), account.to_owned()));
                Ok(())
            }
        }

        #[test]
        fn competing_first_writes_leave_a_readable_secret() {
            // Each write opens its own lock handle, as a second process would.
            for _ in 0..10 {
                let directory = tempfile::tempdir().unwrap();
                let path = entry(&directory, "entry");
                let keys = SlowKeys::default();
                std::thread::scope(|scope| {
                    for writer in ["one", "two", "three"] {
                        let (path, keys) = (&path, &keys);
                        scope.spawn(move || set(path, keys, SERVICE, ACCOUNT, writer).unwrap());
                    }
                });
                let stored = get(&path, &keys, SERVICE, ACCOUNT).unwrap().unwrap();
                assert!(["one", "two", "three"].contains(&stored.as_str()));
            }
        }

        #[test]
        fn a_new_key_is_never_stored_next_to_an_old_file() {
            let directory = tempfile::tempdir().unwrap();
            let path = entry(&directory, "entry");
            let keys = FakeKeys::default();
            set(&path, &keys, SERVICE, ACCOUNT, "secret").unwrap();

            // The key is lost; the file survives. Had the new key been stored
            // first, a failed file write would leave the old file next to it,
            // and reads would fail instead of reporting the secret absent.
            keys.entries.borrow_mut().clear();
            *keys.file_at_key_write.borrow_mut() = Some((path.clone(), true));
            set(&path, &keys, SERVICE, ACCOUNT, "new").unwrap();
            assert!(!keys.file_at_key_write.borrow().as_ref().unwrap().1);
            assert_eq!(
                get(&path, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("new")
            );
        }

        #[test]
        fn stores_in_two_directories_keep_their_own_keys() {
            let (first, second) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
            let (first, second) = (entry(&first, "entry"), entry(&second, "entry"));
            let keys = FakeKeys::default();

            set(&first, &keys, SERVICE, ACCOUNT, "first").unwrap();
            set(&second, &keys, SERVICE, ACCOUNT, "second").unwrap();
            assert_eq!(
                get(&first, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("first")
            );
            assert_eq!(
                get(&second, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("second")
            );

            delete(&first, &keys, SERVICE, ACCOUNT).unwrap();
            assert_eq!(get(&first, &keys, SERVICE, ACCOUNT).unwrap(), None);
            assert_eq!(
                get(&second, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("second")
            );
        }

        #[test]
        fn a_malformed_store_identity_is_replaced_by_the_next_write() {
            let directory = tempfile::tempdir().unwrap();
            let path = entry(&directory, "entry");
            let keys = FakeKeys::default();
            set(&path, &keys, SERVICE, ACCOUNT, "secret").unwrap();

            for malformed in [&b"\xff\xfe not utf-8"[..], b"too short"] {
                fs::write(directory.path().join(".store-id"), malformed).unwrap();
                assert_eq!(get(&path, &keys, SERVICE, ACCOUNT).unwrap(), None);
                set(&path, &keys, SERVICE, ACCOUNT, "rewritten").unwrap();
                assert_eq!(
                    get(&path, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                    Some("rewritten")
                );
            }
        }

        #[test]
        fn only_the_first_write_removes_an_earlier_versions_value() {
            let directory = tempfile::tempdir().unwrap();
            let path = entry(&directory, "entry");
            let keys = FakeKeys::default();
            keys.insert(SERVICE, "legacy");

            set(&path, &keys, SERVICE, ACCOUNT, "first").unwrap();
            assert_eq!(keys.deletes.get(), 1);
            set(&path, &keys, SERVICE, ACCOUNT, "second").unwrap();
            assert_eq!(keys.deletes.get(), 1);
        }

        #[test]
        fn a_failed_key_write_keeps_the_current_value() {
            let directory = tempfile::tempdir().unwrap();
            let path = entry(&directory, "entry");
            let keys = FakeKeys::default();
            keys.insert(SERVICE, "legacy");
            keys.fail_sets.set(true);

            assert!(set(&path, &keys, SERVICE, ACCOUNT, "new").is_err());
            assert!(!path.exists());
            assert_eq!(
                get(&path, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("legacy")
            );
        }

        #[test]
        fn a_failed_file_write_keeps_the_current_value() {
            let directory = tempfile::tempdir().unwrap();
            let path = entry(&directory, "entry");
            let keys = FakeKeys::default();
            set(&path, &keys, SERVICE, ACCOUNT, "current").unwrap();
            let before = fs::read(&path).unwrap();

            FAIL_NEXT_FILE_WRITE.with(|fail| fail.set(true));
            assert!(set(&path, &keys, SERVICE, ACCOUNT, "new").is_err());
            assert_eq!(fs::read(&path).unwrap(), before);
            assert_eq!(
                get(&path, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("current")
            );
        }

        #[test]
        fn a_failed_legacy_cleanup_still_stores_the_secret() {
            let directory = tempfile::tempdir().unwrap();
            let path = entry(&directory, "entry");
            let keys = FakeKeys::default();
            keys.insert(SERVICE, "legacy");
            *keys.fail_deletes_of.borrow_mut() = Some(SERVICE.to_owned());

            set(&path, &keys, SERVICE, ACCOUNT, "new").unwrap();
            assert_eq!(
                get(&path, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("new")
            );
        }

        #[test]
        fn a_file_does_not_open_under_another_entry() {
            let directory = tempfile::tempdir().unwrap();
            let path = entry(&directory, "entry");
            let other = entry(&directory, "other");
            let keys = FakeKeys::default();
            set(&path, &keys, SERVICE, ACCOUNT, "secret").unwrap();
            // Same key for both entries, so only the binding can reject the copy.
            let key = keys.key_of(&path, SERVICE).unwrap();
            keys.put_key(&other, "dev.agentdesktop.other", &key);
            set(&other, &keys, "dev.agentdesktop.other", ACCOUNT, "other").unwrap();
            assert_eq!(keys.key_of(&other, "dev.agentdesktop.other").unwrap(), key);

            fs::copy(&path, &other).unwrap();
            assert!(get(&other, &keys, "dev.agentdesktop.other", ACCOUNT).is_err());
        }

        #[test]
        fn a_tampered_file_or_malformed_key_is_an_error_and_a_missing_key_reads_as_absent() {
            let directory = tempfile::tempdir().unwrap();
            let path = entry(&directory, "entry");
            let keys = FakeKeys::default();
            set(&path, &keys, SERVICE, ACCOUNT, "secret").unwrap();

            let mut sealed = fs::read(&path).unwrap();
            *sealed.last_mut().unwrap() ^= 1;
            fs::write(&path, sealed).unwrap();
            assert!(get(&path, &keys, SERVICE, ACCOUNT).is_err());
            set(&path, &keys, SERVICE, ACCOUNT, "repaired").unwrap();
            assert_eq!(
                get(&path, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("repaired")
            );

            keys.entries.borrow_mut().clear();
            assert_eq!(get(&path, &keys, SERVICE, ACCOUNT).unwrap(), None);
            set(&path, &keys, SERVICE, ACCOUNT, "rekeyed").unwrap();
            assert_eq!(
                get(&path, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("rekeyed")
            );

            keys.put_key(&path, SERVICE, "not base64!");
            assert!(get(&path, &keys, SERVICE, ACCOUNT).is_err());
            set(&path, &keys, SERVICE, ACCOUNT, "fixed").unwrap();
            assert_eq!(
                get(&path, &keys, SERVICE, ACCOUNT).unwrap().as_deref(),
                Some("fixed")
            );
        }

        #[test]
        fn delete_removes_everything_and_a_partial_delete_never_resurrects_an_old_value() {
            let directory = tempfile::tempdir().unwrap();
            let path = entry(&directory, "entry");
            let keys = FakeKeys::default();
            keys.insert(SERVICE, "legacy");
            *keys.fail_deletes_of.borrow_mut() = Some(SERVICE.to_owned());
            set(&path, &keys, SERVICE, ACCOUNT, "new").unwrap();
            *keys.fail_deletes_of.borrow_mut() = None;

            // Stops after the file: the secret reads as gone, a retry finishes.
            *keys.fail_deletes_of.borrow_mut() = Some(key_service(SERVICE));
            assert!(delete(&path, &keys, SERVICE, ACCOUNT).is_err());
            assert_eq!(get(&path, &keys, SERVICE, ACCOUNT).unwrap(), None);
            *keys.fail_deletes_of.borrow_mut() = None;
            delete(&path, &keys, SERVICE, ACCOUNT).unwrap();

            assert!(!path.exists());
            assert!(keys.entries.borrow().is_empty());
            delete(&path, &keys, SERVICE, ACCOUNT).unwrap();
        }
    }
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
    use super::SecretStore;

    #[test]
    fn a_secret_larger_than_a_credential_round_trips_through_credential_manager() {
        let directory = tempfile::tempdir().unwrap();
        let store = SecretStore::new(directory.path()).unwrap();
        let account = format!("test-{}", rand::random::<u64>());
        let secret = "x".repeat(10_000);

        store
            .set("dev.agentdesktop.test", &account, &secret)
            .unwrap();
        assert_eq!(
            store.get("dev.agentdesktop.test", &account).unwrap(),
            secret
        );
        store.delete("dev.agentdesktop.test", &account).unwrap();
        assert_eq!(
            store
                .get_optional("dev.agentdesktop.test", &account)
                .unwrap(),
            None
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
