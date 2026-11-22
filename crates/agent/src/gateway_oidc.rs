use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use agentdesktop_core::http::ClientExt;
use agentdesktop_core::model::{LlmGatewayCredential, LlmGatewayLoginStatus};
use anyhow::{Context, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, watch};
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
/// How long one credential request waits for a pending browser sign-in before
/// it returns [`BrowserSignInPending`]. The sign-in itself has no deadline.
#[cfg(not(test))]
const BROWSER_SIGN_IN_WAIT: Duration = Duration::from_secs(10 * 60);
#[cfg(test)]
const BROWSER_SIGN_IN_WAIT: Duration = Duration::from_millis(300);
/// How long after opening the sign-in page a new credential request may open
/// it again, in case the user closed it. The interval doubles on each reopen.
const BROWSER_REOPEN_INITIAL: Duration = Duration::from_secs(60 * 60);
const BROWSER_REOPEN_MAX: Duration = Duration::from_secs(8 * 60 * 60);
/// The browser sign-in in progress or last finished. There is at most one,
/// because every sign-in serves the same loopback callback port. Requests that
/// need a token while it is pending wait on it instead of opening a new tab.
static BROWSER_LOGIN: StdMutex<Option<BrowserLogin>> = StdMutex::new(None);

type BrowserLoginResult = Option<Result<LlmGatewayCredential, String>>;

struct BrowserLogin {
    account: String,
    prompt_url: Url,
    result: watch::Receiver<BrowserLoginResult>,
    /// Set until one request claims the sign-in as interactive, so only that
    /// request continues to the next step in the same browser tab.
    interactive: Arc<AtomicBool>,
    opened_at: Instant,
    reopen_after: Duration,
    task: tokio::task::AbortHandle,
}

impl BrowserLogin {
    /// Finished with a result, or stopped without one (aborted or panicked).
    fn finished(&self) -> bool {
        self.result.borrow().is_some() || self.result.has_changed().is_err()
    }
}

/// A browser sign-in is waiting for the user. The request did not open
/// another tab; the user finishes the sign-in in the one already open.
#[derive(Debug)]
pub struct BrowserSignInPending {
    prompt_url: Url,
}

impl std::fmt::Display for BrowserSignInPending {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "LLM gateway sign-in pending: finish signing in at {}",
            self.prompt_url
        )
    }
}

impl std::error::Error for BrowserSignInPending {}

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
pub(crate) struct DeviceAuthorizationResponse {
    pub(crate) device_code: String,
    pub(crate) user_code: String,
    pub(crate) verification_uri: String,
    #[serde(default)]
    pub(crate) verification_uri_complete: Option<String>,
    pub(crate) expires_in: u64,
    #[serde(default)]
    pub(crate) interval: Option<u64>,
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
    let (prompt_url, mut result, interactive) = {
        // Checking the store and joining or starting the browser sign-in under
        // one lock means a request cannot miss a token the sign-in is saving
        // and start a second sign-in: the sign-in saves under this lock too.
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
        let redirect_uri = Url::parse(redirect_uri).context("parse OIDC redirect URI")?;
        join_browser_login(&account, || BrowserLoginRequest {
            issuer: issuer.clone(),
            client_id: client_id.to_owned(),
            redirect_uri,
            scopes: scopes.to_owned(),
            allow_insecure,
            state_dir: state_dir.to_owned(),
            account: account.clone(),
            login,
        })
    };
    let finished = tokio::time::timeout(BROWSER_SIGN_IN_WAIT, result.wait_for(Option::is_some))
        .await
        .map_err(|_| BrowserSignInPending {
            prompt_url: prompt_url.clone(),
        })?
        .context("browser sign-in stopped")?
        .clone()
        .expect("waited for a finished sign-in");
    match finished {
        Ok(credential) => Ok(CredentialAcquisition {
            credential,
            interactive: interactive.swap(false, Ordering::AcqRel),
        }),
        Err(error) => Err(anyhow::anyhow!(error)),
    }
}

struct BrowserLoginRequest {
    issuer: Url,
    client_id: String,
    redirect_uri: Url,
    scopes: Vec<String>,
    allow_insecure: bool,
    state_dir: PathBuf,
    account: String,
    login: LoginOptions,
}

