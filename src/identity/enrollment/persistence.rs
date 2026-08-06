use anyhow::{Context, Result, bail};
use base64::Engine;
use rcgen::{KeyPair, PKCS_ECDSA_P256_SHA256};
use serde::{Deserialize, Serialize};
use url::Url;

use super::certificate::{
    ClientIdentity, client_identity, device_identity, public_key_fingerprint,
    validate_persisted_record,
};
use super::client::{EnrollmentRecord, EnrollmentStatus, IssuedCertificate};
use crate::identity::storage::CredentialStore;

#[derive(Serialize)]
struct PersistedEnrollment<'a> {
    enrollment_id: &'a str,
    status: EnrollmentStatus,
    public_key_fingerprint: &'a str,
    created_at: &'a str,
    device_id: Option<&'a str>,
    certificate: Option<&'a IssuedCertificate>,
    private_key_pkcs8: &'a str,
}

#[derive(Deserialize, Serialize)]
pub(super) struct RenewalDraft {
    pub device_id: String,
    pub public_key_fingerprint: String,
    pub private_key_pkcs8: String,
}

pub fn save_enrollment_for(
    issuer: &Url,
    gateway_origin: &Url,
    store: &CredentialStore,
    record: &EnrollmentRecord,
) -> Result<()> {
    validate_persisted_record(record)?;
    let persisted = PersistedEnrollment {
        enrollment_id: &record.enrollment_id,
        status: record.status,
        public_key_fingerprint: &record.public_key_fingerprint,
        created_at: &record.created_at,
        device_id: record.device_id.as_deref(),
        certificate: record.certificate.as_ref(),
        private_key_pkcs8: &record.private_key_pkcs8,
    };
    store.put(
        &enrollment_record_name(issuer, gateway_origin),
        &serde_json::to_vec(&persisted)?,
    )
}

pub fn load_enrollment_for(
    issuer: &Url,
    gateway_origin: &Url,
    store: &CredentialStore,
) -> Result<EnrollmentRecord> {
    let record: EnrollmentRecord =
        serde_json::from_slice(&store.get(&enrollment_record_name(issuer, gateway_origin))?)?;
    validate_persisted_record(&record)?;
    Ok(record)
}

pub fn load_device_identity_for(
    issuer: &Url,
    gateway_origin: &Url,
    store: &CredentialStore,
) -> Result<reqwest::Identity> {
    let record = load_enrollment_for(issuer, gateway_origin, store)?;
    device_identity(&record)
}

pub fn load_client_identity_for(
    issuer: &Url,
    gateway_origin: &Url,
    store: &CredentialStore,
) -> Result<ClientIdentity> {
    let record = load_enrollment_for(issuer, gateway_origin, store)?;
    client_identity(&record)
}

pub fn delete_enrollment_for(
    issuer: &Url,
    gateway_origin: &Url,
    store: &CredentialStore,
) -> Result<()> {
    store.delete_if_exists(&enrollment_record_name(issuer, gateway_origin))
}

fn enrollment_record_name(issuer: &Url, gateway_origin: &Url) -> String {
    format!("enrollment-mtls-v1|{issuer}|{gateway_origin}")
}

pub(super) fn renewal_record_name(issuer: &Url, gateway_origin: &Url) -> String {
    format!("enrollment-mtls-renewal-v1|{issuer}|{gateway_origin}")
}

pub(super) fn load_or_create_renewal_draft(
    issuer: &Url,
    gateway_origin: &Url,
    store: &CredentialStore,
    device_id: &str,
) -> Result<RenewalDraft> {
    let record_name = renewal_record_name(issuer, gateway_origin);
    if let Some(encoded) = store.get_optional(&record_name)? {
        let draft: RenewalDraft = serde_json::from_slice(&encoded)?;
        let private_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&draft.private_key_pkcs8)
            .context("decode renewal private key")?;
        let key = KeyPair::try_from(private_key.as_slice()).context("parse renewal private key")?;
        if draft.device_id != device_id
            || key.algorithm() != &PKCS_ECDSA_P256_SHA256
            || public_key_fingerprint(&key) != draft.public_key_fingerprint
        {
            bail!("persisted renewal draft does not match the approved device");
        }
        return Ok(draft);
    }
    let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
    let draft = RenewalDraft {
        device_id: device_id.to_owned(),
        public_key_fingerprint: public_key_fingerprint(&key),
        private_key_pkcs8: base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(key.serialize_der()),
    };
    store.put(&record_name, &serde_json::to_vec(&draft)?)?;
    Ok(draft)
}
