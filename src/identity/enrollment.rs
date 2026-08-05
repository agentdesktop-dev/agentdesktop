use anyhow::{Context, Result, bail};
use base64::Engine;
use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256, PublicKeyData};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;
use x509_parser::parse_x509_certificate;
use x509_parser::pem::parse_x509_pem;

use super::oauth::ManagedIdentity;
use super::storage::CredentialStore;

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
    private_key_pkcs8: String,
}

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

#[derive(Debug, Deserialize)]
struct AuthorityRecord {
    enrollment_id: String,
    status: EnrollmentStatus,
    public_key_fingerprint: String,
    created_at: String,
    device_id: Option<String>,
    certificate: Option<IssuedCertificate>,
}

pub struct EnrollmentClient {
    client: Client,
    endpoint: Url,
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
        })
    }

    #[cfg(test)]
    fn for_test(service_url: &Url) -> Result<Self> {
        Ok(Self {
            client: Client::new(),
            endpoint: service_url.join("v1/enrollments")?,
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

fn validate_persisted_record(record: &EnrollmentRecord) -> Result<()> {
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

fn public_key_fingerprint(key: &KeyPair) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(key.subject_public_key_info()))
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
    let identity_pem = format!(
        "{}{}",
        certificate.certificate_chain_pem,
        key.serialize_pem()
    );
    Ok(reqwest::Identity::from_pem(identity_pem.as_bytes())?)
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use base64::Engine;
    use rcgen::{
        BasicConstraints, CertificateParams, CertificateSigningRequestParams, CertifiedIssuer,
        IsCa, KeyPair, PKCS_ECDSA_P256_SHA256, PublicKeyData,
    };
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use tokio::net::TcpListener;
    use url::Url;

    use super::{
        EnrollmentClient, EnrollmentStatus, load_device_identity_for, load_enrollment_for,
        save_enrollment_for,
    };
    use crate::identity::dpop::DpopKey;
    use crate::identity::oauth::{ManagedIdentity, StoredSession};
    use crate::identity::storage::{CredentialStorageMode, CredentialStore};

    struct AuthorityState {
        fingerprint: tokio::sync::Mutex<String>,
        certificate: tokio::sync::Mutex<String>,
    }

    #[tokio::test]
    async fn requests_csr_and_installs_matching_certificate() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let service_url =
            Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let state = Arc::new(AuthorityState {
            fingerprint: tokio::sync::Mutex::new(String::new()),
            certificate: tokio::sync::Mutex::new(String::new()),
        });
        let app = Router::new()
            .route("/v1/enrollments", post(request_enrollment))
            .route("/v1/enrollments/{id}", get(enrollment_status))
            .with_state(state);
        let server = tokio::spawn(axum::serve(listener, app).into_future());
        let temporary = tempfile::tempdir().unwrap();
        let store = CredentialStore::setup(
            CredentialStorageMode::File,
            &temporary.path().join("identity"),
        )
        .unwrap();
        let identity = test_identity(&store);
        let client = EnrollmentClient::for_test(&service_url).unwrap();

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

    fn issue_for_csr(request: &CertificateSigningRequestParams) -> String {
        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();
        request.signed_by(&ca).unwrap().pem()
    }

    fn test_identity(store: &CredentialStore) -> ManagedIdentity {
        let dpop_key = DpopKey::generate();
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
                dpop_private_key: base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(dpop_key.to_pkcs8_der().unwrap()),
                generation: 1,
            },
            store.clone(),
        )
    }
}