/// Joins the pending browser sign-in for `account`, or starts one. Returns the
/// sign-in page, its result, and the flag that marks the sign-in interactive.
///
/// A pending sign-in is never restarted, so requests made while the user is
/// away do not each open a tab. The page is reopened only after a backoff, in
/// case the user closed it. A pending sign-in for another account (the gateway
/// configuration changed) is abandoned, because both need the callback port.
fn join_browser_login(
    account: &str,
    request: impl FnOnce() -> BrowserLoginRequest,
) -> (Url, watch::Receiver<BrowserLoginResult>, Arc<AtomicBool>) {
    let mut slot = BROWSER_LOGIN
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if let Some(pending) = slot.as_mut().filter(|login| !login.finished()) {
        if pending.account == account {
            if pending.opened_at.elapsed() >= pending.reopen_after {
                tracing::info!(prompt_url = %pending.prompt_url, "reopening pending Agentdesktop sign-in page");
                oidc::open_in_browser(pending.prompt_url.as_str());
                pending.opened_at = Instant::now();
                pending.reopen_after = pending
                    .reopen_after
                    .saturating_mul(2)
                    .min(BROWSER_REOPEN_MAX);
            } else {
                tracing::debug!(prompt_url = %pending.prompt_url, "waiting on pending Agentdesktop sign-in");
            }
            return (
                pending.prompt_url.clone(),
                pending.result.clone(),
                pending.interactive.clone(),
            );
        }
        tracing::info!("abandoning pending sign-in for a different LLM gateway account");
        pending.task.abort();
    }
    let request = request();
    let prompt_url = oidc::authorization_prompt_url(&request.redirect_uri);
    let (sender, result) = watch::channel(None);
    let task = tokio::spawn(async move {
        let outcome = browser_login(&request).await;
        let signed_in = {
            // Save and publish under LOGIN: see `credential_inner`.
            let _login = LOGIN.lock().await;
            let outcome = match outcome {
                Ok(stored) => SecretStore::new(&request.state_dir)
                    .and_then(|store| save(&store, &request.account, &stored))
                    .map(|()| as_credential(&stored)),
                Err(error) => Err(error),
            };
            if let Err(error) = &outcome {
                tracing::warn!(error = %format!("{error:#}"), "LLM gateway browser sign-in failed");
            }
            let signed_in = outcome.is_ok();
            let _ = sender.send(Some(outcome.map_err(|error| format!("{error:#}"))));
            signed_in
        };
        if signed_in {
            continue_to_github(&request).await;
        }
    });
    let interactive = Arc::new(AtomicBool::new(true));
    *slot = Some(BrowserLogin {
        account: account.to_owned(),
        prompt_url: prompt_url.clone(),
        result: result.clone(),
        interactive: interactive.clone(),
        opened_at: Instant::now(),
        reopen_after: BROWSER_REOPEN_INITIAL,
        task: task.abort_handle(),
    });
    (prompt_url, result, interactive)
}

