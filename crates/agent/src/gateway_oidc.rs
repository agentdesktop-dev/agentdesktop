use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use agentdesktop_core::http::ClientExt;
use agentdesktop_core::model::{LlmGatewayCredential, LlmGatewayLoginStatus};
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use url::Url;

use crate::{oidc, secret_store::SecretStore};

const SECRET_SERVICE: &str = "dev.agentdesktop.gateway-oidc";
const EXPIRY_SKEW_SECONDS: u64 = 60;
static LOGIN: Mutex<()> = Mutex::const_new(());
/// Device authorization sign-ins waiting for approval, keyed by token account.
static PENDING_DEVICE_LOGINS: std::sync::Mutex<BTreeMap<String, PendingDeviceLogin>> =
    std::sync::Mutex::new(BTreeMap::new());
/// One lock per token account, held only while a device grant is being requested from
/// the identity provider. It is what stops two concurrent `login` calls from each
/// starting a grant, without holding the global [`LOGIN`] lock across network I/O.
static STARTING_DEVICE_LOGINS: StdMutex<BTreeMap<String, Arc<Mutex<()>>>> =
    StdMutex::new(BTreeMap::new());
/// Upper bound for the identity provider's device-grant discovery and request. The
/// default HTTP client has no timeout, so without this a stalled provider would hold
/// the per-account start lock (and every `login` queued behind it) forever.
const DEVICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest a device code is honoured, whatever the provider advertises. RFC 8628
/// examples use 1800 seconds; this also keeps `Instant + expires_in` from overflowing
/// on a hostile or broken `expires_in`.
const MAX_DEVICE_LIFETIME: Duration = Duration::from_secs(30 * 60);
/// Longest poll interval honoured, for the same reason.
const MAX_DEVICE_POLL_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct LoginOptions {
    pub callback_listen: Option<SocketAddr>,
    pub subscription_available: bool,
    pub github_client_id: Option<String>,
    /// Never open a browser: return [`SignInRequired`] instead when no valid
    /// or refreshable token exists. Sign in with [`device_login`].
    pub device_authorization: bool,
}

/// The gateway token is missing or expired and could not be refreshed, and
/// sign-in uses device authorization, which needs the user to act.
#[derive(Debug)]
pub struct SignInRequired {
    pending: Option<PendingDeviceLogin>,
}

