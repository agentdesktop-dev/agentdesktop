use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use agentdesktop_core::http::ClientExt;
use agentdesktop_proto::fleet::{BeginEnrollmentResponse, CompleteEnrollmentRequest};
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use jsonwebtoken::{
    DecodingKey, Validation, decode, decode_header,
    jwk::{Jwk, JwkSet},
};
use rand::Rng;
use reqwest::header::CACHE_CONTROL;
use serde::Deserialize;
use tokio::sync::Mutex;
use url::Url;

const ENROLLMENT_LIFETIME: Duration = Duration::from_secs(10 * 60);
const MAX_PENDING_ENROLLMENTS: usize = 1024;
const JWKS_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const JWKS_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);
const ENROLLMENT_SCOPE: &str = "openid profile email offline_access";
// RFC 8628 section 3.2: clients poll every five seconds unless told otherwise.
const DEFAULT_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(5);

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
    userinfo_endpoint: Url,
    device_authorization_endpoint: Option<Url>,
    jwks: JwksCache,
    http: reqwest::Client,
    pending: Mutex<HashMap<String, PendingEnrollment>>,
}

struct JwksCache {
    http: reqwest::Client,
    uri: Url,
    keys: RwLock<JwkSet>,
    refresh: Mutex<Option<Instant>>,
    refresh_cooldown: Duration,
}

struct PendingEnrollment {
    hostname: String,
    /// Absent for device authorization enrollments, which have no
    /// authorization request to bind a nonce to.
    nonce: Option<String>,
    expires_at: Instant,
}

pub struct CompletedEnrollment {
    pub hostname: String,
    pub issuer: String,
    pub subject: String,
    pub idp_claims: BTreeMap<String, serde_json::Value>,
}

/// Stable identity authenticated by one OIDC provider.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OidcPrincipal {
    pub issuer: String,
    pub subject: String,
}

#[derive(Deserialize)]
struct DiscoveryDocument {
    issuer: String,
    authorization_endpoint: Url,
    token_endpoint: Url,
    userinfo_endpoint: Url,
    jwks_uri: Url,
    #[serde(default)]
    device_authorization_endpoint: Option<Url>,
}

/// RFC 8628 section 3.2 device authorization response.
#[derive(Deserialize)]
struct DeviceAuthorizationResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    verification_uri_complete: Option<String>,
    expires_in: u64,
    #[serde(default)]
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct IdTokenClaims {
    sub: String,
    #[serde(default)]
    nonce: Option<String>,
    #[serde(flatten)]
    additional: BTreeMap<String, serde_json::Value>,
}

#[derive(Deserialize)]
struct UserInfo {
    sub: String,
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
            .get_json::<DiscoveryDocument>(&discovery_url)
            .await
            .with_context(|| format!("fetch OIDC discovery document from {discovery_url}"))?;
        if document.issuer != issuer {
            bail!(
                "OIDC discovery issuer mismatch: expected {issuer}, got {}",
                document.issuer
            );
        }

        let jwks = fetch_jwks(http.get(document.jwks_uri.clone())).await?;

