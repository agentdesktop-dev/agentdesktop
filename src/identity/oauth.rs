use std::io;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine;
use oauth2::PkceCodeChallenge;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use url::Url;

use super::dpop::{Es256VerificationJwk, verify_es256_jwt};
use super::storage::CredentialStore;

#[derive(Clone, Debug)]
pub struct LoginConfig {
    pub issuer: Url,
    pub client_id: String,
    pub audience: String,
    pub scope: String,
    pub gateway_origin: Url,
}

#[derive(Clone, Debug, Deserialize)]
struct AuthorizationServerMetadata {
    issuer: Url,
    authorization_endpoint: Url,
    token_endpoint: Url,
    jwks_uri: Url,
    code_challenge_methods_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct JsonWebKeySet {
    keys: Vec<Es256VerificationJwk>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: u64,
    scope: String,
    refresh_token: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredSession {
    pub issuer: Url,
    pub gateway_origin: Url,
    pub client_id: String,
    pub audience: String,
    pub access_token: String,
    pub expires_at: u64,
    pub scope: String,
    pub refresh_token: String,
    #[serde(default)]
    pub generation: u64,
}

#[derive(Clone)]
pub struct ManagedIdentity {
    session: Arc<Mutex<StoredSession>>,
    store: CredentialStore,
}

pub struct ManagedCredentials {
    pub access_token: String,
    pub generation: u64,
}

impl StoredSession {
    pub fn is_expired(&self) -> Result<bool> {
        Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() >= self.expires_at)
    }
}

impl ManagedIdentity {
    pub fn new(session: StoredSession, store: CredentialStore) -> Self {
        Self {
            session: Arc::new(Mutex::new(session)),
            store,
        }
    }

    pub async fn credentials(&self) -> Result<ManagedCredentials> {
        let session = self.refreshed_session().await?;
        Ok(ManagedCredentials {
            access_token: session.access_token.clone(),
            generation: session.generation,
        })
    }

    pub async fn bearer_token(&self) -> Result<String> {
        Ok(self.refreshed_session().await?.access_token.clone())
    }

    async fn refreshed_session(&self) -> Result<tokio::sync::MutexGuard<'_, StoredSession>> {
        let mut session = self.session.lock().await;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        if session.expires_at <= now.saturating_add(30) {
            refresh_session(&mut session, &self.store).await?;
        }
        Ok(session)
    }

    pub async fn status(&self) -> Result<&'static str> {
        let session = self.session.lock().await;
        if session.is_expired()? {
            Ok("refresh-required")
        } else {
            Ok("ready")
        }
    }
}

pub async fn login<F>(
    config: &LoginConfig,
    store: &CredentialStore,
    authorize: F,
) -> Result<StoredSession>
where
    F: FnOnce(&Url) -> Result<()>,
{
    validate_issuer(&config.issuer)?;
    validate_gateway_origin(&config.gateway_origin)?;
    let client = reqwest::Client::new();
    let discovery_url = config
        .issuer
        .join(".well-known/oauth-authorization-server")?;
    let metadata: AuthorizationServerMetadata = client
        .get(discovery_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    validate_metadata(config, &metadata)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let redirect_uri = Url::parse(&format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    ))?;
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
    let state = random_secret();
    let authorization_url =
        authorization_url(config, &metadata, &redirect_uri, &pkce_challenge, &state);
    authorize(&authorization_url)?;
    let code = receive_callback(&listener, &state).await?;

    let token: TokenResponse = client
        .post(metadata.token_endpoint.clone())
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", config.client_id.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("code", code.as_str()),
            ("code_verifier", pkce_verifier.secret()),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if !token.token_type.eq_ignore_ascii_case("bearer") {
        bail!("authorization server returned token_type other than Bearer");
    }
    let key_set: JsonWebKeySet = client
        .get(metadata.jwks_uri)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let token_expiry = validate_access_token(config, &token.access_token, &key_set, now)?;
    validate_granted_scopes(&config.scope, &token.scope)?;
    let session = StoredSession {
        issuer: config.issuer.clone(),
        gateway_origin: config.gateway_origin.clone(),
        client_id: config.client_id.clone(),
        audience: config.audience.clone(),
        access_token: token.access_token,
        expires_at: token_expiry.min(now.saturating_add(token.expires_in)),
        scope: token.scope,
        refresh_token: token
            .refresh_token
            .context("authorization server did not issue a refresh token")?,
        generation: 1,
    };
    store.put(
        &session_record(&config.issuer, &config.gateway_origin),
        &serde_json::to_vec(&session)?,
    )?;
    Ok(session)
}

