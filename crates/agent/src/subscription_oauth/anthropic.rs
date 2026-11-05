use std::{net::SocketAddr, path::Path};

use agentdesktop_core::{http::ClientExt, model::LlmGatewayCredential};
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use url::Url;

use super::{LoginState, OAuthProvider};
use crate::subscription::SubscriptionCredential;

// These are the production Claude subscription OAuth settings used by Claude Code
// 2.1.237. Anthropic does not currently publish them as a stable third-party API.
const AUTHORIZATION_ENDPOINT: &str = "https://claude.com/cai/oauth/authorize";
const TOKEN_ENDPOINT: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const REDIRECT_URI: &str = "http://localhost:51327/callback";
const SCOPE: &str = "user:inference";
const REQUESTED_LIFETIME_SECONDS: u64 = 365 * 24 * 60 * 60;

static LOGIN: LoginState = LoginState::new();
struct Anthropic;

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
) -> anyhow::Result<Option<LlmGatewayCredential>> {
    Ok(
        super::credential(&Anthropic, &LOGIN, state_dir, callback_listen, open_browser)
            .await?
            .map(|credential| credential.token),
    )
}

#[async_trait::async_trait]
impl OAuthProvider for Anthropic {
    type Token = StoredToken;
    const NAME: &'static str = "Anthropic";
    const SERVICE: &'static str = "dev.agentdesktop.anthropic-subscription";
    const ACCOUNT: &'static str = "claude";
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
            account_id: None,
        }
    }

    async fn exchange(
        &self,
        code: &str,
        verifier: &str,
        state: &str,
    ) -> anyhow::Result<StoredToken> {
        let response = exchange_authorization_code(code, verifier, state).await?;
        if !response.token_type.eq_ignore_ascii_case("bearer") {
            bail!("Anthropic OAuth endpoint returned unsupported token type");
        }
        Ok(StoredToken {
            access_token: response.access_token,
            expires_at_unix_seconds: super::now().saturating_add(response.expires_in),
        })
    }
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
        .post_json::<TokenResponse>(
            TOKEN_ENDPOINT,
            &TokenRequest {
                grant_type: "authorization_code",
                code: authorization_code,
                redirect_uri: REDIRECT_URI,
                client_id: CLIENT_ID,
                code_verifier,
                state,
                expires_in: REQUESTED_LIFETIME_SECONDS,
            },
        )
        .await
        .context("exchange Anthropic subscription authorization code")
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