        Ok(Self {
            inner: Arc::new(Inner {
                issuer,
                client_id,
                redirect_uri,
                authorization_endpoint: document.authorization_endpoint,
                token_endpoint: document.token_endpoint,
                userinfo_endpoint: document.userinfo_endpoint,
                device_authorization_endpoint: document.device_authorization_endpoint,
                jwks: JwksCache::new(http.clone(), document.jwks_uri, jwks, JWKS_REFRESH_COOLDOWN),
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
            .append_pair("scope", ENROLLMENT_SCOPE)
            .append_pair("client_id", &self.inner.client_id)
            .append_pair("redirect_uri", &self.inner.redirect_uri)
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", code_challenge)
            .append_pair("code_challenge_method", "S256");

        self.insert_pending(
            enrollment_id.clone(),
            hostname,
            Some(nonce),
            ENROLLMENT_LIFETIME,
        )
        .await?;

        Ok(BeginEnrollmentResponse {
            enrollment_id,
            authorization_url: authorization_url.into(),
            state,
            redirect_uri: self.inner.redirect_uri.clone(),
            token_endpoint: self.inner.token_endpoint.to_string(),
            client_id: self.inner.client_id.clone(),
            ..Default::default()
        })
    }

    /// Starts an enrollment with the OAuth 2.0 Device Authorization Grant
    /// (RFC 8628), for hosts that cannot open a local browser.
    ///
    /// The daemon polls the token endpoint itself and completes the
    /// enrollment with the resulting tokens, exactly like the browser flow.
    pub async fn begin_device(&self, hostname: String) -> anyhow::Result<BeginEnrollmentResponse> {
        if hostname.trim().is_empty() {
            bail!("hostname is required");
        }
        let endpoint = self
            .inner
            .device_authorization_endpoint
            .clone()
            .context("OIDC provider does not support device authorization")?;

        // Reserve a slot BEFORE contacting the IdP, not after. Reserving only once the
        // IdP has already answered (the previous order) let MAX_PENDING_ENROLLMENTS
        // fill up without ever bounding the outbound device-grant requests themselves:
        // every caller's request still reached the IdP and only got rejected
        // afterwards, on the insert. The reservation uses ENROLLMENT_LIFETIME as a
        // placeholder expiry — the real, possibly shorter, device lifetime isn't known
        // until the IdP responds — and is narrowed below once it is, or dropped
        // entirely if the IdP call or the response fails.
        let enrollment_id = random_secret();
        self.insert_pending(enrollment_id.clone(), hostname, None, ENROLLMENT_LIFETIME)
            .await?;

        match self.request_device_grant(&enrollment_id, endpoint).await {
            Ok((response, lifetime)) => {
                self.set_pending_expiry(&enrollment_id, lifetime).await;
                Ok(response)
            }
            Err(error) => {
                self.release_pending(&enrollment_id).await;
                Err(error)
            }
        }
    }

    /// The IdP round-trip and response validation for [`begin_device`], split out so
    /// its error paths all flow through one `?`-propagating function and the caller
    /// can release the reservation on any of them uniformly.
    async fn request_device_grant(
        &self,
        enrollment_id: &str,
        endpoint: Url,
    ) -> anyhow::Result<(BeginEnrollmentResponse, Duration)> {
        let device = self
            .inner
            .http
            .post(endpoint)
            .form(&[
                ("client_id", self.inner.client_id.as_str()),
                ("scope", ENROLLMENT_SCOPE),
            ])
            .send()
            .await
            .context("request OIDC device authorization")?
            .error_for_status()
            .context("OIDC device authorization endpoint returned an error")?
            .json::<DeviceAuthorizationResponse>()
            .await
            .context("decode OIDC device authorization response")?;

        device_enrollment_response(
            enrollment_id.to_owned(),
            device,
            self.inner.token_endpoint.to_string(),
            self.inner.client_id.clone(),
        )
    }

    async fn insert_pending(
        &self,
        enrollment_id: String,
        hostname: String,
        nonce: Option<String>,
        lifetime: Duration,
    ) -> anyhow::Result<()> {
        let mut pending = self.inner.pending.lock().await;
        let now = Instant::now();
        pending.retain(|_, enrollment| enrollment.expires_at > now);
        if pending.len() >= MAX_PENDING_ENROLLMENTS {
            bail!("too many pending enrollments");
        }
        pending.insert(
            enrollment_id,
            PendingEnrollment {
                hostname,
                nonce,
                expires_at: now + lifetime,
            },
        );
        Ok(())
    }

    /// Drops a reservation [`insert_pending`] made, e.g. because the IdP request or
    /// response validation that was supposed to fill it in failed. Already-removed
    /// (expired and pruned, or raced with a concurrent completion) is not an error.
    async fn release_pending(&self, enrollment_id: &str) {
        self.inner.pending.lock().await.remove(enrollment_id);
    }

    /// Narrows a reservation's expiry to the real device-grant lifetime once the IdP
    /// has responded, which can be shorter than the ENROLLMENT_LIFETIME placeholder
    /// [`begin_device`] reserved it with. A no-op if the entry is already gone.
    async fn set_pending_expiry(&self, enrollment_id: &str, lifetime: Duration) {
        if let Some(entry) = self.inner.pending.lock().await.get_mut(enrollment_id) {
            entry.expires_at = Instant::now() + lifetime;
        }
    }

    pub async fn complete(
        &self,
        request: CompleteEnrollmentRequest,
        access_token: &str,
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
        if request.id_token.is_empty() || access_token.is_empty() {
            bail!("ID token and access token are required");
        }
        let header = decode_header(&request.id_token).context("decode ID token header")?;
        let kid = header.kid.context("ID token has no key ID")?;
        let jwk = self.inner.jwks.key_for(&kid).await?;
        let key = DecodingKey::from_jwk(&jwk).context("construct ID token verification key")?;
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&self.inner.issuer]);
        validation.set_audience(&[&self.inner.client_id]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = decode::<IdTokenClaims>(&request.id_token, &key, &validation)
            .context("validate OIDC ID token")?
            .claims;
        if let Some(expected) = &pending.nonce
            && claims.nonce.as_ref() != Some(expected)
        {
            bail!("OIDC nonce mismatch");
        }
        let access_principal = self.authenticate_access_token(access_token).await?;
        if access_principal.subject != claims.sub {
            bail!("access token and ID token subjects do not match");
        }

        let mut idp_claims = claims.additional;
        idp_claims.insert("sub".to_owned(), claims.sub.clone().into());
        if let Some(nonce) = claims.nonce {
            idp_claims.insert("nonce".to_owned(), nonce.into());
        }

        Ok(CompletedEnrollment {
            hostname: pending.hostname,
            issuer: access_principal.issuer,
            subject: access_principal.subject,
            idp_claims,
        })
    }

    pub async fn authenticate_access_token(
        &self,
        access_token: &str,
    ) -> anyhow::Result<OidcPrincipal> {
        if access_token.is_empty() {
            bail!("access token is required");
        }
        let user = self
            .inner
            .http
            .get(self.inner.userinfo_endpoint.clone())
            .bearer_auth(access_token)
            .send()
            .await
            .context("query OIDC UserInfo endpoint")?
            .error_for_status()
            .context("OIDC UserInfo endpoint rejected access token")?
            .json::<UserInfo>()
            .await
            .context("decode OIDC UserInfo response")?;
        if user.sub.is_empty() {
            bail!("OIDC UserInfo response has no subject");
        }
        Ok(OidcPrincipal {
            issuer: self.inner.issuer.clone(),
            subject: user.sub,
        })
    }
}

impl JwksCache {
    fn new(http: reqwest::Client, uri: Url, keys: JwkSet, refresh_cooldown: Duration) -> Self {
        Self {
            http,
            uri,
            keys: RwLock::new(keys),
            refresh: Mutex::new(None),
            refresh_cooldown,
        }
    }

    async fn key_for(&self, kid: &str) -> anyhow::Result<Jwk> {
        self.key_for_with(kid, || refresh_jwks(&self.http, &self.uri))
            .await
    }

    async fn key_for_with<F, Fut>(&self, kid: &str, fetch: F) -> anyhow::Result<Jwk>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = anyhow::Result<JwkSet>>,
    {
        if let Some(key) = self.cached_key(kid) {
            return Ok(key);
        }

        let mut refresh = self.refresh.lock().await;

        // Another request may have refreshed the key set while this request
        // waited for the single-flight refresh gate.
        if let Some(key) = self.cached_key(kid) {
            return Ok(key);
        }

        let now = Instant::now();
        if let Some(last_attempt) = *refresh {
            let elapsed = now.saturating_duration_since(last_attempt);
            if elapsed < self.refresh_cooldown {
                tracing::debug!(
                    retry_after_seconds = (self.refresh_cooldown - elapsed).as_secs(),
                    "OIDC JWKS refresh suppressed by cooldown"
                );
                bail!("ID token refers to an unknown key; OIDC JWKS refresh is rate limited");
            }
        }

        // Record attempts before fallible network I/O so timeouts, malformed
        // responses, and cancelled refreshes cannot produce a request storm.
        *refresh = Some(now);
        let keys = fetch()
            .await
            .context("refresh OIDC JWKS after unknown key")?;
        let key_count = keys.keys.len();
        *self.keys.write().expect("OIDC JWKS cache lock poisoned") = keys;
        tracing::info!(key_count, "refreshed OIDC JWKS after unknown key");

        self.cached_key(kid)
            .context("ID token refers to an unknown key after refreshing OIDC JWKS")
    }

    fn cached_key(&self, kid: &str) -> Option<Jwk> {
        self.keys
            .read()
            .expect("OIDC JWKS cache lock poisoned")
            .find(kid)
            .cloned()
    }
}

async fn refresh_jwks(http: &reqwest::Client, uri: &Url) -> anyhow::Result<JwkSet> {
    // Bound how long an unknown-key request can hold the single-flight refresh gate.
    fetch_jwks(refresh_jwks_request(http, uri)).await
}

fn refresh_jwks_request(http: &reqwest::Client, uri: &Url) -> reqwest::RequestBuilder {
    http.get(uri.clone())
        .header(CACHE_CONTROL, "no-cache")
        .timeout(JWKS_FETCH_TIMEOUT)
}

async fn fetch_jwks(request: reqwest::RequestBuilder) -> anyhow::Result<JwkSet> {
    let jwks = request
        .send()
        .await
        .context("fetch OIDC JWKS")?
        .error_for_status()
        .context("OIDC JWKS endpoint returned an error")?
        .json::<JwkSet>()
        .await
        .context("decode OIDC JWKS")?;
    if jwks.keys.is_empty() {
        bail!("OIDC JWKS is empty");
    }
    Ok(jwks)
}

/// Builds the enrollment response for a device authorization and returns how
/// long the enrollment stays pending: the IdP's device code lifetime, capped at
/// the browser enrollment lifetime.
fn device_enrollment_response(
    enrollment_id: String,
    device: DeviceAuthorizationResponse,
    token_endpoint: String,
    client_id: String,
) -> anyhow::Result<(BeginEnrollmentResponse, Duration)> {
    if device.device_code.is_empty() || device.user_code.is_empty() {
        bail!("OIDC device authorization response has no device or user code");
    }
    Url::parse(&device.verification_uri).context("OIDC device verification URI is invalid")?;
    let verification_uri_complete = device
        .verification_uri_complete
        .filter(|uri| Url::parse(uri).is_ok())
        .unwrap_or_default();
    let lifetime = Duration::from_secs(device.expires_in).min(ENROLLMENT_LIFETIME);
    if lifetime.is_zero() {
        bail!("OIDC device authorization has already expired");
    }
    let interval = device
        .interval
        .filter(|interval| *interval > 0)
        .map_or(DEFAULT_DEVICE_POLL_INTERVAL, Duration::from_secs);

    Ok((
        BeginEnrollmentResponse {
            enrollment_id,
            token_endpoint,
            client_id,
            device_code: device.device_code,
            user_code: device.user_code,
            verification_uri: device.verification_uri,
            verification_uri_complete,
            interval_seconds: interval.as_secs(),
            expires_in_seconds: lifetime.as_secs(),
            ..Default::default()
        },
        lifetime,
    ))
}

fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use axum::http::StatusCode;
    use jsonwebtoken::DecodingKey;
    use serde_json::json;
    use tokio::{sync::Mutex, task::yield_now};
    use url::Url;

    use super::{
        DeviceAuthorizationResponse, Inner, JWKS_FETCH_TIMEOUT, JwkSet, JwksCache,
        MAX_PENDING_ENROLLMENTS, OidcProvider, PendingEnrollment, device_enrollment_response,
        refresh_jwks_request,
    };

    fn device_authorization(value: serde_json::Value) -> DeviceAuthorizationResponse {
        serde_json::from_value(value).unwrap()
    }

    fn device_response(
        value: serde_json::Value,
    ) -> anyhow::Result<(agentdesktop_proto::fleet::BeginEnrollmentResponse, Duration)> {
        device_enrollment_response(
            "enrollment".to_owned(),
            device_authorization(value),
            "https://idp.example/token".to_owned(),
            "client".to_owned(),
        )
    }

    #[test]
    fn device_enrollment_caps_lifetime_and_defaults_interval() {
        let (response, lifetime) = device_response(json!({
            "device_code": "device",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://idp.example/activate",
            "verification_uri_complete": "https://idp.example/activate?user_code=ABCD-EFGH",
            "expires_in": 3600,
        }))
        .unwrap();
        assert_eq!(lifetime, Duration::from_secs(10 * 60));
        assert_eq!(response.expires_in_seconds, 600);
        assert_eq!(response.interval_seconds, 5);
        assert_eq!(response.enrollment_id, "enrollment");
        assert_eq!(response.token_endpoint, "https://idp.example/token");
        assert_eq!(response.client_id, "client");
        assert_eq!(response.device_code, "device");
        assert_eq!(response.user_code, "ABCD-EFGH");
        assert_eq!(
            response.verification_uri_complete,
            "https://idp.example/activate?user_code=ABCD-EFGH"
        );
        assert!(response.authorization_url.is_empty());
        assert!(response.state.is_empty());
    }

    #[test]
    fn device_enrollment_keeps_shorter_lifetime_and_interval() {
        let (response, lifetime) = device_response(json!({
            "device_code": "device",
            "user_code": "CODE",
            "verification_uri": "https://idp.example/activate",
            "expires_in": 120,
            "interval": 10,
        }))
        .unwrap();
        assert_eq!(lifetime, Duration::from_secs(120));
        assert_eq!(response.interval_seconds, 10);
        assert!(response.verification_uri_complete.is_empty());
    }

    #[test]
    fn device_enrollment_rejects_invalid_responses() {
        let valid = json!({
            "device_code": "device",
            "user_code": "CODE",
            "verification_uri": "https://idp.example/activate",
            "expires_in": 120,
        });
        for (field, value) in [
            ("device_code", json!("")),
            ("user_code", json!("")),
            ("verification_uri", json!("not a url")),
            ("expires_in", json!(0)),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(
                device_response(invalid).is_err(),
                "{field} should be rejected"
            );
        }

        let mut invalid_complete = valid;
        invalid_complete["verification_uri_complete"] = json!("not a url");
        let (response, _) = device_response(invalid_complete).unwrap();
        assert!(response.verification_uri_complete.is_empty());
    }

    const RSA_MODULUS: &str = "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzsKJkZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw";

    fn jwks(kids: &[&str]) -> JwkSet {
        let keys = kids
            .iter()
            .map(|kid| {
                json!({
                    "kty": "RSA",
                    "kid": kid,
                    "use": "sig",
                    "alg": "RS256",
                    "n": RSA_MODULUS,
                    "e": "AQAB"
                })
            })
            .collect::<Vec<_>>();
        serde_json::from_value(json!({ "keys": keys })).expect("construct test JWKS")
    }

    fn cache(keys: JwkSet) -> JwksCache {
        JwksCache::new(
            reqwest::Client::new(),
            "https://unused.invalid/keys"
                .parse()
                .expect("parse unused JWKS URL"),
            keys,
            std::time::Duration::from_secs(30),
        )
    }

    /// Builds an `OidcProvider` without going through `discover`'s own HTTP calls, so
    /// device-grant tests can point `device_authorization_endpoint` at a local mock
    /// server and never touch the network for anything else.
    fn provider(device_authorization_endpoint: Option<Url>) -> OidcProvider {
        OidcProvider {
            inner: Arc::new(Inner {
                issuer: "https://idp.example".to_owned(),
                client_id: "client".to_owned(),
                redirect_uri: "https://controller.example/callback".to_owned(),
                authorization_endpoint: "https://idp.example/authorize"
                    .parse()
                    .expect("parse authorization endpoint"),
                token_endpoint: "https://idp.example/token"
                    .parse()
                    .expect("parse token endpoint"),
                userinfo_endpoint: "https://idp.example/userinfo"
                    .parse()
                    .expect("parse userinfo endpoint"),
                device_authorization_endpoint,
                jwks: cache(jwks(&[])),
                http: reqwest::Client::new(),
                pending: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Starts a one-shot HTTP server that counts requests and always answers with
    /// `body`, so a test can assert how many times (zero, in the capacity test) the
    /// IdP was actually contacted.
    async fn count_requests_server(
        status: StatusCode,
        body: &'static str,
    ) -> (Url, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock IdP listener");
        let addr = listener.local_addr().expect("mock IdP local addr");
        let counted = hits.clone();
        let app = axum::Router::new().route(
            "/device_authorization",
            axum::routing::post(move || {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    (status, body)
                }
            }),
        );
        let handle = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve mock IdP device_authorization endpoint");
        });
        let url = format!("http://{addr}/device_authorization")
            .parse()
            .expect("parse mock IdP URL");
        (url, hits, handle)
    }

    // Regression test for the actual bug this fix targets. A slot used to be reserved
    // (MAX_PENDING_ENROLLMENTS checked) only AFTER the IdP had already answered, so
    // once the pending map was full, every caller's device-grant request still
    // reached the IdP — and only got rejected on the insert afterwards. The IdP
    // request must never happen at all once the map is full.
    #[tokio::test]
    async fn begin_device_reserves_capacity_before_contacting_the_idp() {
        let (endpoint, hits, server) =
            count_requests_server(StatusCode::OK, "unused — must not be reached").await;
        let provider = provider(Some(endpoint));
        {
            let mut pending = provider.inner.pending.lock().await;
            for i in 0..MAX_PENDING_ENROLLMENTS {
                pending.insert(
                    format!("filler-{i}"),
                    PendingEnrollment {
                        hostname: "filler".to_owned(),
                        nonce: None,
                        expires_at: Instant::now() + Duration::from_secs(600),
                    },
                );
            }
        }

        let result = provider.begin_device("new-host".to_owned()).await;

        assert!(
            result.is_err(),
            "a full pending map must reject begin_device"
        );
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the IdP must not be contacted at all once MAX_PENDING_ENROLLMENTS is reached"
        );
        server.abort();
    }

    // The companion success path: the reservation made before the IdP call must still
    // end up in `pending` with the real (possibly shorter) device-grant lifetime, not
    // left at the ENROLLMENT_LIFETIME placeholder it was reserved with.
    #[tokio::test]
    async fn begin_device_narrows_reservation_to_the_real_device_lifetime() {
        let device_body = json!({
            "device_code": "device-code",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://idp.example/activate",
            "expires_in": 120,
        })
        .to_string();
        let (endpoint, hits, server) =
            count_requests_server(StatusCode::OK, Box::leak(device_body.into_boxed_str())).await;
        let provider = provider(Some(endpoint));

        let response = provider
            .begin_device("new-host".to_owned())
            .await
            .expect("begin_device succeeds");

        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(response.expires_in_seconds, 120);
        let pending = provider.inner.pending.lock().await;
        let entry = pending
            .get(&response.enrollment_id)
            .expect("reservation is still present under the real enrollment id");
        let remaining = entry.expires_at.saturating_duration_since(Instant::now());
        assert!(
            remaining <= Duration::from_secs(120) && remaining > Duration::from_secs(100),
            "reservation must be narrowed to the device grant's own expires_in (120s), \
             not left at the ENROLLMENT_LIFETIME placeholder (600s); got {remaining:?}"
        );
        server.abort();
    }

    // The IdP request itself must still fail closed: if it errors or returns an
    // unreadable body, the reservation made for it must be released, not left behind
    // consuming a MAX_PENDING_ENROLLMENTS slot forever.
    #[tokio::test]
    async fn begin_device_releases_the_reservation_when_the_idp_call_fails() {
        let (endpoint, hits, server) =
            count_requests_server(StatusCode::BAD_REQUEST, "idp rejected the request").await;
        let provider = provider(Some(endpoint));

        let result = provider.begin_device("new-host".to_owned()).await;

        assert!(
            result.is_err(),
            "a rejected IdP call must fail begin_device"
        );
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(
            provider.inner.pending.lock().await.is_empty(),
            "the reservation must be released, not left occupying a slot"
        );
        server.abort();
    }

    #[tokio::test]
    async fn unknown_key_refreshes_and_replaces_jwks() {
        let cache = cache(jwks(&["old"]));
        let fetches = Arc::new(AtomicUsize::new(0));
        let first_fetch = fetches.clone();

        let key = cache
            .key_for_with("rotated", move || async move {
                first_fetch.fetch_add(1, Ordering::SeqCst);
                Ok(jwks(&["rotated"]))
            })
            .await
            .expect("find rotated key after refresh");
        let cached_fetch = fetches.clone();
        cache
            .key_for_with("rotated", move || async move {
                cached_fetch.fetch_add(1, Ordering::SeqCst);
                Ok(jwks(&["unused"]))
            })
            .await
            .expect("reuse refreshed key");

        DecodingKey::from_jwk(&key).expect("construct rotated decoding key");
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
        assert!(cache.cached_key("old").is_none());
    }

    #[tokio::test]
    async fn concurrent_unknown_key_requests_share_one_refresh() {
        let cache = Arc::new(cache(jwks(&["old"])));
        let fetches = Arc::new(AtomicUsize::new(0));
        let mut requests = Vec::new();
        for _ in 0..16 {
            let cache = cache.clone();
            let fetches = fetches.clone();
            requests.push(tokio::spawn(async move {
                cache
                    .key_for_with("rotated", move || async move {
                        fetches.fetch_add(1, Ordering::SeqCst);
                        yield_now().await;
                        Ok(jwks(&["rotated"]))
                    })
                    .await
            }));
        }

        for request in requests {
            request
                .await
                .expect("join key lookup")
                .expect("find rotated key");
        }

        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_refresh_retains_keys_and_rate_limits_other_unknown_keys() {
        let cache = cache(jwks(&["known"]));
        let fetches = Arc::new(AtomicUsize::new(0));
        let failed_fetch = fetches.clone();

        cache
            .key_for_with("unknown-one", move || async move {
                failed_fetch.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::anyhow!("JWKS unavailable"))
            })
            .await
            .expect_err("JWKS endpoint fails");
        let cached_fetch = fetches.clone();
        cache
            .key_for_with("known", move || async move {
                cached_fetch.fetch_add(1, Ordering::SeqCst);
                Ok(jwks(&["unused"]))
            })
            .await
            .expect("old cached key remains available");
        let suppressed_fetch = fetches.clone();
        let suppressed = cache
            .key_for_with("unknown-two", move || async move {
                suppressed_fetch.fetch_add(1, Ordering::SeqCst);
                Ok(jwks(&["unused"]))
            })
            .await
            .expect_err("failed refresh starts cooldown");

        assert!(format!("{suppressed:#}").contains("rate limited"));
        assert_eq!(fetches.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn refresh_request_forces_revalidation_and_bounds_the_gate() {
        let uri = "https://idp.example/keys".parse().expect("parse JWKS URL");
        let request = refresh_jwks_request(&reqwest::Client::new(), &uri)
            .build()
            .expect("build JWKS refresh request");

        assert_eq!(
            request.headers().get(reqwest::header::CACHE_CONTROL),
            Some(&reqwest::header::HeaderValue::from_static("no-cache"))
        );
        assert_eq!(request.timeout(), Some(&JWKS_FETCH_TIMEOUT));
    }
}
