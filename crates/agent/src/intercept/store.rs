use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::Context;
use sha2::{Digest, Sha256};

use super::InterceptCa;
use crate::{secret_store::SecretStore, secure_fs};

const SECRET_SERVICE: &str = "dev.agentdesktop.intercept-ca";
const SECRET_ACCOUNT: &str = "key";
/// The CA certificate, for an administrator or an MDM profile to trust.
const CERTIFICATE_FILE: &str = "intercept-ca.pem";
/// The hosts the stored CA was generated for.
const HOSTS_FILE: &str = "intercept-ca-hosts.json";

/// Where the intercept CA certificate is written.
pub fn certificate_path(state_dir: &Path) -> PathBuf {
    state_dir.join(CERTIFICATE_FILE)
}

/// The stored intercept CA for `hosts`, or a new one. The key is kept in the
/// daemon's secret store (the System keychain for the system daemon on macOS)
/// and the certificate in the state directory. A CA made for other hosts is
/// replaced, because its Name Constraints would not cover them.
pub(super) fn load_or_create(
    state_dir: &Path,
    hosts: &[String],
) -> anyhow::Result<Arc<InterceptCa>> {
    let store = SecretStore::new(state_dir)?;
    load_or_create_with(
        state_dir,
        hosts,
        || store.get_optional(SECRET_SERVICE, SECRET_ACCOUNT),
        |key| store.set(SECRET_SERVICE, SECRET_ACCOUNT, key),
    )
}

fn load_or_create_with(
    state_dir: &Path,
    hosts: &[String],
    read_key: impl FnOnce() -> anyhow::Result<Option<String>>,
    write_key: impl FnOnce(&str) -> anyhow::Result<()>,
) -> anyhow::Result<Arc<InterceptCa>> {
    let certificate_path = certificate_path(state_dir);
    let hosts_path = state_dir.join(HOSTS_FILE);
    let stored_hosts = std::fs::read(&hosts_path)
        .ok()
        .and_then(|contents| serde_json::from_slice::<Vec<String>>(&contents).ok());
    let stored_certificate = std::fs::read_to_string(&certificate_path).ok();
    if let (Some(stored_hosts), Some(certificate)) = (&stored_hosts, &stored_certificate)
        && stored_hosts == hosts
        && let Some(key) = read_key().context("read the intercept CA key")?
    {
        match InterceptCa::from_pem(certificate, &key, hosts) {
            Ok(ca) => return Ok(Arc::new(ca)),
            Err(error) => tracing::warn!(
                error = %format!("{error:#}"),
                "the stored intercept CA could not be loaded; generating a new one"
            ),
        }
    }

    let ca = InterceptCa::generate(&common_name(), hosts)?;
    secure_fs::ensure_private_dir(state_dir)?;
    // The key first: a certificate on disk whose key was never stored would be
    // replaced at the next start anyway, but a stored key with no certificate
    // costs nothing.
    write_key(ca.key_pem()).context("store the intercept CA key")?;
    secure_fs::atomic_write(&certificate_path, ca.certificate_pem().as_bytes(), 0o644)
        .with_context(|| format!("write {}", certificate_path.display()))?;
    secure_fs::atomic_write(&hosts_path, &serde_json::to_vec(hosts)?, 0o644)
        .with_context(|| format!("write {}", hosts_path.display()))?;
    tracing::warn!(
        certificate = %certificate_path.display(),
        sha256 = %fingerprint(ca.certificate_der()),
        hosts = ?hosts,
        "generated a new intercept CA: Claude Desktop refuses the intercepted hosts until this certificate is a trusted root on the device (System keychain or an MDM profile) and Claude Desktop restarts"
    );
    Ok(Arc::new(ca))
}

/// The certificate's SHA-256 fingerprint, as `AB:CD:…`.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn common_name() -> String {
    let host = hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .unwrap_or_else(|| "this device".to_owned());
    format!("agentdesktop intercept CA ({host})")
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn hosts(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn the_ca_is_reused_for_the_same_hosts_and_replaced_for_others() {
        let dir = tempfile::tempdir().unwrap();
        let key: RefCell<Option<String>> = RefCell::default();
        let load = |names: &[&str]| {
            load_or_create_with(
                dir.path(),
                &hosts(names),
                || Ok(key.borrow().clone()),
                |value| {
                    *key.borrow_mut() = Some(value.to_owned());
                    Ok(())
                },
            )
            .unwrap()
        };
        let first = load(&["claude.ai", "api.anthropic.com"]);
        let again = load(&["claude.ai", "api.anthropic.com"]);
        assert_eq!(first.certificate_der(), again.certificate_der());
        let other = load(&["claude.ai"]);
        assert_ne!(first.certificate_der(), other.certificate_der());
        assert_eq!(
            std::fs::read_to_string(certificate_path(dir.path())).unwrap(),
            other.certificate_pem()
        );
    }

    #[test]
    fn a_missing_key_replaces_the_ca() {
        let dir = tempfile::tempdir().unwrap();
        let names = hosts(&["claude.ai"]);
        let first = load_or_create_with(dir.path(), &names, || Ok(None), |_| Ok(())).unwrap();
        let second = load_or_create_with(dir.path(), &names, || Ok(None), |_| Ok(())).unwrap();
        assert_ne!(first.certificate_der(), second.certificate_der());
    }

    #[test]
    fn fingerprints_are_colon_separated_hex() {
        assert_eq!(fingerprint(b"").len(), 32 * 3 - 1);
        assert!(fingerprint(b"x").starts_with("2D:71:16:42"));
    }
}
