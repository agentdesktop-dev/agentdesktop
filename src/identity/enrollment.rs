use anyhow::{Result, bail};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use url::Url;

use super::oauth::ManagedIdentity;
use super::storage::CredentialStore;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum EnrollmentStatus {
    Pending,
    Approved,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum DeviceStatus {
    Active,
    Revoked,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct EnrollmentUser {
    pub iss: Url,
    pub sub: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub struct EnrollmentRecord {
    pub enrollment_id: String,
    pub status: EnrollmentStatus,
    pub user: EnrollmentUser,
    pub dpop_jkt: String,
    pub device_id: Option<String>,
    pub device_status: Option<DeviceStatus>,
}

#[derive(Debug, Deserialize)]
struct EnrollmentMetadata {
    issuer: Url,
    enrollment_endpoint: Url,
}

pub struct EnrollmentClient {
    client: Client,
    issuer: Url,
    endpoint: Url,
}

impl EnrollmentClient {
    pub async fn discover(issuer: &Url) -> Result<Self> {
        let discovery = issuer.join(".well-known/oauth-authorization-server")?;
        let metadata: EnrollmentMetadata = Client::new()
            .get(discovery)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        if metadata.issuer != *issuer {
            bail!("enrollment metadata issuer does not match configured issuer");
        }
        if metadata.enrollment_endpoint.query().is_some()
            || metadata.enrollment_endpoint.fragment().is_some()
        {
            bail!("enrollment endpoint must not contain a query or fragment");
        }
        Ok(Self {
            client: Client::new(),
            issuer: issuer.clone(),
            endpoint: metadata.enrollment_endpoint,
        })
    }

    pub async fn request(&self, identity: &ManagedIdentity) -> Result<EnrollmentRecord> {
        let response = self
            .authenticated(identity, reqwest::Method::POST, self.endpoint.clone())
            .await?
            .send()
            .await?;
        if response.status() != StatusCode::ACCEPTED {
            bail!(
                "enrollment request failed with status {}",
                response.status()
            );
        }
        validate_record(
            response.json().await?,
            Some(EnrollmentStatus::Pending),
            &self.issuer,
            &identity.dpop_thumbprint().await?,
        )
    }

    pub async fn status(
        &self,
        identity: &ManagedIdentity,
        enrollment_id: &str,
    ) -> Result<EnrollmentRecord> {
        if enrollment_id.is_empty() {
            bail!("enrollment ID must not be empty");
        }
        let mut endpoint = self.endpoint.clone();
        endpoint
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("enrollment endpoint cannot be a base URL"))?
            .push(enrollment_id);
        let response = self
            .authenticated(identity, reqwest::Method::GET, endpoint)
            .await?
            .send()
            .await?
            .error_for_status()?;
        validate_record(
            response.json().await?,
            None,
            &self.issuer,
            &identity.dpop_thumbprint().await?,
        )
    }

    async fn authenticated(
        &self,
        identity: &ManagedIdentity,
        method: reqwest::Method,
        endpoint: Url,
    ) -> Result<reqwest::RequestBuilder> {
        let credentials = identity
            .credentials(method.as_str(), endpoint.as_str())
            .await?;
        Ok(self
            .client
            .request(method, endpoint)
            .header(
                "authorization",
                format!("DPoP {}", credentials.access_token),
            )
            .header("dpop", credentials.proof))
    }
}

fn validate_record(
    record: EnrollmentRecord,
    expected: Option<EnrollmentStatus>,
    issuer: &Url,
    dpop_jkt: &str,
) -> Result<EnrollmentRecord> {
    if record.enrollment_id.is_empty() || record.user.sub.is_empty() || record.dpop_jkt.is_empty() {
        bail!("enrollment authority returned an incomplete identity record");
    }
    if expected.is_some_and(|expected| record.status != expected) {
        bail!("enrollment authority returned an unexpected status");
    }
    if record.user.iss != *issuer || record.dpop_jkt != dpop_jkt {
        bail!("enrollment authority returned mismatched identity binding");
    }
    match record.status {
        EnrollmentStatus::Pending
            if record.device_id.is_some() || record.device_status.is_some() =>
        {
            bail!("pending enrollment must not contain device identity")
        }
        EnrollmentStatus::Approved
            if record.device_id.as_deref().is_none_or(str::is_empty)
                || record.device_status.is_none() =>
        {
            bail!("approved enrollment is missing device identity or status")
        }
        _ => {}
    }
    Ok(record)
}

pub fn save_enrollment_for(
    issuer: &Url,
    gateway_origin: &Url,
    store: &CredentialStore,
    record: &EnrollmentRecord,
) -> Result<()> {
    store.put(
        &enrollment_record_name(issuer, gateway_origin),
        &serde_json::to_vec(record)?,
    )
}

pub fn load_enrollment_for(
    issuer: &Url,
    gateway_origin: &Url,
    dpop_jkt: &str,
    store: &CredentialStore,
) -> Result<EnrollmentRecord> {
    let record: EnrollmentRecord =
        serde_json::from_slice(&store.get(&enrollment_record_name(issuer, gateway_origin))?)?;
    if record.user.iss != *issuer || record.dpop_jkt != dpop_jkt {
        bail!("persisted enrollment does not match the current identity session");
    }
    Ok(record)
}

pub fn delete_enrollment_for(
    issuer: &Url,
    gateway_origin: &Url,
    store: &CredentialStore,
) -> Result<()> {
    store.delete_if_exists(&enrollment_record_name(issuer, gateway_origin))
}

fn enrollment_record_name(issuer: &Url, gateway_origin: &Url) -> String {
    format!("enrollment|{issuer}|{gateway_origin}")
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
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use tokio::net::TcpListener;
    use url::Url;

    use super::{
        DeviceStatus, EnrollmentClient, EnrollmentRecord, EnrollmentStatus, EnrollmentUser,
        delete_enrollment_for, load_enrollment_for, save_enrollment_for, validate_record,
    };
    use crate::identity::dpop::{DpopKey, decode_jwt_claims};
    use crate::identity::oauth::{ManagedIdentity, StoredSession};
    use crate::identity::storage::{CredentialStorageMode, CredentialStore};

    struct AuthorityState {
        issuer: Url,
        enrollment_endpoint: Url,
        dpop_jkt: String,
    }

    #[tokio::test]
    async fn discovers_and_reads_dpop_bound_enrollment() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let key = DpopKey::generate();
        let state = Arc::new(AuthorityState {
            enrollment_endpoint: issuer.join("enrollments").unwrap(),
            issuer: issuer.clone(),
            dpop_jkt: key.thumbprint().unwrap(),
        });
        let app = Router::new()
            .route("/.well-known/oauth-authorization-server", get(metadata))
            .route("/enrollments", post(request_enrollment))
            .route("/enrollments/{id}", get(enrollment_status))
            .with_state(state);
        let server = tokio::spawn(axum::serve(listener, app).into_future());

        let temporary = tempfile::tempdir().unwrap();
        let store = CredentialStore::setup(
            CredentialStorageMode::File,
            &temporary.path().join("identity"),
        )
        .unwrap();
        let identity = ManagedIdentity::new(
            StoredSession {
                issuer: issuer.clone(),
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
                    .encode(key.to_pkcs8_der().unwrap()),
                generation: 1,
            },
            store,
        );

        let client = EnrollmentClient::discover(&issuer).await.unwrap();
        let pending = client.request(&identity).await.unwrap();
        assert_eq!(pending.status, EnrollmentStatus::Pending);
        let approved = client
            .status(&identity, &pending.enrollment_id)
            .await
            .unwrap();
        assert_eq!(approved.status, EnrollmentStatus::Approved);
        assert_eq!(approved.device_id.as_deref(), Some("device-1"));
        assert_eq!(approved.device_status, Some(DeviceStatus::Active));

        server.abort();
    }

    #[test]
    fn rejects_device_identity_on_pending_record() {
        let record = EnrollmentRecord {
            enrollment_id: "enrollment-1".into(),
            status: EnrollmentStatus::Pending,
            user: EnrollmentUser {
                iss: Url::parse("https://issuer.example/").unwrap(),
                sub: "user-1".into(),
            },
            dpop_jkt: "thumbprint".into(),
            device_id: Some("untrusted-device".into()),
            device_status: None,
        };

        let error = validate_record(
            record,
            None,
            &Url::parse("https://issuer.example/").unwrap(),
            "thumbprint",
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("must not contain device identity")
        );
    }

    #[test]
    fn rejects_mismatched_authority_identity_binding() {
        let issuer = Url::parse("https://issuer.example/").unwrap();
        let record = EnrollmentRecord {
            enrollment_id: "enrollment-1".into(),
            status: EnrollmentStatus::Pending,
            user: EnrollmentUser {
                iss: issuer.clone(),
                sub: "user-1".into(),
            },
            dpop_jkt: "different-thumbprint".into(),
            device_id: None,
            device_status: None,
        };

        let error = validate_record(record, None, &issuer, "expected-thumbprint").unwrap_err();

        assert!(error.to_string().contains("mismatched identity binding"));
    }

    #[test]
    fn persists_only_for_the_current_identity_key() {
        let temporary = tempfile::tempdir().unwrap();
        let store = CredentialStore::setup(
            CredentialStorageMode::File,
            &temporary.path().join("identity"),
        )
        .unwrap();
        let issuer = Url::parse("https://issuer.example/").unwrap();
        let gateway = Url::parse("https://gateway.example/").unwrap();
        let record = EnrollmentRecord {
            enrollment_id: "enrollment-1".into(),
            status: EnrollmentStatus::Pending,
            user: EnrollmentUser {
                iss: issuer.clone(),
                sub: "user-1".into(),
            },
            dpop_jkt: "thumbprint".into(),
            device_id: None,
            device_status: None,
        };

        save_enrollment_for(&issuer, &gateway, &store, &record).unwrap();
        assert_eq!(
            load_enrollment_for(&issuer, &gateway, "thumbprint", &store).unwrap(),
            record
        );
        let stale = load_enrollment_for(&issuer, &gateway, "rotated-key", &store).unwrap_err();
        assert!(stale.to_string().contains("current identity session"));

        delete_enrollment_for(&issuer, &gateway, &store).unwrap();
        delete_enrollment_for(&issuer, &gateway, &store).unwrap();
        assert!(load_enrollment_for(&issuer, &gateway, "thumbprint", &store).is_err());
    }

    async fn metadata(State(state): State<Arc<AuthorityState>>) -> Json<Value> {
        Json(json!({
            "issuer": state.issuer,
            "enrollment_endpoint": state.enrollment_endpoint,
        }))
    }

    async fn request_enrollment(
        State(state): State<Arc<AuthorityState>>,
        headers: HeaderMap,
    ) -> (StatusCode, Json<Value>) {
        assert_credentials(&headers, "POST", state.enrollment_endpoint.as_str());
        (
            StatusCode::ACCEPTED,
            Json(json!({
                "enrollment_id": "enrollment-1",
                "status": "pending",
                "user": {"iss": state.issuer, "sub": "user-1"},
                "dpop_jkt": state.dpop_jkt,
            })),
        )
    }

    async fn enrollment_status(
        Path(id): Path<String>,
        State(state): State<Arc<AuthorityState>>,
        headers: HeaderMap,
    ) -> Json<Value> {
        assert_eq!(id, "enrollment-1");
        let endpoint = state.issuer.join("enrollments/enrollment-1").unwrap();
        assert_credentials(&headers, "GET", endpoint.as_str());
        Json(json!({
            "enrollment_id": id,
            "status": "approved",
            "user": {"iss": state.issuer, "sub": "user-1"},
            "dpop_jkt": state.dpop_jkt,
            "device_id": "device-1",
            "device_status": "active",
        }))
    }

    fn assert_credentials(headers: &HeaderMap, method: &str, endpoint: &str) {
        assert_eq!(headers["authorization"], "DPoP access-token");
        let claims = decode_jwt_claims(headers["dpop"].to_str().unwrap()).unwrap();
        assert_eq!(claims["htm"], method);
        assert_eq!(claims["htu"], endpoint);
        let expected =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest("access-token"));
        assert_eq!(claims["ath"], expected);
    }
}
