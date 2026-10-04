use std::{collections::BTreeMap, net::SocketAddr, path::Path, time::Duration};

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
    let _login = LOGIN.lock().await;
    let store = SecretStore::new(state_dir)?;
    let account = account(issuer, client_id);
    if stored_credential(&store, &account, client_id)
        .await?
        .is_some()
    {
        return Ok(signed_in());
    }
    if let Some(pending) = pending_device_login(&account) {
        return Ok(awaiting(pending));
    }

    let metadata = discover(issuer, allow_insecure).await?;
    let endpoint = metadata
        .device_authorization_endpoint
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
    use url::Url;

    use super::{
        DeviceAuthorizationResponse, PendingDeviceLogin, SignInRequired, authorization_url,
        device_authorization_form, pending_from, poll_interval,
    };

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
