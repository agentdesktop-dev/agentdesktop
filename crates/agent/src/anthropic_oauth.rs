use std::{
    net::SocketAddr,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

use agentdesktop_core::model::InferenceGatewayCredential;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use url::Url;

use crate::{oidc, secret_store::SecretStore};

// These are the production Claude subscription OAuth settings used by Claude Code
// 2.1.237. Anthropic does not currently publish them as a stable third-party API.
const AUTHORIZATION_ENDPOINT: &str = "https://claude.com/cai/oauth/authorize";
const TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const REDIRECT_URI: &str = "http://localhost:51327/callback";
const SCOPE: &str = "user:inference";
const REQUESTED_LIFETIME_SECONDS: u64 = 365 * 24 * 60 * 60;
const EXPIRY_SKEW_SECONDS: u64 = 60;
const SECRET_SERVICE: &str = "dev.agentdesktop.anthropic-subscription";
const SECRET_ACCOUNT: &str = "claude";

static LOGIN: Mutex<()> = Mutex::const_new(());
static DECLINED_FOR_PROCESS: AtomicBool = AtomicBool::new(false);

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredToken {
    access_token: String,
    expires_at_unix_seconds: u64,
}

#[derive(Serialize)]
struct TokenRequest<'a> {
    grant_type: &'static str,
    code: &'a str,
    redirect_uri: &'static str,
    client_id: &'static str,
    code_verifier: &'a str,
    state: &'a str,
    expires_in: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    token_type: String,
}

pub async fn credential(
    state_dir: &Path,
    callback_listen: Option<SocketAddr>,
    open_browser: bool,
) -> anyhow::Result<Option<InferenceGatewayCredential>> {
    let state_dir = state_dir.to_owned();
    tokio::spawn(async move { credential_inner(&state_dir, callback_listen, open_browser).await })
        .await
        .context("join Anthropic subscription OAuth task")?
}

async fn credential_inner(
    state_dir: &Path,
    callback_listen: Option<SocketAddr>,
    open_browser: bool,
) -> anyhow::Result<Option<InferenceGatewayCredential>> {
    let _login = LOGIN.lock().await;
    let store = SecretStore::new(state_dir)?;
    let redirect_uri = Url::parse(REDIRECT_URI).expect("static Anthropic redirect URI is valid");
    if let Some(stored) = load(&store)?
        && stored.expires_at_unix_seconds > now().saturating_add(EXPIRY_SKEW_SECONDS)
    {
        if !open_browser {
            oidc::continue_subscription_page(&redirect_uri, callback_listen, true).await?;
        }
        return Ok(Some(as_credential(&stored)));
    }
    if DECLINED_FOR_PROCESS.load(Ordering::Relaxed) {
        if !open_browser {
            oidc::continue_subscription_page(&redirect_uri, callback_listen, false).await?;
        }
        return Ok(None);
    }

    let state = oidc::random_secret();
    let (verifier, challenge) = oidc::pkce();
    let authorization_url = authorization_url(&redirect_uri, &state, &challenge);
    let authorization_code = oidc::wait_for_authorization_code_with_page(
        authorization_url.as_str(),
        &redirect_uri,
        state.clone(),
        callback_listen,
        oidc::AuthorizationPage::Subscription,
        open_browser,
    )
    .await?;
    let Some(authorization_code) = authorization_code else {
        DECLINED_FOR_PROCESS.store(true, Ordering::Relaxed);
        return Ok(None);
    };
    let response = exchange_authorization_code(&authorization_code, &verifier, &state).await?;
    if !response.token_type.eq_ignore_ascii_case("bearer") {
        bail!("Anthropic OAuth endpoint returned unsupported token type");
    }
    let stored = StoredToken {
        access_token: response.access_token,
        expires_at_unix_seconds: now().saturating_add(response.expires_in),
    };
    save(&store, &stored)?;
    Ok(Some(as_credential(&stored)))
}

fn authorization_url(redirect_uri: &Url, state: &str, challenge: &str) -> Url {
    let mut endpoint = Url::parse(AUTHORIZATION_ENDPOINT)
        .expect("static Anthropic authorization endpoint is valid");
    endpoint.query_pairs_mut().extend_pairs([
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", redirect_uri.as_str()),
        ("scope", SCOPE),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("state", state),
    ]);
    endpoint
}

async fn exchange_authorization_code(
    authorization_code: &str,
    code_verifier: &str,
    state: &str,
) -> anyhow::Result<TokenResponse> {
    reqwest::Client::new()
        .post(TOKEN_ENDPOINT)
        .json(&TokenRequest {
            grant_type: "authorization_code",
            code: authorization_code,
            redirect_uri: REDIRECT_URI,
            client_id: CLIENT_ID,
            code_verifier,
            state,
            expires_in: REQUESTED_LIFETIME_SECONDS,
        })
        .send()
        .await
        .context("exchange Anthropic subscription authorization code")?
        .error_for_status()
        .context("Anthropic OAuth endpoint rejected authorization code")?
        .json()
        .await
        .context("decode Anthropic OAuth token response")
}

fn load(store: &SecretStore) -> anyhow::Result<Option<StoredToken>> {
    store
        .get_optional(SECRET_SERVICE, SECRET_ACCOUNT)?
        .map(|value| serde_json::from_str(&value).context("decode stored Anthropic OAuth token"))
        .transpose()
}

fn save(store: &SecretStore, token: &StoredToken) -> anyhow::Result<()> {
    store.set(
        SECRET_SERVICE,
        SECRET_ACCOUNT,
        &serde_json::to_string(token).context("encode Anthropic OAuth token")?,
    )
}

fn as_credential(token: &StoredToken) -> InferenceGatewayCredential {
    InferenceGatewayCredential {
        credential: token.access_token.clone(),
        expires_at_unix_seconds: token.expires_at_unix_seconds,
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
    use std::collections::HashMap;

    use url::Url;

    use super::{
        CLIENT_ID, REDIRECT_URI, REQUESTED_LIFETIME_SECONDS, SCOPE, TokenRequest, authorization_url,
    };

    #[test]
    fn authorization_request_matches_claude_subscription_pkce_flow() {
        let redirect = Url::parse(REDIRECT_URI).unwrap();
        let url = authorization_url(&redirect, "state", "challenge");
        let parameters: HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(parameters["code"], "true");
        assert_eq!(parameters["client_id"], CLIENT_ID);
        assert_eq!(parameters["response_type"], "code");
        assert_eq!(parameters["redirect_uri"], REDIRECT_URI);
        assert_eq!(parameters["scope"], SCOPE);
        assert_eq!(parameters["code_challenge"], "challenge");
        assert_eq!(parameters["code_challenge_method"], "S256");
        assert_eq!(parameters["state"], "state");
    }

    #[test]
    fn token_exchange_requests_a_one_year_token() {
        let request = serde_json::to_value(TokenRequest {
            grant_type: "authorization_code",
            code: "code",
            redirect_uri: REDIRECT_URI,
            client_id: CLIENT_ID,
            code_verifier: "verifier",
            state: "state",
            expires_in: REQUESTED_LIFETIME_SECONDS,
        })
        .unwrap();
        assert_eq!(request["expires_in"], 31_536_000);
        assert_eq!(request["redirect_uri"], REDIRECT_URI);
        assert_eq!(request["client_id"], CLIENT_ID);
    }
}
