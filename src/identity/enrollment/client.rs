use super::certificate::{device_identity, public_key_fingerprint, validate_persisted_record};
use super::persistence::{
    load_enrollment_for, load_or_create_renewal_draft, renewal_record_name, save_enrollment_for,
};
use crate::identity::oauth::ManagedIdentity;
use crate::identity::storage::CredentialStore;
use anyhow::{Context, Result, bail};
use base64::Engine;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::pkcs8::DecodePrivateKey;
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum EnrollmentStatus {
    Pending,
    Issuing,
    Approved,
    Rejected,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct IssuedCertificate {
    pub certificate_chain_pem: String,
    pub serial_number: String,
    pub not_before: String,
    pub not_after: String,
}

#[derive(Clone, Deserialize, Eq, PartialEq)]
pub struct EnrollmentRecord {
    pub enrollment_id: String,
    pub status: EnrollmentStatus,
    pub public_key_fingerprint: String,
    pub created_at: String,
    pub device_id: Option<String>,
    pub certificate: Option<IssuedCertificate>,
    pub(super) private_key_pkcs8: String,
}

#[derive(Debug, Deserialize)]
struct AuthorityRecord {
    enrollment_id: String,
    status: EnrollmentStatus,
    public_key_fingerprint: String,
    created_at: String,
    device_id: Option<String>,
    certificate: Option<IssuedCertificate>,
}

#[derive(Debug, Deserialize)]
struct AuthorityRenewal {
    renewal_id: String,
    status: EnrollmentStatus,
    device_id: String,
    public_key_fingerprint: String,
    certificate: IssuedCertificate,
}

#[derive(Debug, Deserialize)]
struct AuthorityRecoveryChallenge {
    challenge_id: String,
    device_id: String,
    public_key_fingerprint: String,
    nonce: String,
    expires_at: String,
}

pub struct EnrollmentClient {
    client: Client,
    endpoint: Url,
    renewal_endpoint: Url,
    recovery_challenge_endpoint: Url,
    recovery_endpoint: Url,
}

impl EnrollmentClient {
    pub fn new(service_url: &Url) -> Result<Self> {
        if service_url.scheme() != "https"
            || service_url.host_str().is_none()
            || service_url.path() != "/"
            || service_url.query().is_some()
            || service_url.fragment().is_some()
        {
            bail!("enrollment service URL must be an HTTPS origin");
        }
        Ok(Self {
            client: Client::new(),
            endpoint: service_url.join("v1/enrollments")?,
            renewal_endpoint: service_url.join("v1/renewals")?,
            recovery_challenge_endpoint: service_url.join("v1/recovery/challenges")?,
            recovery_endpoint: service_url.join("v1/recovery")?,
        })
    }

    pub async fn request(&self, identity: &ManagedIdentity) -> Result<EnrollmentRecord> {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let fingerprint = public_key_fingerprint(&key);
        let csr = CertificateParams::default()
            .serialize_request(&key)?
            .pem()?;
        let response = self
            .client
            .post(self.endpoint.clone())
            .bearer_auth(identity.bearer_token().await?)
            .json(&serde_json::json!({"csr": csr}))
            .send()
            .await?;
        if response.status() != StatusCode::ACCEPTED {
            bail!(
                "enrollment request failed with status {}",
                response.status()
            );
        }
        let authority: AuthorityRecord = response.json().await?;
        validate_authority_record(&authority, Some(EnrollmentStatus::Pending), &fingerprint)?;
        Ok(authority.with_key(&key))
    }

    pub async fn status(
        &self,
        identity: &ManagedIdentity,
        enrollment: &EnrollmentRecord,
    ) -> Result<EnrollmentRecord> {
        validate_persisted_record(enrollment)?;
        let mut endpoint = self.endpoint.clone();
        endpoint
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("enrollment endpoint cannot be a base URL"))?
            .push(&enrollment.enrollment_id);
        let authority: AuthorityRecord = self
            .client
            .get(endpoint)
            .bearer_auth(identity.bearer_token().await?)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        validate_authority_record(&authority, None, &enrollment.public_key_fingerprint)?;
        let updated = authority.with_private_key(enrollment.private_key_pkcs8.clone());
        validate_persisted_record(&updated)?;
        Ok(updated)
    }

    pub async fn renew_and_save(
        &self,
        identity: &ManagedIdentity,
        issuer: &Url,
        gateway_origin: &Url,
        store: &CredentialStore,
    ) -> Result<EnrollmentRecord> {
        let enrollment = load_enrollment_for(issuer, gateway_origin, store)?;
        validate_persisted_record(&enrollment)?;
        if enrollment.status != EnrollmentStatus::Approved {
            bail!("only an approved device enrollment can be renewed");
        }
        let current_device_id = enrollment
            .device_id
            .as_deref()
            .context("approved enrollment is missing its device ID")?;
        let draft = load_or_create_renewal_draft(issuer, gateway_origin, store, current_device_id)?;
        let private_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&draft.private_key_pkcs8)
            .context("decode renewal private key")?;
        let key = KeyPair::try_from(private_key.as_slice()).context("parse renewal private key")?;
        let csr = CertificateParams::default()
            .serialize_request(&key)?
            .pem()?;
        let client = Client::builder()
            .identity(device_identity(&enrollment)?)
            .build()?;
        let response = client
            .post(self.renewal_endpoint.clone())
            .bearer_auth(identity.bearer_token().await?)
            .json(&serde_json::json!({"csr": csr}))
            .send()
            .await?;
        if response.status() != StatusCode::OK {
            bail!(
                "certificate renewal failed with status {}",
                response.status()
            );
        }
        let authority: AuthorityRenewal = response.json().await?;
        if authority.renewal_id.is_empty()
            || authority.status != EnrollmentStatus::Approved
            || authority.device_id != current_device_id
            || authority.public_key_fingerprint != draft.public_key_fingerprint
        {
            bail!("renewal authority returned an incomplete or mismatched record");
        }
        let renewed = EnrollmentRecord {
            enrollment_id: enrollment.enrollment_id.clone(),
            status: EnrollmentStatus::Approved,
            public_key_fingerprint: draft.public_key_fingerprint,
            created_at: enrollment.created_at.clone(),
            device_id: Some(authority.device_id),
            certificate: Some(authority.certificate),
            private_key_pkcs8: draft.private_key_pkcs8,
        };
        validate_persisted_record(&renewed)?;
        save_enrollment_for(issuer, gateway_origin, store, &renewed)?;
        store.delete_if_exists(&renewal_record_name(issuer, gateway_origin))?;
        Ok(renewed)
    }

    pub async fn recover_and_save(
        &self,
        identity: &ManagedIdentity,
        issuer: &Url,
        gateway_origin: &Url,
        store: &CredentialStore,
    ) -> Result<EnrollmentRecord> {
        let enrollment = load_enrollment_for(issuer, gateway_origin, store)?;
        validate_persisted_record(&enrollment)?;
        if enrollment.status != EnrollmentStatus::Approved {
            bail!("only an approved device enrollment can be recovered");
        }
        let device_id = enrollment
            .device_id
            .as_deref()
            .context("approved enrollment is missing its device ID")?;
        let serial = enrollment
            .certificate
            .as_ref()
            .context("approved enrollment is missing its certificate")?
            .serial_number
            .as_str();
        let draft = load_or_create_renewal_draft(issuer, gateway_origin, store, device_id)?;
        let replacement_der = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&draft.private_key_pkcs8)
            .context("decode recovery private key")?;
        let replacement_key =
            KeyPair::try_from(replacement_der.as_slice()).context("parse recovery private key")?;
        let csr = CertificateParams::default()
            .serialize_request(&replacement_key)?
            .pem()?;
        let bearer = identity.bearer_token().await?;
        let response = self
            .client
            .post(self.recovery_challenge_endpoint.clone())
            .bearer_auth(&bearer)
            .json(&serde_json::json!({
                "device_id": device_id,
                "presented_serial_number": serial,
                "csr": csr,
            }))
            .send()
            .await?;
        if response.status() != StatusCode::CREATED {
            bail!(
                "certificate recovery challenge failed with status {}",
                response.status()
            );
        }
        let challenge: AuthorityRecoveryChallenge = response.json().await?;
        if challenge.challenge_id.is_empty()
            || challenge.device_id != device_id
            || challenge.public_key_fingerprint != draft.public_key_fingerprint
            || challenge.expires_at.is_empty()
        {
            bail!("recovery authority returned an incomplete or mismatched challenge");
        }
        let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&challenge.nonce)
            .context("decode recovery nonce")?;
        if nonce.len() != 32 {
            bail!("recovery authority returned an invalid nonce");
        }
        let enrolled_der = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&enrollment.private_key_pkcs8)
            .context("decode enrolled private key")?;
        let signing_key =
            SigningKey::from_pkcs8_der(&enrolled_der).context("parse enrolled private key")?;
        let message = format!(
            "agentdesktop-device-recovery-v1\n{}\n{}\n{}",
            challenge.challenge_id, challenge.nonce, challenge.public_key_fingerprint
        );
        let signature: Signature = signing_key.sign(message.as_bytes());
        let proof =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_der().as_bytes());
        let response = self
            .client
            .post(self.recovery_endpoint.clone())
            .bearer_auth(bearer)
            .json(&serde_json::json!({
                "challenge_id": challenge.challenge_id,
                "proof": proof,
            }))
            .send()
            .await?;
        if response.status() != StatusCode::OK {
            bail!(
                "certificate recovery failed with status {}",
                response.status()
            );
        }
        let authority: AuthorityRenewal = response.json().await?;
        if authority.renewal_id.is_empty()
            || authority.status != EnrollmentStatus::Approved
            || authority.device_id != device_id
            || authority.public_key_fingerprint != draft.public_key_fingerprint
        {
            bail!("recovery authority returned an incomplete or mismatched record");
        }
        let recovered = EnrollmentRecord {
            enrollment_id: enrollment.enrollment_id.clone(),
            status: EnrollmentStatus::Approved,
            public_key_fingerprint: draft.public_key_fingerprint,
            created_at: enrollment.created_at.clone(),
            device_id: Some(authority.device_id),
            certificate: Some(authority.certificate),
            private_key_pkcs8: draft.private_key_pkcs8,
        };
        validate_persisted_record(&recovered)?;
        save_enrollment_for(issuer, gateway_origin, store, &recovered)?;
        store.delete_if_exists(&renewal_record_name(issuer, gateway_origin))?;
        Ok(recovered)
    }
}