impl std::fmt::Display for SignInRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LLM gateway sign-in required: run `agentdesktop-headless login`")?;
        if let Some(pending) = &self.pending {
            write!(
                formatter,
                ", or approve the pending sign-in at {} with code {}",
                pending.verification_url, pending.user_code
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for SignInRequired {}

#[derive(Clone, Debug)]
struct PendingDeviceLogin {
    verification_url: String,
    user_code: String,
}

pub struct CredentialAcquisition {
    pub credential: LlmGatewayCredential,
    pub interactive: bool,
}

#[derive(Deserialize)]
struct ProviderMetadata {
    authorization_endpoint: Url,
    token_endpoint: Url,
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

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredTokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_at_unix_seconds: u64,
    token_endpoint: String,
}

pub async fn credential(
    issuer: &Url,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    allow_insecure: bool,
    state_dir: &Path,
    login: LoginOptions,
) -> anyhow::Result<CredentialAcquisition> {
    let issuer = issuer.clone();
    let client_id = client_id.to_owned();
    let redirect_uri = redirect_uri.to_owned();
    let scopes = scopes.to_owned();
    let state_dir = state_dir.to_owned();
    tokio::spawn(async move {
        credential_inner(
            &issuer,
            &client_id,
            &redirect_uri,
            &scopes,
            allow_insecure,
            &state_dir,
            login,
        )
        .await
    })
    .await
    .context("join gateway OIDC credential task")?
}

async fn credential_inner(
    issuer: &Url,
    client_id: &str,
    redirect_uri: &str,
    scopes: &[String],
    allow_insecure: bool,
    state_dir: &Path,
    login: LoginOptions,
) -> anyhow::Result<CredentialAcquisition> {
    let _login = LOGIN.lock().await;
    let store = SecretStore::new(state_dir)?;
    let account = account(issuer, client_id);
    if let Some(credential) = stored_credential(&store, &account, client_id).await? {
        return Ok(CredentialAcquisition {
            credential,
            interactive: false,
        });
    }
    if login.device_authorization {
        return Err(SignInRequired {
            pending: pending_device_login(&account),
        }
        .into());
    }

    let metadata = discover(issuer, allow_insecure).await?;
    let redirect_uri = Url::parse(redirect_uri).context("parse OIDC redirect URI")?;
    let state = oidc::random_secret();
    let (verifier, challenge) = oidc::pkce();
    let authorization_url = authorization_url(
        metadata.authorization_endpoint,
        client_id,
        &redirect_uri,
        scopes,
        &state,
        &challenge,
    );
    let authorization_code = oidc::wait_for_authorization_code_with_page(
        authorization_url.as_str(),
        &redirect_uri,
        state,
        login.callback_listen,
        oidc::AuthorizationPage::Identity {
            subscription_available: login.subscription_available,
            github_available: login.github_client_id.is_some(),
        },
        true,
    )
    .await?
    .context("required identity authorization was skipped")?;
    let tokens = oidc::exchange_authorization_code(
        metadata.token_endpoint.as_str(),
        client_id,
        redirect_uri.as_str(),
        &authorization_code,
        &verifier,
    )
    .await?;
    let stored = StoredTokens {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_unix_seconds: now().saturating_add(tokens.expires_in),
        token_endpoint: metadata.token_endpoint.to_string(),
    };
    save(&store, &account, &stored)?;
    if let Some(client_id) = login
        .github_client_id
        .filter(|_| !login.subscription_available)
    {
        crate::github_oauth::credential(
            &client_id,
            state_dir,
            Some((redirect_uri, login.callback_listen)),
        )
        .await?;
    }
    Ok(CredentialAcquisition {
        credential: as_credential(&stored),
        interactive: true,
    })
}

/// Returns the stored access token, refreshing it first if it is about to
/// expire. Returns `None` when the user has to sign in again.
async fn stored_credential(
    store: &SecretStore,
    account: &str,
    client_id: &str,
) -> anyhow::Result<Option<LlmGatewayCredential>> {
    let Some(mut tokens) = load(store, account)? else {
        return Ok(None);
    };
    if tokens.expires_at_unix_seconds > now().saturating_add(EXPIRY_SKEW_SECONDS) {
        return Ok(Some(as_credential(&tokens)));
    }
    let Some(refresh_token) = tokens.refresh_token.as_deref() else {
        return Ok(None);
    };
    match oidc::refresh_access_token(&tokens.token_endpoint, client_id, refresh_token).await {
        Ok(refreshed) => {
            tokens.access_token = refreshed.access_token;
            tokens.refresh_token = refreshed.refresh_token.or(tokens.refresh_token.take());
            tokens.expires_at_unix_seconds = now().saturating_add(refreshed.expires_in);
            save(store, account, &tokens)?;
            Ok(Some(as_credential(&tokens)))
        }
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "OIDC token refresh failed; signing in again");
            Ok(None)
        }
    }
}

