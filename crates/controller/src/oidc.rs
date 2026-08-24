use std::{
    collections::{BTreeMap, HashMap},
    future::Future,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

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
    userinfo_endpoint: Url,
    jwks_uri: Url,
}

#[derive(Deserialize)]
struct IdTokenClaims {
    sub: String,
    nonce: String,
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

        let jwks = fetch_jwks(http.get(document.jwks_uri.clone())).await?;

        Ok(Self {
            inner: Arc::new(Inner {
                issuer,
                client_id,
                redirect_uri,
                authorization_endpoint: document.authorization_endpoint,
                token_endpoint: document.token_endpoint,
                userinfo_endpoint: document.userinfo_endpoint,
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
            .append_pair("scope", "openid profile email offline_access")
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
            token_endpoint: self.inner.token_endpoint.to_string(),
            client_id: self.inner.client_id.clone(),
        })
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
        if claims.nonce != pending.nonce {
            bail!("OIDC nonce mismatch");
        }
        let access_subject = self.authenticate_access_token(access_token).await?;
        if access_subject != claims.sub {
            bail!("access token and ID token subjects do not match");
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

    pub async fn authenticate_access_token(&self, access_token: &str) -> anyhow::Result<String> {
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
        Ok(user.sub)
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

fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use jsonwebtoken::DecodingKey;
    use serde_json::json;
    use tokio::task::yield_now;

    use super::{JWKS_FETCH_TIMEOUT, JwkSet, JwksCache, refresh_jwks_request};

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
