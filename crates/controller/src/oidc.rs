use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use agentdesktop_core::http::ClientExt;
use agentdesktop_proto::fleet::LoginConfiguration;
use anyhow::{Context, bail};
use jsonwebtoken::{
    DecodingKey, Validation, decode, decode_header,
    jwk::{Jwk, JwkSet},
};
use reqwest::header::CACHE_CONTROL;
use serde::Deserialize;
use tokio::sync::Mutex;
use url::Url;

const JWKS_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const JWKS_REFRESH_COOLDOWN: Duration = Duration::from_secs(30);
const ENROLLMENT_SCOPE: &str = "openid profile email offline_access";

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
}

struct JwksCache {
    http: reqwest::Client,
    uri: Url,
    keys: RwLock<JwkSet>,
    refresh: Mutex<Option<Instant>>,
    refresh_cooldown: Duration,
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
            }),
        })
    }

    pub fn login_configuration(&self) -> LoginConfiguration {
        LoginConfiguration {
            issuer: self.inner.issuer.clone(),
            authorization_endpoint: self.inner.authorization_endpoint.to_string(),
            token_endpoint: self.inner.token_endpoint.to_string(),
            client_id: self.inner.client_id.clone(),
            redirect_uri: self.inner.redirect_uri.clone(),
            device_authorization_endpoint: self
                .inner
                .device_authorization_endpoint
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            scopes: ENROLLMENT_SCOPE
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
        }
    }

    pub async fn authenticate_enrollment(
        &self,
        hostname: String,
        id_token: &str,
        access_token: &str,
    ) -> anyhow::Result<CompletedEnrollment> {
        if hostname.trim().is_empty()
            || hostname.len() > 255
            || hostname.chars().any(char::is_control)
        {
            bail!("invalid enrollment hostname");
        }
        if id_token.is_empty() || id_token.len() > 64 * 1024 || access_token.is_empty() {
            bail!("ID token and access token are required");
        }
        let header = decode_header(id_token).context("decode ID token header")?;
        let kid = header.kid.context("ID token has no key ID")?;
        let jwk = self.inner.jwks.key_for(&kid).await?;
        let key = DecodingKey::from_jwk(&jwk).context("construct ID token verification key")?;
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&[&self.inner.issuer]);
        validation.set_audience(&[&self.inner.client_id]);
        validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
        let claims = decode::<IdTokenClaims>(id_token, &key, &validation)
            .context("validate OIDC ID token")?
            .claims;
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
            hostname,
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
            .timeout(Duration::from_secs(15))
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

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use jsonwebtoken::DecodingKey;
    use serde_json::json;
    use tokio::task::yield_now;
    use url::Url;

    use super::{Inner, JWKS_FETCH_TIMEOUT, JwkSet, JwksCache, OidcProvider, refresh_jwks_request};

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
            }),
        }
    }

    pub(crate) async fn authenticated_provider() -> (
        OidcProvider,
        jsonwebtoken::EncodingKey,
        tokio::task::JoinHandle<()>,
    ) {
        use aws_lc_rs::signature::{Ed25519KeyPair, KeyPair as _};
        use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};

        let _ = jsonwebtoken::crypto::aws_lc::DEFAULT_PROVIDER.install_default();
        let private =
            Ed25519KeyPair::generate_pkcs8(&aws_lc_rs::rand::SystemRandom::new()).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(private.as_ref()).unwrap();
        let keys = serde_json::from_value(json!({"keys": [{
            "kty": "OKP", "crv": "Ed25519", "alg": "EdDSA", "kid": "login-test",
            "x": URL_SAFE_NO_PAD.encode(key.public_key().as_ref())
        }]}))
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/userinfo",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                match headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                {
                    Some("Bearer access-token") => Ok(axum::Json(json!({"sub": "user-1"}))),
                    Some("Bearer other-user") => Ok(axum::Json(json!({"sub": "user-2"}))),
                    _ => Err(axum::http::StatusCode::UNAUTHORIZED),
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut provider = provider(None);
        let inner = Arc::get_mut(&mut provider.inner).unwrap();
        inner.jwks = cache(keys);
        inner.userinfo_endpoint = format!("http://{address}/userinfo").parse().unwrap();
        (
            provider,
            jsonwebtoken::EncodingKey::from_ed_der(private.as_ref()),
            server,
        )
    }

    pub(crate) fn id_token(key: &jsonwebtoken::EncodingKey, claims: &serde_json::Value) -> String {
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
        header.kid = Some("login-test".to_owned());
        jsonwebtoken::encode(&header, claims, key).unwrap()
    }

    pub(crate) fn user_claims() -> serde_json::Value {
        json!({
            "iss": "https://idp.example", "aud": "client", "sub": "user-1",
            "exp": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() + 600,
        })
    }

    #[tokio::test]
    async fn enrollment_authenticates_user_without_a_prior_login_attempt() {
        let (provider, key, server) = authenticated_provider().await;
        let completed = provider
            .authenticate_enrollment(
                "host".to_owned(),
                &id_token(&key, &user_claims()),
                "access-token",
            )
            .await
            .unwrap();
        assert_eq!(completed.hostname, "host");
        assert_eq!(completed.issuer, "https://idp.example");
        assert_eq!(completed.subject, "user-1");
        assert_eq!(completed.idp_claims["sub"], "user-1");
        server.abort();
    }

    #[tokio::test]
    async fn enrollment_rejects_invalid_tokens_and_subject_mismatch() {
        let (provider, key, server) = authenticated_provider().await;
        let original = user_claims();
        for (field, value) in [
            ("iss", json!("https://other.example")),
            ("aud", json!("another-client")),
            ("sub", json!("user-2")),
            ("exp", json!(1)),
        ] {
            let mut claims = original.clone();
            claims[field] = value;
            assert!(
                provider
                    .authenticate_enrollment(
                        "host".to_owned(),
                        &id_token(&key, &claims),
                        "access-token",
                    )
                    .await
                    .is_err(),
                "{field} must be verified"
            );
        }
        let token = id_token(&key, &original);
        for (id_token, access_token) in [
            ("", "access-token"),
            ("not-a-jwt", "access-token"),
            (token.as_str(), ""),
            (token.as_str(), "rejected"),
            (token.as_str(), "other-user"),
        ] {
            assert!(
                provider
                    .authenticate_enrollment("host".to_owned(), id_token, access_token)
                    .await
                    .is_err()
            );
        }
        let mut tampered = token.into_bytes();
        let index = tampered.len() - 10;
        tampered[index] = if tampered[index] == b'A' { b'B' } else { b'A' };
        assert!(
            provider
                .authenticate_enrollment(
                    "host".to_owned(),
                    std::str::from_utf8(&tampered).unwrap(),
                    "access-token",
                )
                .await
                .is_err()
        );
        server.abort();
    }

    #[test]
    fn login_configuration_is_read_only_and_contains_no_attempt_state() {
        let configured = provider(Some("https://idp.example/device".parse().unwrap()));
        for _ in 0..2048 {
            let config = configured.login_configuration();
            assert_eq!(config.issuer, "https://idp.example");
            assert_eq!(config.client_id, "client");
            assert_eq!(
                config.device_authorization_endpoint,
                "https://idp.example/device"
            );
            assert_eq!(
                config.scopes,
                ["openid", "profile", "email", "offline_access"]
            );
        }
        assert!(
            provider(None)
                .login_configuration()
                .device_authorization_endpoint
                .is_empty()
        );
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