/// Signs in with the OAuth 2.0 Device Authorization Grant (RFC 8628).
///
/// Returns immediately with the verification URL and user code. The daemon
/// polls the token endpoint in the background and stores the tokens once the
/// user approves the request from any device. Calling this again while a
/// sign-in is pending returns the same code instead of starting another one.
pub async fn device_login(
    issuer: &Url,
    client_id: &str,
    scopes: &[String],
    allow_insecure: bool,
    state_dir: &Path,
) -> anyhow::Result<LlmGatewayLoginStatus> {
    let account = account(issuer, client_id);
    if let Some(status) = device_login_status(&account, client_id, state_dir).await? {
        return Ok(status);
    }

    // Serialise starting a grant per account, NOT with the global LOGIN lock: the
    // provider round-trip below can be slow, and holding LOGIN across it would block
    // every concurrent /credential and /login call. Callers that lose the race wait
    // here, then find the winner's pending grant in the re-check and return it, so
    // no duplicate grant is ever requested.
    let start_lock = starting_lock(&account);
    let _starting = start_lock.lock().await;
    if let Some(status) = device_login_status(&account, client_id, state_dir).await? {
        return Ok(status);
    }

    let (metadata, device) = tokio::time::timeout(
        DEVICE_REQUEST_TIMEOUT,
        request_device_grant(issuer, client_id, scopes, allow_insecure),
    )
    .await
    .context("OIDC device authorization timed out")??;
    let pending = pending_from(&device);
    tracing::info!(
        verification_uri = %device.verification_uri,
        verification_uri_complete = device.verification_uri_complete.as_deref().unwrap_or_default(),
        user_code = %device.user_code,
        "approve LLM gateway sign-in: open the verification URL on any device and confirm the code"
    );
    lock_pending().insert(account.clone(), pending.clone());

    let client_id = client_id.to_owned();
    let token_endpoint = metadata.token_endpoint.to_string();
    let state_dir = state_dir.to_owned();
    tokio::spawn(async move {
        let result =
            complete_device_login(&device, &token_endpoint, &client_id, &state_dir, &account).await;
        lock_pending().remove(&account);
        match result {
            Ok(()) => tracing::info!("LLM gateway device authorization sign-in complete"),
            Err(error) => tracing::error!(
                error = %format!("{error:#}"),
                "LLM gateway device authorization sign-in failed"
            ),
        }
    });
    Ok(awaiting(pending))
}

/// Starts a device sign-in for the gateway if `config` asks for one, in the
/// background, logging the verification URL and code. Safe to call on every
/// configuration apply: [`device_login`] returns the existing pending grant, or
/// `signedIn`, instead of starting another.
///
/// Managed `llmGateway` policy arrives after the daemon has started (from the cached
/// controller configuration, then from each update), so the startup path that only
/// sees the local file cannot be the only place a grant is started.
pub fn start_device_login_if_configured(
    config: &agentdesktop_core::config::DaemonConfig,
    state_dir: &Path,
) {
    let Some(agentdesktop_core::config::LlmGatewayAuthentication::Oidc {
        issuer,
        client_id,
        scopes,
        allow_insecure,
        device_authorization: true,
        ..
    }) = config
        .llm_gateway
        .as_ref()
        .and_then(|gateway| gateway.authentication.as_ref())
    else {
        return;
    };
    let (issuer, client_id, scopes, allow_insecure, state_dir) = (
        issuer.clone(),
        client_id.clone(),
        scopes.clone(),
        *allow_insecure,
        state_dir.to_owned(),
    );
    tokio::spawn(async move {
        tracing::info!(%issuer, "starting LLM gateway OIDC device authorization");
        if let Err(error) =
            device_login(&issuer, &client_id, &scopes, allow_insecure, &state_dir).await
        {
            tracing::error!(
                error = %format!("{error:#}"),
                "LLM gateway device authorization failed to start"
            );
        }
    });
}

/// `signedIn` or the pending grant for `account`, if there is one. Takes the global
/// lock only for the duration of the (local) token-store read.
async fn device_login_status(
    account: &str,
    client_id: &str,
    state_dir: &Path,
) -> anyhow::Result<Option<LlmGatewayLoginStatus>> {
    let _login = LOGIN.lock().await;
    let store = SecretStore::new(state_dir)?;
    if stored_credential(&store, account, client_id)
        .await?
        .is_some()
    {
        return Ok(Some(signed_in()));
    }
    Ok(pending_device_login(account).map(awaiting))
}