/// Runs one browser sign-in to completion. It has no deadline: the page stays
/// usable until the user finishes, however long they are away.
async fn browser_login(request: &BrowserLoginRequest) -> anyhow::Result<StoredTokens> {
    let login = &request.login;
    let metadata = discover(&request.issuer, request.allow_insecure).await?;
    let state = oidc::random_secret();
    let (verifier, challenge) = oidc::pkce();
    let authorization_url = authorization_url(
        metadata.authorization_endpoint,
        &request.client_id,
        &request.redirect_uri,
        &request.scopes,
        &state,
        &challenge,
    );
    let authorization_code = oidc::wait_for_authorization_code_until_completed(
        authorization_url.as_str(),
        &request.redirect_uri,
        state,
        login.callback_listen,
        oidc::AuthorizationPage::Identity {
            subscription_available: login.subscription_available,
            github_available: login.github_client_id.is_some(),
        },
    )
    .await?
    .context("required identity authorization was skipped")?;
    let tokens = oidc::exchange_authorization_code(
        metadata.token_endpoint.as_str(),
        &request.client_id,
        request.redirect_uri.as_str(),
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
    Ok(stored)
}

/// Continues the finished identity sign-in to GitHub in the same tab, when
/// GitHub is the next step.
async fn continue_to_github(request: &BrowserLoginRequest) {
    let login = &request.login;
    let Some(client_id) = login
        .github_client_id
        .as_deref()
        .filter(|_| !login.subscription_available)
    else {
        return;
    };
    if let Err(error) = crate::github_oauth::credential(
        client_id,
        &request.state_dir,
        Some((request.redirect_uri.clone(), login.callback_listen)),
    )
    .await
    {
        tracing::warn!(error = %format!("{error:#}"), "GitHub sign-in after LLM gateway sign-in failed");
    }
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
    let device = request_device_authorization(endpoint, client_id, scopes).await?;
    Ok((metadata, device))
}

pub(crate) async fn request_device_authorization(
    endpoint: Url,
    client_id: &str,
    scopes: &[String],
) -> anyhow::Result<DeviceAuthorizationResponse> {
    let device = reqwest::Client::new()
        .post(endpoint)
        .timeout(DEVICE_REQUEST_TIMEOUT)
        .form(&device_authorization_form(client_id, scopes))
        .send()
        .await
        .context("request OIDC device authorization")?
        .error_for_status()
        .context("OIDC device authorization endpoint returned an error")?
        .json::<DeviceAuthorizationResponse>()
        .await
        .context("decode OIDC device authorization response")?;
    validate_device(device)
}

/// Rejects a device-authorization response that could not be acted on, and caps its
/// timing. An empty code or a non-URL verification address would publish an unusable `awaitingAuthentication`
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

pub(crate) fn authorization_url(
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

    /// Many requests made while the user is away share one sign-in tab, the
    /// tab still works after every request has given up on it, and finishing
    /// it serves later requests from the store.
    #[tokio::test(flavor = "multi_thread")]
    async fn browser_sign_in_is_shared_and_outlives_waiting_requests() {
        use axum::{Json, Router, routing::get, routing::post};

        tokio::time::timeout(Duration::from_secs(20), async {
            let idp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let idp_address = idp.local_addr().unwrap();
            let issuer = Url::parse(&format!("http://{idp_address}/")).unwrap();
            let discovery = serde_json::json!({
                "authorization_endpoint": format!("http://{idp_address}/authorize"),
                "token_endpoint": format!("http://{idp_address}/token"),
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
                    "/token",
                    post(|| async {
                        Json(serde_json::json!({
                            "access_token": "signed-in-token",
                            "expires_in": 3600,
                            "token_type": "Bearer",
                        }))
                    }),
                );
            tokio::spawn(async move { axum::serve(idp, app).await });
            let callback = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let callback_address = callback.local_addr().unwrap();
            drop(callback);
            let redirect_uri = format!("http://{callback_address}/callback");
            let prompt_url = format!("http://{callback_address}/");
            let state_dir = tempfile::tempdir().unwrap();
            let client_id = "coalesce-test";
            let request = || {
                let issuer = issuer.clone();
                let redirect_uri = redirect_uri.clone();
                let state_dir = state_dir.path().to_owned();
                async move {
                    super::credential(
                        &issuer,
                        client_id,
                        &redirect_uri,
                        &["openid".to_owned()],
                        true,
                        &state_dir,
                        super::LoginOptions {
                            callback_listen: None,
                            subscription_available: false,
                            github_client_id: None,
                            device_authorization: false,
                        },
                    )
                    .await
                }
            };
            let opened = || {
                crate::oidc::OPENED_PAGES
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|page| **page == prompt_url)
                    .count()
            };

            // Nobody is at the machine: every request gives up, one tab opens.
            let waiting = futures_util::future::join_all((0..5).map(|_| request())).await;
            for result in waiting {
                let error = result.err().expect("no one signed in");
                assert!(error.is::<super::BrowserSignInPending>(), "{error:#}");
                assert!(error.to_string().contains(&prompt_url));
            }
            assert_eq!(opened(), 1);

            // The user returns to the same tab and signs in.
            let late = tokio::spawn(request());
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let response = client.get(&prompt_url).send().await.unwrap();
            let location = Url::parse(response.headers()["location"].to_str().unwrap()).unwrap();
            assert_eq!(location.path(), "/authorize");
            let state = location
                .query_pairs()
                .find(|(name, _)| name == "state")
                .unwrap()
                .1
                .into_owned();
            let response = client
                .get(format!("{redirect_uri}?code=identity-code&state={state}"))
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());

            let acquired = late.await.unwrap().unwrap();
            assert_eq!(acquired.credential.credential, "signed-in-token");
            assert!(acquired.interactive);
            let stored = request().await.unwrap();
            assert_eq!(stored.credential.credential, "signed-in-token");
            assert!(!stored.interactive);
            assert_eq!(opened(), 1);

            let store = crate::secret_store::SecretStore::new(state_dir.path()).unwrap();
            store
                .delete(super::SECRET_SERVICE, &super::account(&issuer, client_id))
                .unwrap();
        })
        .await
        .unwrap();
    }

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
        let issuer = mock_idp(Duration::from_millis(1500), requests.clone()).await;
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

        // While the provider is still thinking, the global lock must be free. Brief
        // holds for token-store reads (the macOS Keychain can be slow under load) are
        // fine; the probe gives up well before the provider answers at 1500ms.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(1000), LOGIN.lock())
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
