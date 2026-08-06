use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use rcgen::{KeyPair, PKCS_ECDSA_P256_SHA256, PublicKeyData};
use sha2::{Digest, Sha256};
use x509_parser::parse_x509_certificate;
use x509_parser::pem::parse_x509_pem;

use super::client::{EnrollmentRecord, EnrollmentStatus};

#[derive(Clone)]
pub struct ClientIdentity {
    pub certificate_chain_pem: String,
    pub private_key_pem: String,
}

pub(super) fn validate_persisted_record(record: &EnrollmentRecord) -> Result<()> {
    let private_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&record.private_key_pkcs8)
        .context("decode enrollment private key")?;
    let key = KeyPair::try_from(private_key.as_slice()).context("parse enrollment private key")?;
    if key.algorithm() != &PKCS_ECDSA_P256_SHA256
        || public_key_fingerprint(&key) != record.public_key_fingerprint
    {
        bail!("persisted enrollment key does not match its fingerprint");
    }
    if let Some(certificate) = &record.certificate {
        let (_, pem) = parse_x509_pem(certificate.certificate_chain_pem.as_bytes())
            .map_err(|_| anyhow::anyhow!("issued certificate chain is not valid PEM"))?;
        if pem.label != "CERTIFICATE" {
            bail!("issued certificate chain does not begin with a certificate");
        }
        let (_, leaf) = parse_x509_certificate(&pem.contents)
            .map_err(|_| anyhow::anyhow!("issued leaf certificate is invalid"))?;
        if leaf.public_key().raw != key.subject_public_key_info() {
            bail!("issued certificate does not match the enrollment private key");
        }
    }
    Ok(())
}

pub(super) fn public_key_fingerprint(key: &KeyPair) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(key.subject_public_key_info()))
}

pub fn certificate_renewal_due(
    record: &EnrollmentRecord,
    now: SystemTime,
    renew_before: Duration,
) -> Result<bool> {
    let not_after = certificate_not_after(record)?;
    Ok(not_after
        .duration_since(now)
        .map_or(true, |remaining| remaining <= renew_before))
}

pub fn certificate_expired(record: &EnrollmentRecord, now: SystemTime) -> Result<bool> {
    Ok(certificate_not_after(record)? <= now)
}

fn certificate_not_after(record: &EnrollmentRecord) -> Result<SystemTime> {
    validate_persisted_record(record)?;
    let certificate = record
        .certificate
        .as_ref()
        .context("approved enrollment is missing its certificate")?;
    let (_, pem) = parse_x509_pem(certificate.certificate_chain_pem.as_bytes())
        .map_err(|_| anyhow::anyhow!("issued certificate chain is not valid PEM"))?;
    let (_, leaf) = parse_x509_certificate(&pem.contents)
        .map_err(|_| anyhow::anyhow!("issued leaf certificate is invalid"))?;
    let timestamp = u64::try_from(leaf.validity().not_after.timestamp())
        .context("issued certificate expiration precedes the Unix epoch")?;
    UNIX_EPOCH
        .checked_add(Duration::from_secs(timestamp))
        .context("issued certificate expiration is out of range")
}

pub(super) fn device_identity(record: &EnrollmentRecord) -> Result<reqwest::Identity> {
    let identity = client_identity(record)?;
    let identity_pem = format!(
        "{}{}",
        identity.certificate_chain_pem, identity.private_key_pem
    );
    Ok(reqwest::Identity::from_pem(identity_pem.as_bytes())?)
}

pub(super) fn client_identity(record: &EnrollmentRecord) -> Result<ClientIdentity> {
    if record.status != EnrollmentStatus::Approved {
        bail!("managed device enrollment is not approved");
    }
    let certificate = record
        .certificate
        .as_ref()
        .context("approved enrollment is missing its certificate")?;
    let private_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(&record.private_key_pkcs8)
        .context("decode enrollment private key")?;
    let key = KeyPair::try_from(private_key.as_slice()).context("parse enrollment private key")?;
    Ok(ClientIdentity {
        certificate_chain_pem: certificate.certificate_chain_pem.clone(),
        private_key_pem: key.serialize_pem(),
    })
}