fn starting_lock(account: &str) -> Arc<Mutex<()>> {
    STARTING_DEVICE_LOGINS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(account.to_owned())
        .or_default()
        .clone()
}

/// Discovery plus the device-authorization request, with the response validated.
async fn request_device_grant(
    issuer: &Url,
    client_id: &str,
    scopes: &[String],
    allow_insecure: bool,
) -> anyhow::Result<(ProviderMetadata, DeviceAuthorizationResponse)> {
    let metadata = discover(issuer, allow_insecure).await?;
    let endpoint = metadata
        .device_authorization_endpoint
        .clone()
        .context("OIDC provider does not support device authorization")?;
    let device = reqwest::Client::new()
        .post(endpoint)
        .form(&device_authorization_form(client_id, scopes))
        .send()
        .await
        .context("request OIDC device authorization")?
        .error_for_status()
        .context("OIDC device authorization endpoint returned an error")?
        .json::<DeviceAuthorizationResponse>()
        .await
        .context("decode OIDC device authorization response")?;
    Ok((metadata, validate_device(device)?))
}

/// Rejects a device-authorization response that could not be acted on, and caps its
/// timing. Mirrors the controller's `device_enrollment_response` checks: an empty code
/// or a non-URL verification address would publish an unusable `awaitingAuthentication`
/// status, and an unbounded `expires_in` overflows `Instant + Duration` in the
/// background poll task, which would panic before it cleared the pending entry.
fn validate_device(
    mut device: DeviceAuthorizationResponse,
) -> anyhow::Result<DeviceAuthorizationResponse> {
    if device.device_code.is_empty() || device.user_code.is_empty() {
        bail!("OIDC device authorization response has no device or user code");
    }
    Url::parse(&device.verification_uri).context("OIDC device verification URI is invalid")?;
    device.verification_uri_complete = device
        .verification_uri_complete
        .filter(|uri| Url::parse(uri).is_ok());
    device.expires_in = device.expires_in.min(MAX_DEVICE_LIFETIME.as_secs());
    if device.expires_in == 0 {
        bail!("OIDC device authorization has already expired");
    }
    Ok(device)
}

async fn complete_device_login(
    device: &DeviceAuthorizationResponse,
    token_endpoint: &str,
    client_id: &str,
    state_dir: &Path,
    account: &str,
) -> anyhow::Result<()> {
    let tokens = oidc::poll_device_token(
        token_endpoint,
        client_id,
        &device.device_code,
        poll_interval(device),
        Duration::from_secs(device.expires_in),
    )
    .await?;
    let _login = LOGIN.lock().await;
    let store = SecretStore::new(state_dir)?;
    save(
        &store,
        account,
        &StoredTokens {
            access_token: tokens.access_token,
            refresh_token: tokens.refresh_token,
            expires_at_unix_seconds: now().saturating_add(tokens.expires_in),
            token_endpoint: token_endpoint.to_owned(),
        },
    )
}

