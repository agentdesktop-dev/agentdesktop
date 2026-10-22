use std::{net::SocketAddr, path::Path, time::Duration};

use agentdesktop_core::model::LlmGatewayCredential;
use anyhow::{Context, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::Url;

use super::{EXPIRY_SKEW_SECONDS, LoginState, OAuthProvider};
use crate::subscription::SubscriptionCredential;

const AUTHORIZATION_ENDPOINT: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_ENDPOINT: &str = "https://auth.openai.com/oauth/token";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";

static LOGIN: LoginState = LoginState::new();
struct OpenAi;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredToken {
    access_token: String,
    refresh_token: String,
    account_id: String,
    expires_at_unix_seconds: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
    token_type: Option<String>,
}

#[derive(Debug)]
struct ReauthorizationRequired;

impl std::fmt::Display for ReauthorizationRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ChatGPT subscription requires sign-in again")
    }
}

impl std::error::Error for ReauthorizationRequired {}

pub async fn credential(
    state_dir: &Path,
    callback_listen: Option<SocketAddr>,
    open_browser: bool,
) -> anyhow::Result<Option<(LlmGatewayCredential, String)>> {
    Ok(
        super::credential(&OpenAi, &LOGIN, state_dir, callback_listen, open_browser)
            .await?
            .map(|credential| {
                (
                    credential.token,
                    credential
                        .account_id
                        .expect("OpenAI adapter supplies account ID"),
                )
            }),
    )
}

#[async_trait::async_trait]
impl OAuthProvider for OpenAi {
    type Token = StoredToken;
    const NAME: &'static str = "ChatGPT";
    const SERVICE: &'static str = "dev.agentdesktop.openai-subscription";
    const ACCOUNT: &'static str = "codex";
    const REDIRECT_URI: &'static str = REDIRECT_URI;

    fn authorization_url(&self, redirect: &Url, state: &str, challenge: &str) -> Url {
        authorization_url(redirect, state, challenge)
    }

    fn expires_at(&self, token: &StoredToken) -> u64 {
        token.expires_at_unix_seconds
    }

    fn credential_from_token(&self, token: StoredToken) -> SubscriptionCredential {
        SubscriptionCredential {
            token: LlmGatewayCredential {
                credential: token.access_token,
                expires_at_unix_seconds: token.expires_at_unix_seconds,
            },
            account_id: Some(token.account_id),
        }
    }

    async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        _state: &str,
    ) -> anyhow::Result<StoredToken> {
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", REDIRECT_URI),
        ];
        let response = request_token(TOKEN_ENDPOINT, &form, false).await?;
        stored_token(response, None, super::now())
    }

    async fn refresh(&self, token: &StoredToken) -> anyhow::Result<Option<StoredToken>> {
        match request_token(TOKEN_ENDPOINT, &refresh_form(&token.refresh_token), true).await {
            Ok(response) => Ok(Some(stored_token(response, Some(token), super::now())?)),
            Err(error) if error.is::<ReauthorizationRequired>() => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn authorization_url(redirect_uri: &Url, state: &str, challenge: &str) -> Url {
    let mut endpoint = Url::parse(AUTHORIZATION_ENDPOINT).expect("valid ChatGPT authorization URL");
    endpoint.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri.as_str()),
        ("scope", "openid profile email offline_access"),
        ("code_challenge", challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("state", state),
        ("originator", "codex_cli_rs"),
    ]);
    endpoint
}

fn refresh_form(refresh_token: &str) -> [(&str, &str); 3] {
    [
        ("grant_type", "refresh_token"),
        ("client_id", CLIENT_ID),
        ("refresh_token", refresh_token),
    ]
}

async fn request_token(
    endpoint: &str,
    form: &[(&str, &str)],
    refreshing: bool,
) -> anyhow::Result<TokenResponse> {
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()?
        .post(endpoint)
        .form(form)
        .send()
        .await
        .context("request ChatGPT subscription OAuth token")?;
    let status = response.status();
    if !status.is_success() {
        let invalid_grant = response.json::<Value>().await.ok().is_some_and(|body| {
            matches!(
                body["error"].as_str(),
                Some("invalid_grant" | "invalid_refresh_token")
            )
        });
        if refreshing && (status == reqwest::StatusCode::UNAUTHORIZED || invalid_grant) {
            return Err(ReauthorizationRequired.into());
        }
        bail!("ChatGPT OAuth token endpoint rejected the request ({status})");
    }
    response
        .json()
        .await
        .context("decode ChatGPT OAuth token response")
}

fn claims(token: &str) -> anyhow::Result<Value> {
    let payload = token
        .split('.')
        .nth(1)
        .context("ChatGPT token has no claims")?;
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .context("decode ChatGPT token claims")?;
    serde_json::from_slice(&payload).context("parse ChatGPT token claims")
}

