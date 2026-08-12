use std::path::PathBuf;

use anyhow::{Context, Result};
use rustls::RootCertStore;

const SSL_CERT_FILE_ENV: &str = "SSL_CERT_FILE";

pub fn http_client() -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    if let Some(bundle) = process_bundle()? {
        for certificate in reqwest::Certificate::from_pem_bundle(&bundle)? {
            builder = builder.add_root_certificate(certificate);
        }
    }
    Ok(builder.build()?)
}

pub fn root_store() -> Result<RootCertStore> {
    let native = rustls_native_certs::load_native_certs();
    if !native.errors.is_empty() {
        tracing::warn!(
            event = "native_ca_load_incomplete",
            errors = native.errors.len()
        );
    }
    let mut roots = RootCertStore::empty();
    for certificate in native.certs {
        roots.add(certificate)?;
    }
    if let Some(bundle) = process_bundle()? {
        for certificate in rustls_pemfile::certs(&mut bundle.as_slice()) {
            roots.add(certificate?)?;
        }
    }
    Ok(roots)
}

fn process_bundle() -> Result<Option<Vec<u8>>> {
    let Some(path) = std::env::var_os(SSL_CERT_FILE_ENV).filter(|path| !path.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(path);
    std::fs::read(&path)
        .with_context(|| format!("read process CA bundle {}", path.display()))
        .map(Some)
}