fn device_authorization_form<'a>(client_id: &'a str, scopes: &[String]) -> [(&'a str, String); 2] {
    [
        ("client_id", client_id.to_owned()),
        ("scope", scopes.join(" ")),
    ]
}

/// Prefers `verification_uri_complete`, which embeds the user code, so the
/// user only has to open one link.
fn pending_from(device: &DeviceAuthorizationResponse) -> PendingDeviceLogin {
    PendingDeviceLogin {
        verification_url: device
            .verification_uri_complete
            .clone()
            .filter(|url| !url.is_empty())
            .unwrap_or_else(|| device.verification_uri.clone()),
        user_code: device.user_code.clone(),
    }
}

fn poll_interval(device: &DeviceAuthorizationResponse) -> Duration {
    device
        .interval
        .filter(|seconds| *seconds > 0)
        .map_or(oidc::DEFAULT_DEVICE_POLL_INTERVAL, Duration::from_secs)
        .min(MAX_DEVICE_POLL_INTERVAL)
}

fn lock_pending() -> std::sync::MutexGuard<'static, BTreeMap<String, PendingDeviceLogin>> {
    PENDING_DEVICE_LOGINS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn pending_device_login(account: &str) -> Option<PendingDeviceLogin> {
    lock_pending().get(account).cloned()
}

fn signed_in() -> LlmGatewayLoginStatus {
    LlmGatewayLoginStatus {
        status: "signedIn".to_owned(),
        verification_url: None,
        user_code: None,
    }
}

fn awaiting(pending: PendingDeviceLogin) -> LlmGatewayLoginStatus {
    LlmGatewayLoginStatus {
        status: "awaitingAuthentication".to_owned(),
        verification_url: Some(pending.verification_url),
        user_code: Some(pending.user_code),
    }
}

async fn discover(issuer: &Url, allow_insecure: bool) -> anyhow::Result<ProviderMetadata> {
    let endpoint = format!(
        "{}/.well-known/openid-configuration",
        issuer.as_str().trim_end_matches('/')
    );
    let metadata = reqwest::Client::new()
        .get_json::<ProviderMetadata>(endpoint)
        .await
        .context("discover OIDC provider")?;
    for (name, endpoint) in [
        ("authorization", Some(&metadata.authorization_endpoint)),
        ("token", Some(&metadata.token_endpoint)),
        (
            "device authorization",
            metadata.device_authorization_endpoint.as_ref(),
        ),
    ] {
        let Some(endpoint) = endpoint else {
            continue;
        };
        let secure = endpoint.scheme() == "https";
        let insecure_loopback = allow_insecure
            && endpoint.scheme() == "http"
            && endpoint
                .host_str()
                .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1" | "[::1]"));
        if endpoint.host().is_none() || (!secure && !insecure_loopback) {
            bail!("OIDC {name} endpoint must be HTTPS or explicitly allowed loopback HTTP");
        }
    }
    Ok(metadata)
}

fn authorization_url(
    mut endpoint: Url,
    client_id: &str,
    redirect_uri: &Url,
    scopes: &[String],
    state: &str,
    challenge: &str,
) -> Url {
    endpoint.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri.as_str()),
        ("scope", scopes.join(" ").as_str()),
        ("state", state),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
    ]);
    endpoint
}

fn load(store: &SecretStore, account: &str) -> anyhow::Result<Option<StoredTokens>> {
    store
        .get_optional(SECRET_SERVICE, account)?
        .map(|value| serde_json::from_str(&value).context("decode stored gateway OIDC token"))
        .transpose()
}

fn save(store: &SecretStore, account: &str, tokens: &StoredTokens) -> anyhow::Result<()> {
    store.set(
        SECRET_SERVICE,
        account,
        &serde_json::to_string(tokens).context("encode gateway OIDC token")?,
    )
}