impl AuthorityRecord {
    fn with_key(self, key: &KeyPair) -> EnrollmentRecord {
        self.with_private_key(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key.serialize_der()),
        )
    }

    fn with_private_key(self, private_key_pkcs8: String) -> EnrollmentRecord {
        EnrollmentRecord {
            enrollment_id: self.enrollment_id,
            status: self.status,
            public_key_fingerprint: self.public_key_fingerprint,
            created_at: self.created_at,
            device_id: self.device_id,
            certificate: self.certificate,
            private_key_pkcs8,
        }
    }
}

fn validate_authority_record(
    record: &AuthorityRecord,
    expected: Option<EnrollmentStatus>,
    fingerprint: &str,
) -> Result<()> {
    if record.enrollment_id.is_empty()
        || record.public_key_fingerprint != fingerprint
        || record.created_at.is_empty()
    {
        bail!("enrollment authority returned an incomplete or mismatched record");
    }
    if expected.is_some_and(|expected| record.status != expected) {
        bail!("enrollment authority returned an unexpected status");
    }
    match record.status {
        EnrollmentStatus::Pending | EnrollmentStatus::Issuing
            if record.device_id.is_some() || record.certificate.is_some() =>
        {
            bail!("incomplete enrollment must not contain a device credential")
        }
        EnrollmentStatus::Approved
            if record.device_id.as_deref().is_none_or(str::is_empty)
                || record.certificate.is_none() =>
        {
            bail!("approved enrollment is missing its device credential")
        }
        EnrollmentStatus::Rejected
            if record.device_id.is_some() || record.certificate.is_some() =>
        {
            bail!("rejected enrollment must not contain a device credential")
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use base64::Engine;
    use p256::ecdsa::signature::Verifier;
    use p256::ecdsa::{Signature as P256Signature, VerifyingKey};
    use rcgen::{
        BasicConstraints, CertificateParams, CertificateSigningRequestParams, CertifiedIssuer,
        IsCa, KeyPair, PKCS_ECDSA_P256_SHA256, PublicKeyData,
    };
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use tokio::net::TcpListener;
    use url::Url;
    use x509_parser::parse_x509_certificate;
    use x509_parser::pem::parse_x509_pem;

    use super::{EnrollmentClient, EnrollmentStatus};
    use crate::identity::enrollment::{
        certificate_renewal_due, load_device_identity_for, load_enrollment_for, save_enrollment_for,
    };
    use crate::identity::oauth::{ManagedIdentity, StoredSession};
    use crate::identity::storage::{CredentialStorageMode, CredentialStore};

    struct AuthorityState {
        fingerprint: tokio::sync::Mutex<String>,
        certificate: tokio::sync::Mutex<String>,
        recovery_fingerprint: tokio::sync::Mutex<String>,
        recovery_certificate: tokio::sync::Mutex<String>,
    }

    fn fixture_client(service_url: &Url) -> EnrollmentClient {
        EnrollmentClient {
            client: reqwest::Client::new(),
            endpoint: service_url.join("v1/enrollments").unwrap(),
            renewal_endpoint: service_url.join("v1/renewals").unwrap(),
            recovery_challenge_endpoint: service_url.join("v1/recovery/challenges").unwrap(),
            recovery_endpoint: service_url.join("v1/recovery").unwrap(),
        }
    }

    #[test]
    fn client_requires_https_origin() {
        assert!(EnrollmentClient::new(&Url::parse("https://authority.example/").unwrap()).is_ok());
        for invalid in [
            "http://authority.example/",
            "https://authority.example/path",
            "https://authority.example/?query=value",
            "https://authority.example/#fragment",
        ] {
            assert!(
                EnrollmentClient::new(&Url::parse(invalid).unwrap()).is_err(),
                "accepted invalid enrollment service URL: {invalid}"
            );
        }
    }

    #[tokio::test]
    async fn requests_csr_and_installs_matching_certificate() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let service_url =
            Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let state = Arc::new(AuthorityState {
            fingerprint: tokio::sync::Mutex::new(String::new()),
            certificate: tokio::sync::Mutex::new(String::new()),
            recovery_fingerprint: tokio::sync::Mutex::new(String::new()),
            recovery_certificate: tokio::sync::Mutex::new(String::new()),
        });
        let app = Router::new()
            .route("/v1/enrollments", post(request_enrollment))
            .route("/v1/enrollments/{id}", get(enrollment_status))
            .route("/v1/renewals", post(renew_certificate))
            .route("/v1/recovery/challenges", post(recovery_challenge))
            .route("/v1/recovery", post(recover_certificate))
            .with_state(state);
        let server = tokio::spawn(axum::serve(listener, app).into_future());
        let temporary = tempfile::tempdir().unwrap();
        let store = CredentialStore::setup(
            CredentialStorageMode::File,
            &temporary.path().join("identity"),
        )
        .unwrap();
        let identity = test_identity(&store);
        let client = fixture_client(&service_url);

        let pending = client.request(&identity).await.unwrap();
        assert_eq!(pending.status, EnrollmentStatus::Pending);
        save_enrollment_for(
            &Url::parse("https://issuer.example/").unwrap(),
            &Url::parse("https://gateway.example/").unwrap(),
            &store,
            &pending,
        )
        .unwrap();
        let approved = client.status(&identity, &pending).await.unwrap();
        assert_eq!(approved.status, EnrollmentStatus::Approved);
        assert_eq!(approved.device_id.as_deref(), Some("device-1"));
        assert!(
            !certificate_renewal_due(&approved, UNIX_EPOCH, std::time::Duration::ZERO,).unwrap()
        );
        assert!(
            certificate_renewal_due(
                &approved,
                UNIX_EPOCH + std::time::Duration::from_secs(100_000_000_000),
                std::time::Duration::ZERO,
            )
            .unwrap()
        );
        save_enrollment_for(
            &Url::parse("https://issuer.example/").unwrap(),
            &Url::parse("https://gateway.example/").unwrap(),
            &store,
            &approved,
        )
        .unwrap();
        load_device_identity_for(
            &Url::parse("https://issuer.example/").unwrap(),
            &Url::parse("https://gateway.example/").unwrap(),
            &store,
        )
        .unwrap();

        let recovered = client
            .recover_and_save(
                &identity,
                &Url::parse("https://issuer.example/").unwrap(),
                &Url::parse("https://gateway.example/").unwrap(),
                &store,
            )
            .await
            .unwrap();
        assert_eq!(recovered.status, EnrollmentStatus::Approved);
        assert_eq!(recovered.device_id.as_deref(), Some("device-1"));
        assert_ne!(
            recovered.public_key_fingerprint,
            approved.public_key_fingerprint
        );

        let renewed = client
            .renew_and_save(
                &identity,
                &Url::parse("https://issuer.example/").unwrap(),
                &Url::parse("https://gateway.example/").unwrap(),
                &store,
            )
            .await
            .unwrap();
        assert_eq!(renewed.status, EnrollmentStatus::Approved);
        assert_eq!(renewed.device_id.as_deref(), Some("device-1"));
        assert_ne!(
            renewed.public_key_fingerprint,
            approved.public_key_fingerprint
        );
        let persisted = load_enrollment_for(
            &Url::parse("https://issuer.example/").unwrap(),
            &Url::parse("https://gateway.example/").unwrap(),
            &store,
        )
        .unwrap();
        assert_eq!(
            persisted.public_key_fingerprint,
            renewed.public_key_fingerprint
        );
        load_device_identity_for(
            &Url::parse("https://issuer.example/").unwrap(),
            &Url::parse("https://gateway.example/").unwrap(),
            &store,
        )
        .unwrap();

        server.abort();
    }

    #[test]
    fn rejects_persisted_key_mismatch() {
        let temporary = tempfile::tempdir().unwrap();
        let store = CredentialStore::setup(
            CredentialStorageMode::File,
            &temporary.path().join("identity"),
        )
        .unwrap();
        let issuer = Url::parse("https://issuer.example/").unwrap();
        let gateway = Url::parse("https://gateway.example/").unwrap();
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let record = super::EnrollmentRecord {
            enrollment_id: "enrollment-1".into(),
            status: EnrollmentStatus::Pending,
            public_key_fingerprint: "attacker-fingerprint".into(),
            created_at: "2026-08-04T00:00:00Z".into(),
            device_id: None,
            certificate: None,
            private_key_pkcs8: base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(key.serialize_der()),
        };
        assert!(save_enrollment_for(&issuer, &gateway, &store, &record).is_err());
        assert!(load_enrollment_for(&issuer, &gateway, &store).is_err());
    }

    #[test]
    fn reuses_protected_renewal_draft() {
        let temporary = tempfile::tempdir().unwrap();
        let store = CredentialStore::setup(
            CredentialStorageMode::File,
            &temporary.path().join("identity"),
        )
        .unwrap();
        let issuer = Url::parse("https://issuer.example/").unwrap();
        let gateway = Url::parse("https://gateway.example/").unwrap();

        let first =
            super::load_or_create_renewal_draft(&issuer, &gateway, &store, "device-1").unwrap();
        let retried =
            super::load_or_create_renewal_draft(&issuer, &gateway, &store, "device-1").unwrap();

        assert_eq!(first.public_key_fingerprint, retried.public_key_fingerprint);
        assert_eq!(first.private_key_pkcs8, retried.private_key_pkcs8);
    }

    async fn request_enrollment(
        State(state): State<Arc<AuthorityState>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        assert_eq!(headers["authorization"], "Bearer access-token");
        let csr = body["csr"].as_str().unwrap();
        let request = CertificateSigningRequestParams::from_pem(csr).unwrap();
        let fingerprint = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(request.public_key.subject_public_key_info()));
        let certificate = issue_for_csr(&request);
        *state.fingerprint.lock().await = fingerprint.clone();
        *state.certificate.lock().await = certificate;
        (
            StatusCode::ACCEPTED,
            Json(json!({
                "enrollment_id": "enrollment-1",
                "status": "pending",
                "public_key_fingerprint": fingerprint,
                "created_at": "2026-08-04T00:00:00Z"
            })),
        )
    }

    async fn enrollment_status(
        Path(id): Path<String>,
        State(state): State<Arc<AuthorityState>>,
        headers: HeaderMap,
    ) -> Json<Value> {
        assert_eq!(id, "enrollment-1");
        assert_eq!(headers["authorization"], "Bearer access-token");
        Json(json!({
            "enrollment_id": id,
            "status": "approved",
            "public_key_fingerprint": state.fingerprint.lock().await.clone(),
            "created_at": "2026-08-04T00:00:00Z",
            "device_id": "device-1",
            "certificate": {
                "certificate_chain_pem": state.certificate.lock().await.clone(),
                "serial_number": "01",
                "not_before": "2026-08-04T00:00:00Z",
                "not_after": "2026-08-05T00:00:00Z"
            }
        }))
    }

    async fn renew_certificate(headers: HeaderMap, Json(body): Json<Value>) -> Json<Value> {
        assert_eq!(headers["authorization"], "Bearer access-token");
        let csr = body["csr"].as_str().unwrap();
        let request = CertificateSigningRequestParams::from_pem(csr).unwrap();
        let fingerprint = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(request.public_key.subject_public_key_info()));
        Json(json!({
            "renewal_id": "renewal-1",
            "status": "approved",
            "device_id": "device-1",
            "public_key_fingerprint": fingerprint,
            "certificate": {
                "certificate_chain_pem": issue_for_csr(&request),
                "serial_number": "02",
                "not_before": "2026-08-05T00:00:00Z",
                "not_after": "2026-08-06T00:00:00Z"
            }
        }))
    }

    async fn recovery_challenge(
        State(state): State<Arc<AuthorityState>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        assert_eq!(headers["authorization"], "Bearer access-token");
        assert_eq!(body["device_id"], "device-1");
        assert_eq!(body["presented_serial_number"], "01");
        let request =
            CertificateSigningRequestParams::from_pem(body["csr"].as_str().unwrap()).unwrap();
        let fingerprint = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(request.public_key.subject_public_key_info()));
        *state.recovery_fingerprint.lock().await = fingerprint.clone();
        *state.recovery_certificate.lock().await = issue_for_csr(&request);
        (
            StatusCode::CREATED,
            Json(json!({
                "challenge_id": "challenge-1",
                "device_id": "device-1",
                "public_key_fingerprint": fingerprint,
                "nonce": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7_u8; 32]),
                "expires_at": "2026-08-05T00:05:00Z"
            })),
        )
    }

    async fn recover_certificate(
        State(state): State<Arc<AuthorityState>>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        assert_eq!(headers["authorization"], "Bearer access-token");
        assert_eq!(body["challenge_id"], "challenge-1");
        let certificate = state.certificate.lock().await.clone();
        let (_, pem) = parse_x509_pem(certificate.as_bytes()).unwrap();
        let (_, leaf) = parse_x509_certificate(&pem.contents).unwrap();
        let verifying_key =
            VerifyingKey::from_sec1_bytes(leaf.public_key().subject_public_key.data.as_ref())
                .unwrap();
        let proof = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(body["proof"].as_str().unwrap())
            .unwrap();
        let signature = P256Signature::from_der(&proof).unwrap();
        let fingerprint = state.recovery_fingerprint.lock().await.clone();
        let nonce = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7_u8; 32]);
        let message =
            format!("agentdesktop-device-recovery-v1\nchallenge-1\n{nonce}\n{fingerprint}");
        verifying_key
            .verify(message.as_bytes(), &signature)
            .unwrap();
        let certificate = state.recovery_certificate.lock().await.clone();
        *state.certificate.lock().await = certificate.clone();
        Json(json!({
            "renewal_id": "recovery-renewal-1",
            "status": "approved",
            "device_id": "device-1",
            "public_key_fingerprint": fingerprint,
            "certificate": {
                "certificate_chain_pem": certificate,
                "serial_number": "02",
                "not_before": "2026-08-05T00:00:00Z",
                "not_after": "2026-08-06T00:00:00Z"
            }
        }))
    }

    fn issue_for_csr(request: &CertificateSigningRequestParams) -> String {
        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();
        request.signed_by(&ca).unwrap().pem()
    }

    fn test_identity(store: &CredentialStore) -> ManagedIdentity {
        ManagedIdentity::new(
            StoredSession {
                issuer: Url::parse("https://issuer.example/").unwrap(),
                gateway_origin: Url::parse("https://gateway.example/").unwrap(),
                client_id: "client".into(),
                audience: "gateway".into(),
                access_token: "access-token".into(),
                expires_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 300,
                scope: "agentgateway.invoke".into(),
                refresh_token: "refresh-token".into(),
                generation: 1,
            },
            store.clone(),
        )
    }
}