pub fn open_authorization_url(authorization_url: &Url) -> Result<()> {
    open_authorization_url_with(authorization_url, |url| open::that(url).map_err(Into::into))
}

fn open_authorization_url_with<F>(authorization_url: &Url, open_url: F) -> Result<()>
where
    F: FnOnce(&str) -> Result<()>,
{
    if let Err(error) = open_url(authorization_url.as_str()) {
        eprintln!("Could not open a browser automatically: {error}");
        println!("Open this URL in a browser on this device to continue:\n{authorization_url}");
    }
    Ok(())
}

fn validate_granted_scopes(requested: &str, granted: &str) -> Result<()> {
    let granted_scopes: std::collections::HashSet<_> = granted.split_whitespace().collect();
    if requested
        .split_whitespace()
        .any(|scope| !granted_scopes.contains(scope))
    {
        bail!("authorization server did not grant every requested scope");
    }
    Ok(())
}

async fn refresh_session(session: &mut StoredSession, store: &CredentialStore) -> Result<()> {
    let config = LoginConfig {
        issuer: session.issuer.clone(),
        client_id: session.client_id.clone(),
        audience: session.audience.clone(),
        scope: session.scope.clone(),
        gateway_origin: session.gateway_origin.clone(),
    };
    let client = reqwest::Client::new();
    let metadata: AuthorizationServerMetadata = client
        .get(
            config
                .issuer
                .join(".well-known/oauth-authorization-server")?,
        )
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    validate_metadata(&config, &metadata)?;
    let token: TokenResponse = client
        .post(metadata.token_endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", session.client_id.as_str()),
            ("refresh_token", session.refresh_token.as_str()),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if !token.token_type.eq_ignore_ascii_case("bearer") {
        bail!("authorization server returned token_type other than Bearer");
    }
    let key_set: JsonWebKeySet = client
        .get(metadata.jwks_uri)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let token_expiry = validate_access_token(&config, &token.access_token, &key_set, now)?;
    validate_granted_scopes(&config.scope, &token.scope)?;
    let refresh_token = token
        .refresh_token
        .context("authorization server did not rotate the refresh token")?;
    let refreshed = StoredSession {
        access_token: token.access_token,
        expires_at: token_expiry.min(now.saturating_add(token.expires_in)),
        scope: token.scope,
        refresh_token,
        generation: session.generation.saturating_add(1),
        ..session.clone()
    };
    store.put(
        &session_record(&refreshed.issuer, &refreshed.gateway_origin),
        &serde_json::to_vec(&refreshed)?,
    )?;
    *session = refreshed;
    Ok(())
}

pub fn load_session(config: &LoginConfig, store: &CredentialStore) -> Result<StoredSession> {
    load_session_for(&config.issuer, &config.gateway_origin, store)
}

pub fn load_session_for(
    issuer: &Url,
    gateway_origin: &Url,
    store: &CredentialStore,
) -> Result<StoredSession> {
    let session: StoredSession =
        serde_json::from_slice(&store.get(&session_record(issuer, gateway_origin))?)?;
    if session.issuer != *issuer || session.gateway_origin != *gateway_origin {
        bail!("stored identity session does not match configured issuer and gateway");
    }
    Ok(session)
}

pub fn delete_session_for(
    issuer: &Url,
    gateway_origin: &Url,
    store: &CredentialStore,
) -> Result<()> {
    validate_issuer(issuer)?;
    validate_gateway_origin(gateway_origin)?;
    store.delete(&session_record(issuer, gateway_origin))
}

fn validate_issuer(issuer: &Url) -> Result<()> {
    if issuer.scheme() == "https" {
        return Ok(());
    }
    let loopback = issuer.scheme() == "http"
        && issuer.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
    if !loopback {
        bail!("authorization-server issuer must use HTTPS unless it is loopback");
    }
    Ok(())
}

fn validate_gateway_origin(gateway_origin: &Url) -> Result<()> {
    if !matches!(gateway_origin.scheme(), "http" | "https")
        || gateway_origin.host_str().is_none()
        || gateway_origin.path() != "/"
        || gateway_origin.query().is_some()
        || gateway_origin.fragment().is_some()
    {
        bail!("gateway origin must be an HTTP(S) origin without a path, query, or fragment");
    }
    Ok(())
}

fn validate_metadata(config: &LoginConfig, metadata: &AuthorizationServerMetadata) -> Result<()> {
    if metadata.issuer != config.issuer {
        bail!("discovered issuer does not exactly match configured issuer");
    }
    if !metadata
        .code_challenge_methods_supported
        .iter()
        .any(|method| method == "S256")
    {
        bail!("authorization server does not advertise S256 PKCE");
    }
    Ok(())
}

fn authorization_url(
    config: &LoginConfig,
    metadata: &AuthorizationServerMetadata,
    redirect_uri: &Url,
    challenge: &PkceCodeChallenge,
    state: &str,
) -> Url {
    let mut url = metadata.authorization_endpoint.clone();
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &config.client_id)
        .append_pair("redirect_uri", redirect_uri.as_str())
        .append_pair("scope", &config.scope)
        .append_pair("state", state)
        .append_pair("code_challenge", challenge.as_str())
        .append_pair("code_challenge_method", "S256")
        .append_pair("audience", &config.audience);
    url
}

