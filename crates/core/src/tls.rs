use std::{path::Path, sync::Arc};

use anyhow::Context;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};

/// Server TLS identity with an explicit crypto provider, so a listener does not depend on
/// which rustls backends the final binary happens to enable. `purpose` labels errors.
pub fn server_config(
    certificate: &Path,
    key: &Path,
    purpose: &str,
) -> anyhow::Result<rustls::ServerConfig> {
    let certificates = CertificateDer::pem_file_iter(certificate)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .with_context(|| format!("read {purpose} TLS certificate {}", certificate.display()))?;
    anyhow::ensure!(
        !certificates.is_empty(),
        "{purpose} TLS certificate {} contains no certificates",
        certificate.display()
    );
    let key = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("read {purpose} TLS key {}", key.display()))?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .with_context(|| format!("load {purpose} TLS identity"))?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::server_config;

    #[test]
    fn loads_a_pem_identity_and_labels_errors_with_the_purpose() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("agentdesktop-tls-{}-{unique}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let certificate = directory.join("certificate.pem");
        let key = directory.join("key.pem");
        let empty = directory.join("empty.pem");
        std::fs::write(&certificate, cert.pem()).unwrap();
        std::fs::write(&key, signing_key.serialize_pem()).unwrap();
        std::fs::write(&empty, "").unwrap();

        let config = server_config(&certificate, &key, "test").expect("load identity");
        assert_eq!(
            config.alpn_protocols,
            [b"h2".to_vec(), b"http/1.1".to_vec()]
        );

        let error = server_config(&empty, &key, "test").unwrap_err();
        assert!(
            error.to_string().starts_with("test TLS certificate")
                && error.to_string().ends_with("contains no certificates"),
            "{error}"
        );
        let error =
            server_config(&certificate, &directory.join("missing.pem"), "test").unwrap_err();
        assert!(
            error.to_string().starts_with("read test TLS key"),
            "{error}"
        );

        std::fs::remove_dir_all(directory).unwrap();
    }
}