fn stored_token(
    response: TokenResponse,
    previous: Option<&StoredToken>,
    now: u64,
) -> anyhow::Result<StoredToken> {
    if response
        .token_type
        .as_deref()
        .is_some_and(|kind| !kind.eq_ignore_ascii_case("bearer"))
    {
        bail!("ChatGPT OAuth returned an unsupported token type");
    }
    if response.access_token.is_empty()
        || response
            .access_token
            .bytes()
            .any(|byte| byte <= b' ' || byte == b':')
    {
        bail!("ChatGPT OAuth returned an invalid access token");
    }
    let token_claims = response.id_token.as_deref().map(claims).transpose()?;
    let account_id = token_claims
        .as_ref()
        .and_then(|value| value["https://api.openai.com/auth"]["chatgpt_account_id"].as_str())
        .or_else(|| previous.map(|token| token.account_id.as_str()))
        .context("ChatGPT OAuth token has no account ID")?;
    if account_id.is_empty()
        || !account_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        bail!("ChatGPT OAuth returned an invalid account ID");
    }
    if previous.is_some_and(|token| token.account_id != account_id) {
        bail!("ChatGPT token refresh changed the account; sign in again");
    }
    let expires_at_unix_seconds = response
        .expires_in
        .map(|lifetime| now.saturating_add(lifetime))
        .or_else(|| claims(&response.access_token).ok()?.get("exp")?.as_u64())
        .context("ChatGPT OAuth token has no expiry")?;
    if expires_at_unix_seconds <= now.saturating_add(EXPIRY_SKEW_SECONDS) {
        bail!("ChatGPT OAuth returned an expired or short-lived access token");
    }
    let refresh_token = response
        .refresh_token
        .or_else(|| previous.map(|token| token.refresh_token.clone()))
        .filter(|token| !token.is_empty())
        .context("ChatGPT OAuth returned no refresh token")?;
    Ok(StoredToken {
        access_token: response.access_token,
        refresh_token,
        account_id: account_id.to_owned(),
        expires_at_unix_seconds,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(account_id: &str) -> TokenResponse {
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::json!({
                "https://api.openai.com/auth": {"chatgpt_account_id": account_id}
            })
            .to_string(),
        );
        TokenResponse {
            access_token: "access-token".to_owned(),
            refresh_token: Some("refresh-token".to_owned()),
            id_token: Some(format!("e30.{payload}.signature")),
            expires_in: Some(3600),
            token_type: Some("Bearer".to_owned()),
        }
    }

    #[test]
    fn authorization_uses_registered_callback_and_pkce() {
        let url = authorization_url(&Url::parse(REDIRECT_URI).unwrap(), "state", "challenge");
        let parameters: std::collections::HashMap<_, _> = url.query_pairs().collect();
        assert_eq!(parameters["client_id"], CLIENT_ID);
        assert_eq!(
            parameters["redirect_uri"],
            "http://localhost:1455/auth/callback"
        );
        assert_eq!(parameters["code_challenge_method"], "S256");
        assert_eq!(parameters["state"], "state");
    }

    #[test]
    fn token_refresh_retains_account_and_rotates_refresh_token() {
        let previous = stored_token(response("account-1"), None, 100).unwrap();
        let mut refreshed = response("account-1");
        refreshed.id_token = None;
        refreshed.refresh_token = Some("rotated-refresh".to_owned());
        let stored = stored_token(refreshed, Some(&previous), 200).unwrap();
        assert_eq!(stored.account_id, "account-1");
        assert_eq!(stored.refresh_token, "rotated-refresh");
        assert_eq!(stored.expires_at_unix_seconds, 3800);
        assert!(stored_token(response("account-2"), Some(&previous), 200).is_err());
    }

    #[test]
    fn invalid_account_and_short_lived_tokens_are_rejected() {
        assert!(stored_token(response("account:injection"), None, 100).is_err());
        let mut short = response("account-1");
        short.expires_in = Some(1);
        assert!(stored_token(short, None, 100).is_err());
    }

    async fn token_server(
        status: reqwest::StatusCode,
        body: Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/token", listener.local_addr().unwrap());
        let app = axum::Router::new().route(
            "/token",
            axum::routing::post(
                move |axum::Form(form): axum::Form<std::collections::HashMap<String, String>>| {
                    let body = body.clone();
                    async move {
                        assert_eq!(form["grant_type"], "refresh_token");
                        assert_eq!(form["client_id"], CLIENT_ID);
                        assert_eq!(form["refresh_token"], "original-refresh");
                        (status, axum::Json(body))
                    }
                },
            ),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (endpoint, task)
    }

    #[tokio::test]
    async fn refresh_uses_the_registered_client_and_preserves_omitted_rotation() {
        let (endpoint, server) = token_server(
            reqwest::StatusCode::OK,
            serde_json::json!({
                "access_token": "refreshed-access", "expires_in": 3600, "token_type": "Bearer"
            }),
        )
        .await;
        let response = request_token(&endpoint, &refresh_form("original-refresh"), true)
            .await
            .unwrap();
        let mut previous = stored_token(self::response("account-1"), None, 100).unwrap();
        previous.refresh_token = "original-refresh".to_owned();
        let stored = stored_token(response, Some(&previous), 200).unwrap();
        assert_eq!(stored.refresh_token, "original-refresh");
        assert_eq!(stored.access_token, "refreshed-access");
        assert_eq!(stored.account_id, "account-1");
        server.abort();
    }

    #[tokio::test]
    async fn rejected_refresh_reauthenticates_but_transient_errors_do_not_leak_tokens() {
        for (status, code, reauthorize) in [
            (reqwest::StatusCode::BAD_REQUEST, "invalid_grant", true),
            (
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                "temporary_failure",
                false,
            ),
        ] {
            let (endpoint, server) = token_server(
                status,
                serde_json::json!({
                    "error": code, "error_description": "secret-refresh-token-must-not-leak"
                }),
            )
            .await;
            let error = request_token(&endpoint, &refresh_form("original-refresh"), true)
                .await
                .err()
                .unwrap();
            assert_eq!(error.is::<ReauthorizationRequired>(), reauthorize);
            assert!(!format!("{error:#}").contains("secret-refresh-token"));
            server.abort();
        }
    }
}
