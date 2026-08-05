use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use clap::ValueEnum;
use sha2::{Digest, Sha256};

const SERVICE: &str = "agentdesktop";
const BACKEND_FILE: &str = "credential-storage";

pub fn default_storage_root() -> Result<PathBuf> {
    if let Some(config_home) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(config_home).join(SERVICE).join("identity"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join(SERVICE)
        .join("identity"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum CredentialStorageMode {
    Auto,
    SecretService,
    File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SelectedBackend {
    SecretService,
    File,
}

#[derive(Clone, Debug)]
pub enum CredentialStore {
    SecretService,
    File(ProtectedFileStore),
}

impl CredentialStore {
    pub fn setup(mode: CredentialStorageMode, root: &Path) -> Result<Self> {
        ensure_secure_directory(root)?;
        let secret_service = preflight_secret_service();
        let selected = select_backend(mode, secret_service.is_ok())?;
        let store = match selected {
            SelectedBackend::SecretService => Self::SecretService,
            SelectedBackend::File => {
                let store = ProtectedFileStore::new(root.join("credentials"))?;
                store.preflight()?;
                Self::File(store)
            }
        };
        write_secure_file(root, BACKEND_FILE, selected.as_str().as_bytes())?;
        if mode == CredentialStorageMode::SecretService {
            secret_service.context("Linux Secret Service preflight failed")?;
        }
        Ok(store)
    }

    pub fn load(root: &Path) -> Result<Self> {
        ensure_secure_directory(root)?;
        let selected = String::from_utf8(read_secure_file(root, BACKEND_FILE)?)?;
        match selected.as_str() {
            "secret-service" => {
                preflight_secret_service().context("configured Secret Service is unavailable")?;
                Ok(Self::SecretService)
            }
            "file" => Ok(Self::File(ProtectedFileStore::new(
                root.join("credentials"),
            )?)),
            _ => bail!("unknown persisted credential storage backend {selected:?}"),
        }
    }

    pub fn put(&self, record: &str, secret: &[u8]) -> Result<()> {
        match self {
            Self::SecretService => secret_service_entry(record)?.set_secret(secret)?,
            Self::File(store) => store.put(record, secret)?,
        }
        Ok(())
    }

    pub fn get(&self, record: &str) -> Result<Vec<u8>> {
        match self {
            Self::SecretService => Ok(secret_service_entry(record)?.get_secret()?),
            Self::File(store) => store.get(record),
        }
    }

    pub fn get_optional(&self, record: &str) -> Result<Option<Vec<u8>>> {
        match self {
            Self::SecretService => match secret_service_entry(record)?.get_secret() {
                Ok(secret) => Ok(Some(secret)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(error.into()),
            },
            Self::File(store) => store.get_optional(record),
        }
    }

    pub fn delete(&self, record: &str) -> Result<()> {
        match self {
            Self::SecretService => secret_service_entry(record)?.delete_credential()?,
            Self::File(store) => store.delete(record)?,
        }
        Ok(())
    }

    pub fn delete_if_exists(&self, record: &str) -> Result<()> {
        match self {
            Self::SecretService => match secret_service_entry(record)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(error.into()),
            },
            Self::File(store) => store.delete_if_exists(record),
        }
    }

    pub const fn backend_name(&self) -> &'static str {
        match self {
            Self::SecretService => "secret-service",
            Self::File(_) => "file",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProtectedFileStore {
    root: PathBuf,
}

impl ProtectedFileStore {
    fn new(root: PathBuf) -> Result<Self> {
        ensure_secure_directory(&root)?;
        Ok(Self { root })
    }

    fn preflight(&self) -> Result<()> {
        let record = format!("preflight-{}", std::process::id());
        self.put(&record, b"preflight")?;
        if self.get(&record)? != b"preflight" {
            bail!("protected file credential preflight returned the wrong value");
        }
        self.delete(&record)
    }

    fn put(&self, record: &str, secret: &[u8]) -> Result<()> {
        write_secure_file(&self.root, &record_name(record), secret)
    }

    fn get(&self, record: &str) -> Result<Vec<u8>> {
        read_secure_file(&self.root, &record_name(record))
    }

    fn get_optional(&self, record: &str) -> Result<Option<Vec<u8>>> {
        let name = record_name(record);
        let path = self.root.join(&name);
        match fs::symlink_metadata(path) {
            Ok(_) => read_secure_file(&self.root, &name).map(Some),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn delete(&self, record: &str) -> Result<()> {
        let path = self.root.join(record_name(record));
        validate_secure_file(&path)?;
        fs::remove_file(path)?;
        Ok(())
    }

    fn delete_if_exists(&self, record: &str) -> Result<()> {
        let path = self.root.join(record_name(record));
        match fs::symlink_metadata(&path) {
            Ok(_) => self.delete(record),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

impl SelectedBackend {
    const fn as_str(self) -> &'static str {
        match self {
            Self::SecretService => "secret-service",
            Self::File => "file",
        }
    }
}

fn select_backend(
    mode: CredentialStorageMode,
    secret_service_available: bool,
) -> Result<SelectedBackend> {
    match (mode, secret_service_available) {
        (CredentialStorageMode::Auto | CredentialStorageMode::SecretService, true) => {
            Ok(SelectedBackend::SecretService)
        }
        (CredentialStorageMode::Auto | CredentialStorageMode::File, false)
        | (CredentialStorageMode::File, true) => Ok(SelectedBackend::File),
        (CredentialStorageMode::SecretService, false) => {
            bail!("Linux Secret Service is required but unavailable")
        }
    }
}

fn preflight_secret_service() -> Result<()> {
    let record = format!(
        "preflight-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    );
    let entry = secret_service_entry(&record)?;
    entry.set_secret(b"preflight")?;
    let read = entry.get_secret();
    let delete = entry.delete_credential();
    if read? != b"preflight" {
        bail!("Secret Service preflight returned the wrong value");
    }
    delete?;
    Ok(())
}

fn secret_service_entry(record: &str) -> Result<keyring::Entry> {
    keyring::Entry::new(SERVICE, record).map_err(Into::into)
}

fn record_name(record: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(record.as_bytes()))
}

#[cfg(unix)]
fn ensure_secure_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    if !path.exists() {
        fs::create_dir_all(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        bail!("credential path {} is not a directory", path.display());
    }
    if metadata.uid() != rustix::process::getuid().as_raw() {
        bail!(
            "credential directory {} has the wrong owner",
            path.display()
        );
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!(
            "credential directory {} must have mode 0700",
            path.display()
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_secure_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    Ok(())
}

fn write_secure_file(root: &Path, name: &str, value: &[u8]) -> Result<()> {
    ensure_secure_directory(root)?;
    let destination = root.join(name);
    if destination.exists() {
        validate_secure_file(&destination)?;
    }
    let temporary = root.join(format!(
        ".{name}.{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    ));
    let mut file = secure_create_new(&temporary)?;
    file.write_all(value)?;
    file.sync_all()?;
    fs::rename(&temporary, &destination)?;
    validate_secure_file(&destination)
}

fn read_secure_file(root: &Path, name: &str) -> Result<Vec<u8>> {
    ensure_secure_directory(root)?;
    let path = root.join(name);
    validate_secure_file(&path)?;
    let mut file = secure_open_read(&path)?;
    let mut value = Vec::new();
    file.read_to_end(&mut value)?;
    Ok(value)
}

#[cfg(unix)]
fn secure_create_new(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    Ok(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)?)
}

#[cfg(not(unix))]
fn secure_create_new(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().write(true).create_new(true).open(path)?)
}

#[cfg(unix)]
fn secure_open_read(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    Ok(OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32)
        .open(path)?)
}

#[cfg(not(unix))]
fn secure_open_read(path: &Path) -> Result<File> {
    Ok(File::open(path)?)
}

#[cfg(unix)]
fn validate_secure_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect credential file {}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        bail!("credential path {} is not a regular file", path.display());
    }
    if metadata.uid() != rustix::process::getuid().as_raw() {
        bail!("credential file {} has the wrong owner", path.display());
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("credential file {} must have mode 0600", path.display());
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_secure_file(path: &Path) -> Result<()> {
    if !path.is_file() {
        bail!("credential path {} is not a regular file", path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        CredentialStorageMode, CredentialStore, SelectedBackend, record_name, select_backend,
    };

    #[test]
    fn selects_explicit_and_fallback_backends() {
        assert_eq!(
            select_backend(CredentialStorageMode::Auto, true).unwrap(),
            SelectedBackend::SecretService
        );
        assert_eq!(
            select_backend(CredentialStorageMode::Auto, false).unwrap(),
            SelectedBackend::File
        );
        assert_eq!(
            select_backend(CredentialStorageMode::File, true).unwrap(),
            SelectedBackend::File
        );
        assert!(select_backend(CredentialStorageMode::SecretService, false).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn file_store_persists_backend_and_secret() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("identity");
        let store = CredentialStore::setup(CredentialStorageMode::File, &root).unwrap();
        store.put("issuer|gateway|session", b"secret").unwrap();

        let loaded = CredentialStore::load(&root).unwrap();
        assert_eq!(loaded.backend_name(), "file");
        assert_eq!(loaded.get("issuer|gateway|session").unwrap(), b"secret");
        loaded.delete("issuer|gateway|session").unwrap();
        assert!(loaded.get("issuer|gateway|session").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn file_store_rejects_broad_directory_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("identity");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();

        let error = CredentialStore::setup(CredentialStorageMode::File, &root).unwrap_err();
        assert!(error.to_string().contains("mode 0700"));
    }

    #[cfg(unix)]
    #[test]
    fn file_store_rejects_symlinked_record() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("identity");
        let store = CredentialStore::setup(CredentialStorageMode::File, &root).unwrap();
        store.put("session", b"secret").unwrap();
        let record = root.join("credentials").join(record_name("session"));
        std::fs::remove_file(&record).unwrap();
        symlink("/etc/passwd", &record).unwrap();

        let error = store.get("session").unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
    }
}
