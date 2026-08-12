use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

use agentdesktop_proto::fleet::{BeginEnrollmentResponse, CompleteEnrollmentRequest};
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header, jwk::JwkSet};
use rand::Rng;
use serde::Deserialize;
use tokio::sync::Mutex;
use url::Url;

const ENROLLMENT_LIFETIME: Duration = Duration::from_secs(10 * 60);
const MAX_PENDING_ENROLLMENTS: usize = 1024;

#[derive(Clone)]
pub struct OidcProvider {
    inner: Arc<Inner>,
}

struct Inner {
    issuer: String,
    client_id: String,
    redirect_uri: String,
    authorization_endpoint: Url,
    token_endpoint: Url,
    jwks: JwkSet,
    http: reqwest::Client,
    pending: Mutex<HashMap<String, PendingEnrollment>>,
}

struct PendingEnrollment {
    hostname: String,
    nonce: String,
    expires_at: Instant,
}

pub struct CompletedEnrollment {
    pub hostname: String,
    pub issuer: String,
    pub subject: String,
    pub idp_claims: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    authorization_endpoint: Url,
    token_endpoint: Url,
    jwks_uri: Url,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
}

#[derive(Deserialize)]
struct IdTokenClaims {
    sub: String,
    nonce: String,
    #[serde(flatten)]
    additional: BTreeMap<String, serde_json::Value>,
}

impl OidcProvider {
    pub async fn discover(
        issuer: String,
        client_id: String,
        redirect_uri: String,
    ) -> anyhow::Result<Self> {
        let _ = jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER.install_default();
        let http = reqwest::Client::new();
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            issuer.trim_end_matches('/')
        );
        let document = http
            .get(&discovery_url)
            .send()
            .await
            .with_context(|| format!("fetch OIDC discovery document from {discovery_url}"))?
            .error_for_status()
            .context("OIDC discovery endpoint returned an error")?
            .json::<DiscoveryDocument>()
            .await
            .context("decode OIDC discovery document")?;
        if document.issuer != issuer {
            bail!(
                "OIDC discovery issuer mismatch: expected {issuer}, got {}",
                document.issuer
            );
        }

        let jwks = http
            .get(document.jwks_uri)
            .send()
            .await
            .context("fetch OIDC JWKS")?
            .error_for_status()
            .context("OIDC JWKS endpoint returned an error")?
            .json::<JwkSet>()
            .await
            .context("decode OIDC JWKS")?;

        Ok(Self {
            inner: Arc::new(Inner {
                issuer,
                client_id,
                redirect_uri,
                authorization_endpoint: document.authorization_endpoint,
                token_endpoint: document.token_endpoint,
                jwks,
                http,
                pending: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub async fn begin(
        &self,
        hostname: String,
        code_challenge: &str,
    ) -> anyhow::Result<BeginEnrollmentResponse> {
        if hostname.trim().is_empty() {
            bail!("hostname is required");
        }
        if code_challenge.len() != 43
            || !code_challenge
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            bail!("invalid PKCE code challenge");
        }

        let enrollment_id = random_secret();
        let state = random_secret();
        let nonce = random_secret();
        let mut authorization_url = self.inner.authorization_endpoint.clone();
        authorization_url
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("scope", "openid profile email")
            .append_pair("client_id", &self.inner.client_id)
            .append_pair("redirect_uri", &self.inner.redirect_uri)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256");

        let mut pending = self.inner.pending.lock().await;
        let now = Instant::now();
        pending.retain(|_, enrollment| enrollment.expires_at > now);
        if pending.len() >= MAX_PENDING_ENROLLMENTS {
            bail!("too many pending enrollments");
        }
        pending.insert(
            enrollment_id.clone(),
            PendingEnrollment {
                hostname,
                nonce,
                expires_at: now + ENROLLMENT_LIFETIME,
            },
        );

        Ok(BeginEnrollmentResponse {
            enrollment_id,
            authorization_url: authorization_url.into(),
            state,
            redirect_uri: self.inner.redirect_uri.clone(),
        })
    }

    pub async fn complete(
        &self,
        request: CompleteEnrollmentRequest,
    ) -> anyhow::Result<CompletedEnrollment> {
        let pending = self
            .inner
            .pending
            .lock()
            .await
            .remove(&request.enrollment_id)
            .context("unknown or already completed enrollment")?;
        if pending.expires_at <= Instant::now() {
            bail!("enrollment expired");
        }
        if request.authorization_code.is_empty() || request.code_verifier.is_empty() {
            bail!("authorization code and PKCE verifier are required");
        }

        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("grant_type", "authorization_code")
            .append_pair("code", &request.authorization_code)
            .append_pair("redirect_uri", &self.inner.redirect_uri)
            .append_pair("client_id", &self.inner.client_id)
            .append_pair("code_verifier", &request.code_verifier)
            .finish();
        let token = self
            .inner
            .http
            .post(self.inner.token_endpoint.clone())
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .context("exchange OIDC authorization code")?
            .error_for_status()
            .context("OIDC token endpoint rejected authorization code")?
            .json::<TokenResponse>()
            .await
            .context("decode OIDC token response")?;

        let header = decode_header(&token.id_token).context("decode ID token header")?;
        let kid = header.kid.context("ID token has no key ID")?;
        let jwk = self
            .inner
            .jwks
            .find(&kid)
            .context("ID token refers to an unknown key")?;
        let key = DecodingKey::from_jwk(jwk).context("construct ID token verification key")?;
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&self.inner.issuer]);
        validation.set_audience(&[&self.inner.client_id]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = decode::<IdTokenClaims>(&token.id_token, &key, &validation)
            .context("validate OIDC ID token")?
            .claims;
        if claims.nonce != pending.nonce {
            bail!("OIDC nonce mismatch");
        }

        let mut idp_claims = claims.additional;
        idp_claims.insert("sub".to_owned(), claims.sub.clone().into());
        idp_claims.insert("nonce".to_owned(), claims.nonce.into());

        Ok(CompletedEnrollment {
            hostname: pending.hostname,
            issuer: self.inner.issuer.clone(),
            subject: claims.sub,
            idp_claims,
        })
    }
}

fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