fn account(issuer: &Url, client_id: &str) -> String {
    let digest = Sha256::digest(format!("{issuer}\0{client_id}").as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

fn as_credential(tokens: &StoredTokens) -> LlmGatewayCredential {
    LlmGatewayCredential {
        credential: tokens.access_token.clone(),
        expires_at_unix_seconds: tokens.expires_at_unix_seconds,
    }
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use url::Url;

    use super::{
        DeviceAuthorizationResponse, LOGIN, MAX_DEVICE_LIFETIME, MAX_DEVICE_POLL_INTERVAL,
        PendingDeviceLogin, SignInRequired, authorization_url, device_authorization_form,
        device_login, lock_pending, pending_from, poll_interval, validate_device,
    };

    fn device(value: serde_json::Value) -> DeviceAuthorizationResponse {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn device_response_validation_rejects_unusable_grants() {
        let valid = serde_json::json!({
            "device_code": "device",
            "user_code": "ABCD-EFGH",
            "verification_uri": "https://idp.example/activate",
            "expires_in": 600,
        });
        assert!(validate_device(device(valid.clone())).is_ok());
        for (field, value) in [
            ("device_code", serde_json::json!("")),
            ("user_code", serde_json::json!("")),
            ("verification_uri", serde_json::json!("not a url")),
            ("expires_in", serde_json::json!(0)),
        ] {
            let mut invalid = valid.clone();
            invalid[field] = value;
            assert!(
                validate_device(device(invalid)).is_err(),
                "{field} should be rejected"
            );
        }
    }

    #[test]
    fn device_response_validation_bounds_timing_and_drops_bad_complete_uri() {
        let validated = validate_device(device(serde_json::json!({
            "device_code": "device",
            "user_code": "CODE",
            "verification_uri": "https://idp.example/activate",
            "verification_uri_complete": "not a url",
            // Large enough to overflow Instant + Duration if used as given.
            "expires_in": u64::MAX,
            "interval": u64::MAX,
        })))
        .unwrap();
        assert_eq!(validated.expires_in, MAX_DEVICE_LIFETIME.as_secs());
        assert!(validated.verification_uri_complete.is_none());
        assert_eq!(poll_interval(&validated), MAX_DEVICE_POLL_INTERVAL);
        // The user is sent to the plain verification URI, not the malformed one.
        assert_eq!(
            pending_from(&validated).verification_url,
            "https://idp.example/activate"
        );
    }

    /// Serves OIDC discovery plus a device-authorization endpoint on loopback. `delay`
    /// holds the device-authorization response back; `requests` counts the grants asked for.
    async fn mock_idp(delay: Duration, requests: Arc<AtomicUsize>) -> Url {
        use axum::{Json, Router, routing::get, routing::post};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let discovery = serde_json::json!({
            "authorization_endpoint": format!("{base}/authorize"),
            "token_endpoint": format!("{base}/token"),
            "device_authorization_endpoint": format!("{base}/device"),
        });
        let app = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || {
                    let discovery = discovery.clone();
                    async move { Json(discovery) }
                }),
            )
            .route(
                "/device",
                post(move || {
                    let requests = requests.clone();
                    async move {
                        requests.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(delay).await;
                        Json(serde_json::json!({
                            "device_code": "device",
                            "user_code": "ABCD-EFGH",
                            "verification_uri": "https://idp.example/activate",
                            "expires_in": 600,
                            "interval": 60,
                        }))
                    }
                }),
            )
            // Never approve: the background poll just keeps waiting.
            .route(
                "/token",
                post(|| async {
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        r#"{"error":"authorization_pending"}"#,
                    )
                }),
            );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Url::parse(&base).unwrap()
    }

    // Regression test for two findings at once. Concurrent `login` calls must share ONE
    // grant (an obvious fix for the lock problem — just dropping the lock — would start
    // several), and a slow provider must not hold the global LOGIN lock, which every
    // /credential call takes.
    #[tokio::test]
    async fn concurrent_logins_share_one_grant_without_blocking_the_global_lock() {
        let requests = Arc::new(AtomicUsize::new(0));
        let issuer = mock_idp(Duration::from_millis(600), requests.clone()).await;
        let state_dir = tempfile::tempdir().unwrap();

        let logins: Vec<_> = (0..4)
            .map(|_| {
                let issuer = issuer.clone();
                let state_dir = state_dir.path().to_owned();
                tokio::spawn(async move {
                    device_login(&issuer, "client", &["openid".to_owned()], true, &state_dir).await
                })
            })
            .collect();

        // While the provider is still thinking, the global lock must be free.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), LOGIN.lock())
                .await
                .is_ok(),
            "device_login held the global LOGIN lock across the provider round-trip"
        );

        for login in logins {
            let status = login.await.unwrap().expect("login succeeds");
            assert_eq!(status.status, "awaitingAuthentication");
            assert_eq!(status.user_code.as_deref(), Some("ABCD-EFGH"));
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            1,
            "concurrent logins must share a single device grant"
        );
        lock_pending().clear();
    }

    // A provider that never answers must fail the login rather than hold the per-account
    // start lock, and every login queued behind it, forever.
    #[tokio::test(start_paused = true)]
    async fn stalled_provider_times_out_instead_of_wedging_login() {
        let requests = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer = Url::parse(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        tokio::spawn(async move {
            // Accept and hold every connection without answering.
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                requests.fetch_add(1, Ordering::SeqCst);
                held.push(socket);
            }
        });
        let state_dir = tempfile::tempdir().unwrap();
        let result = device_login(&issuer, "stalled", &[], true, state_dir.path()).await;
        assert!(result.is_err(), "a stalled provider must fail the login");
    }

    #[test]
    fn authorization_request_uses_pkce_and_configured_scopes() {
        let url = authorization_url(
            Url::parse("https://idp.example/authorize").unwrap(),
            "agentdesktop",
            &Url::parse("http://127.0.0.1:51327/callback").unwrap(),
            &["openid".to_owned(), "offline_access".to_owned()],
            "state",
            "challenge",
        );
        let parameters: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(parameters["response_type"], "code");
        assert_eq!(parameters["scope"], "openid offline_access");
        assert_eq!(parameters["code_challenge_method"], "S256");
        assert_eq!(parameters["code_challenge"], "challenge");
    }

    #[test]
    fn device_authorization_request_sends_client_id_and_configured_scopes() {
        let form = device_authorization_form(
            "agentdesktop",
            &["openid".to_owned(), "offline_access".to_owned()],
        );
        assert_eq!(
            form,
            [
                ("client_id", "agentdesktop".to_owned()),
                ("scope", "openid offline_access".to_owned()),
            ]
        );
    }

    #[test]
    fn device_authorization_prefers_complete_verification_uri() {
        let device: DeviceAuthorizationResponse = serde_json::from_str(
            r#"{
                "device_code": "device",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://idp.example/activate",
                "verification_uri_complete": "https://idp.example/activate?user_code=ABCD-EFGH",
                "expires_in": 600,
                "interval": 10
            }"#,
        )
        .unwrap();
        let pending = pending_from(&device);
        assert_eq!(
            pending.verification_url,
            "https://idp.example/activate?user_code=ABCD-EFGH"
        );
        assert_eq!(pending.user_code, "ABCD-EFGH");
        assert_eq!(poll_interval(&device), std::time::Duration::from_secs(10));
    }

    #[test]
    fn device_authorization_falls_back_to_verification_uri_and_default_interval() {
        let device: DeviceAuthorizationResponse = serde_json::from_str(
            r#"{
                "device_code": "device",
                "user_code": "ABCD-EFGH",
                "verification_uri": "https://idp.example/activate",
                "expires_in": 600
            }"#,
        )
        .unwrap();
        assert_eq!(
            pending_from(&device).verification_url,
            "https://idp.example/activate"
        );
        assert_eq!(
            poll_interval(&device),
            crate::oidc::DEFAULT_DEVICE_POLL_INTERVAL
        );
    }

    #[test]
    fn sign_in_required_names_the_login_command_and_pending_code() {
        assert_eq!(
            SignInRequired { pending: None }.to_string(),
            "LLM gateway sign-in required: run `agentdesktop-headless login`"
        );
        let pending = SignInRequired {
            pending: Some(PendingDeviceLogin {
                verification_url: "https://idp.example/activate".to_owned(),
                user_code: "ABCD-EFGH".to_owned(),
            }),
        };
        assert_eq!(
            pending.to_string(),
            "LLM gateway sign-in required: run `agentdesktop-headless login`, or approve the \
             pending sign-in at https://idp.example/activate with code ABCD-EFGH"
        );
    }
}
