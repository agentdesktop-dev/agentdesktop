use std::{net::SocketAddr, path::Path};

use agentdesktop_core::model::InferenceGatewayCredential;
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

#[derive(Deserialize)]
struct ProviderMetadata {
    authorization_endpoint: Url,
    token_endpoint: Url,
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
    callback_listen: Option<SocketAddr>,
) -> anyhow::Result<InferenceGatewayCredential> {
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
            callback_listen,
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
    callback_listen: Option<SocketAddr>,
) -> anyhow::Result<InferenceGatewayCredential> {
    let _login = LOGIN.lock().await;
    let store = SecretStore::new(state_dir)?;
    let account = account(issuer, client_id);
    let mut stored = load(&store, &account)?;

    if let Some(tokens) = stored.as_ref()
        && tokens.expires_at_unix_seconds > now().saturating_add(EXPIRY_SKEW_SECONDS)
    {
        return Ok(as_credential(tokens));
    }

    if let Some(tokens) = stored.as_mut()
        && let Some(refresh_token) = tokens.refresh_token.as_deref()
    {
        match oidc::refresh_access_token(&tokens.token_endpoint, client_id, refresh_token).await {
            Ok(refreshed) => {
                tokens.access_token = refreshed.access_token;
                tokens.refresh_token = refreshed.refresh_token.or(tokens.refresh_token.take());
                tokens.expires_at_unix_seconds = now().saturating_add(refreshed.expires_in);
                save(&store, &account, tokens)?;
                return Ok(as_credential(tokens));
            }
            Err(error) => tracing::warn!(%error, "OIDC token refresh failed; signing in again"),
        }
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
    let authorization_code = oidc::wait_for_authorization_code(
        authorization_url.as_str(),
        &redirect_uri,
        state,
        callback_listen,
    )
    .await?;
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
    Ok(as_credential(&stored))
}

async fn discover(issuer: &Url, allow_insecure: bool) -> anyhow::Result<ProviderMetadata> {
    let endpoint = format!(
        "{}/.well-known/openid-configuration",
        issuer.as_str().trim_end_matches('/')
    );
    let metadata = reqwest::Client::new()
        .get(endpoint)
        .send()
        .await
        .context("discover OIDC provider")?
        .error_for_status()
        .context("OIDC provider rejected discovery request")?
        .json::<ProviderMetadata>()
        .await
        .context("decode OIDC provider metadata")?;
    for (name, endpoint) in [
        ("authorization", &metadata.authorization_endpoint),
        ("token", &metadata.token_endpoint),
    ] {
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

fn as_credential(tokens: &StoredTokens) -> InferenceGatewayCredential {
    InferenceGatewayCredential {
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

    use super::authorization_url;

    #[test]
    fn authorization_request_uses_pkce_and_configured_scopes() {
        let url = authorization_url(
            Url::parse("https://idp.example/authorize").unwrap(),
            "agentdesktop",
            &Url::parse("http://127.0.0.1:5555/callback").unwrap(),
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
}