async fn receive_callback(listener: &TcpListener, expected_state: &str) -> Result<String> {
    let (mut stream, _) = listener.accept().await?;
    let mut request = Vec::new();
    loop {
        let mut buffer = [0_u8; 1024];
        let read = stream.read(&mut buffer).await?;
        if read == 0 || request.len() + read > 16 * 1024 {
            bail!("invalid OAuth callback request");
        }
        request.extend_from_slice(&buffer[..read]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    let request = std::str::from_utf8(&request)?;
    let target = request
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|line| line.strip_suffix(" HTTP/1.1"))
        .context("invalid OAuth callback request line")?;
    let callback = Url::parse(&format!("http://localhost{target}"))?;
    let state = callback
        .query_pairs()
        .find(|(name, _)| name == "state")
        .map(|(_, value)| value.into_owned())
        .context("OAuth callback is missing state")?;
    if state != expected_state {
        bail!("OAuth callback state does not match");
    }
    let code = callback
        .query_pairs()
        .find(|(name, _)| name == "code")
        .map(|(_, value)| value.into_owned())
        .context("OAuth callback is missing authorization code")?;
    const RESPONSE_BODY: &str = "Agent Desktop login complete.\n";
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{RESPONSE_BODY}",
        RESPONSE_BODY.len()
    );
    stream
        .write_all(response.as_bytes())
        .await
        .or_else(|error| {
            if error.kind() == io::ErrorKind::BrokenPipe {
                Ok(())
            } else {
                Err(error)
            }
        })?;
    Ok(code)
}

fn validate_access_token(
    config: &LoginConfig,
    token: &str,
    key_set: &JsonWebKeySet,
    now: u64,
) -> Result<u64> {
    let claims = verify_es256_jwt(token, &key_set.keys)?;
    if claims["iss"] != config.issuer.as_str() {
        bail!("access token issuer does not match configured issuer");
    }
    let audience_matches = claims["aud"] == config.audience
        || claims["aud"].as_array().is_some_and(|audiences| {
            audiences
                .iter()
                .any(|audience| audience == &config.audience)
        });
    if !audience_matches {
        bail!("access token audience does not contain configured gateway audience");
    }
    if claims["sub"].as_str().is_none_or(str::is_empty) {
        bail!("access token has no subject");
    }
    let expires_at = claims["exp"]
        .as_u64()
        .context("access token has no numeric expiry")?;
    if expires_at <= now {
        bail!("access token has expired");
    }
    Ok(expires_at)
}

fn session_record(issuer: &Url, gateway_origin: &Url) -> String {
    format!("{issuer}|{gateway_origin}|session")
}

fn random_secret() -> String {
    let mut value = [0_u8; 32];
    getrandom::fill(&mut value).expect("operating-system randomness is unavailable");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value)
}

#[cfg(test)]
mod tests {
    use super::{LoginConfig, open_authorization_url_with, validate_issuer};
    use anyhow::bail;
    use url::Url;

    #[test]
    fn permits_loopback_http_issuer_for_local_tests() {
        validate_issuer(&Url::parse("http://127.0.0.1:8080").unwrap()).unwrap();
    }

    #[test]
    fn rejects_remote_http_issuer() {
        let error = validate_issuer(&Url::parse("http://identity.example").unwrap()).unwrap_err();
        assert!(error.to_string().contains("must use HTTPS"));
    }

    #[test]
    fn browser_launch_failure_uses_manual_fallback() {
        let authorization_url = Url::parse("https://identity.example/authorize").unwrap();
        open_authorization_url_with(&authorization_url, |_| bail!("no desktop session")).unwrap();
    }

    #[allow(dead_code)]
    fn config() -> LoginConfig {
        LoginConfig {
            issuer: Url::parse("https://identity.example").unwrap(),
            client_id: "client".into(),
            audience: "gateway".into(),
            scope: "gateway.invoke".into(),
            gateway_origin: Url::parse("https://gateway.example").unwrap(),
        }
    }
}
