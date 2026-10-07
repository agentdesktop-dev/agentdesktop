use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use agentdesktop_core::model::LlmGatewayCredential;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::Mutex,
    time::{Instant, sleep, timeout_at},
};

use crate::secret_store::SecretStore;

const SECRET_SERVICE: &str = "dev.agentdesktop.github-oauth";
const EXPIRY_SKEW_SECONDS: u64 = 60;
/// Body of the device-flow page; `{{user_code}}` is replaced with the escaped code.
const DEVICE_PAGE: &str = include_str!("github_oauth.html");
static LOGIN: Mutex<()> = Mutex::const_new(());

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredTokens {
    access_token: String,
    expires_at: u64,
    refresh_token: Option<String>,
    refresh_expires_at: u64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    token_type: String,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
    refresh_token_expires_in: Option<u64>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TokenResult {
    Error { error: String },
    Token(TokenResponse),
}

#[derive(Deserialize)]
struct DeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: u64,
}

pub(crate) async fn credential(
    client_id: &str,
    state_dir: &Path,
    continuation: Option<(url::Url, Option<std::net::SocketAddr>)>,
) -> anyhow::Result<LlmGatewayCredential> {
    let client_id = client_id.to_owned();
    let state_dir = state_dir.to_owned();
    // Complete refresh and persist rotated tokens even if the HTTP caller disconnects.
    tokio::spawn(async move {
        let _login = LOGIN.lock().await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent("agentdesktop")
            .build()?;
        acquire(
            &client,
            "https://github.com",
            &client_id,
            &SecretStore::new(&state_dir)?,
            continuation,
        )
        .await
    })
    .await
    .context("join GitHub OAuth task")?
}

async fn acquire(
    client: &reqwest::Client,
    base: &str,
    client_id: &str,
    store: &SecretStore,
    continuation: Option<(url::Url, Option<std::net::SocketAddr>)>,
) -> anyhow::Result<LlmGatewayCredential> {
    let mut page_started = false;
    let result = acquire_inner(
        client,
        base,
        client_id,
        store,
        &continuation,
        &mut page_started,
    )
    .await;
    // In a continued flow the browser is already waiting for this page; an
    // error before it was served would otherwise leave that tab polling.
    if result.is_err() && !page_started && continuation.is_some() {
        match device_page("", continuation.as_ref()).await {
            Ok((_, status, server)) => finish_page(status, server, false),
            Err(error) => {
                tracing::warn!(%error, "could not show the GitHub authorization failure page")
            }
        }
    }
    result
}

async fn acquire_inner(
    client: &reqwest::Client,
    base: &str,
    client_id: &str,
    store: &SecretStore,
    continuation: &Option<(url::Url, Option<std::net::SocketAddr>)>,
    page_started: &mut bool,
) -> anyhow::Result<LlmGatewayCredential> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let stored = store
        .get_optional(SECRET_SERVICE, client_id)?
        .map(|value| serde_json::from_str::<StoredTokens>(&value))
        .transpose()
        .context("decode stored GitHub OAuth tokens")?;
    if let Some(stored) = stored {
        if stored.expires_at > now.saturating_add(EXPIRY_SKEW_SECONDS) {
            if continuation.is_some() {
                let (_, status, server) = device_page("", continuation.as_ref()).await?;
                *page_started = true;
                finish_page(status, server, true);
            }
            return Ok(LlmGatewayCredential {
                credential: stored.access_token,
                expires_at_unix_seconds: stored.expires_at,
            });
        }
        if let Some(refresh) = stored.refresh_token.as_deref()
            && stored.refresh_expires_at > now.saturating_add(EXPIRY_SKEW_SECONDS)
        {
            match token_request(
                client,
                base,
                &[
                    ("client_id", client_id),
                    ("grant_type", "refresh_token"),
                    ("refresh_token", refresh),
                ],
            )
            .await?
            {
                TokenResult::Token(response) => {
                    let credential = save(store, client_id, response)?;
                    if continuation.is_some() {
                        let (_, status, server) = device_page("", continuation.as_ref()).await?;
                        *page_started = true;
                        finish_page(status, server, true);
                    }
                    return Ok(credential);
                }
                TokenResult::Error { error }
                    if matches!(
                        error.as_str(),
                        "bad_refresh_token" | "invalid_grant" | "expired_token"
                    ) =>
                {
                    tracing::info!(
                        "GitHub refresh token expired or revoked; authorization required"
                    );
                }
                TokenResult::Error { .. } => bail!("GitHub rejected the token refresh"),
            }
        }
    }

    let code: DeviceCode = client
        .post(format!("{base}/login/device/code"))
        .header("Accept", "application/json")
        .form(&[("client_id", client_id)])
        .send()
        .await
        .context("request GitHub device code")?
        .error_for_status()
        .context("GitHub device authorization failed")?
        .json()
        .await
        .context("decode GitHub device code")?;
    let verification = url::Url::parse(&code.verification_uri)?;
    if verification.scheme() != "https" || verification.host_str() != Some("github.com") {
        bail!("GitHub returned an unexpected verification URL");
    }
    let (page_url, status, server) = device_page(&code.user_code, continuation.as_ref()).await?;
    *page_started = true;
    println!("Open this URL to connect GitHub:\n{page_url}");
    if continuation.is_none()
        && let Err(error) = open::that_detached(&page_url)
    {
        tracing::warn!(%error, "could not open the Agentdesktop sign-in page; use the URL above");
    }
    let deadline = Instant::now() + Duration::from_secs(code.expires_in);
    let mut interval = Duration::from_secs(code.interval.max(1));
    let result = timeout_at(deadline, async {
        loop {
            sleep(interval).await;
            match token_request(client, base, &[
                ("client_id", client_id), ("device_code", code.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ]).await? {
                TokenResult::Token(response) => return save(store, client_id, response),
                TokenResult::Error { error } => match error.as_str() {
                    "authorization_pending" => {},
                    "slow_down" => interval += Duration::from_secs(5),
                    "access_denied" => bail!("GitHub authorization declined"),
                    "expired_token" => bail!("GitHub device code expired; retry to sign in"),
                    _ => bail!("GitHub rejected device authorization; check the App client ID and Device Flow setting"),
                },
            }
        }
    }).await.context("GitHub device authorization timed out; retry to sign in")
        .and_then(|result| result);
    finish_page(status, server, result.is_ok());
    result
}

fn finish_page(
    status: tokio::sync::watch::Sender<&'static str>,
    server: tokio::task::JoinHandle<()>,
    connected: bool,
) {
    status.send_replace(if connected { "connected" } else { "failed" });
    tokio::spawn(async move {
        sleep(Duration::from_secs(60)).await;
        server.abort();
        let _ = server.await;
    });
}

async fn device_page(
    user_code: &str,
    continuation: Option<&(url::Url, Option<std::net::SocketAddr>)>,
) -> anyhow::Result<(
    String,
    tokio::sync::watch::Sender<&'static str>,
    tokio::task::JoinHandle<()>,
)> {
    use axum::{Router, extract::State, response::Html, routing::get};
    let (listener, path, page_url) = if let Some((redirect, listen)) = continuation {
        let listener = crate::oidc::bind_callback(redirect, *listen).await?;
        let mut prompt = redirect.clone();
        prompt.set_path("/");
        prompt.set_query(None);
        prompt.set_fragment(None);
        (listener, String::new(), prompt.to_string())
    } else {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("bind GitHub sign-in page")?;
        let path = format!("/{}", crate::oidc::random_secret());
        let url = format!("http://{}{path}/", listener.local_addr()?);
        (listener, path, url)
    };
    let escaped_code = user_code
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;");
    let html = crate::oidc::page(
        "Connect GitHub",
        &DEVICE_PAGE.replace("{{user_code}}", &escaped_code),
    );
    let (status, receiver) = tokio::sync::watch::channel("pending");
    let app = Router::new()
        .route(
            "/flow-ready",
            get(|| async { axum::http::StatusCode::NO_CONTENT }),
        )
        .route(
            &format!("{path}/"),
            get(move || {
                let html = html.clone();
                async move { Html(html) }
            }),
        )
        .route(
            &format!("{path}/status"),
            get(
                |State(state): State<tokio::sync::watch::Receiver<&'static str>>| async move {
                    *state.borrow()
                },
            ),
        )
        .layer(axum::middleware::map_response(
            |mut response: axum::response::Response| async move {
                response
                    .headers_mut()
                    .insert("cache-control", "no-store".parse().unwrap());
                response
                    .headers_mut()
                    .insert("referrer-policy", "no-referrer".parse().unwrap());
                response
                    .headers_mut()
                    .insert("x-frame-options", "DENY".parse().unwrap());
                response
            },
        ))
        .with_state(receiver);
    let server = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::warn!(%error, "GitHub sign-in page stopped");
        }
    });
    Ok((page_url, status, server))
}

async fn token_request(
    client: &reqwest::Client,
    base: &str,
    fields: &[(&str, &str)],
) -> anyhow::Result<TokenResult> {
    client
        .post(format!("{base}/login/oauth/access_token"))
        .header("Accept", "application/json")
        .form(fields)
        .send()
        .await
        .context("request GitHub OAuth token")?
        .error_for_status()
        .context("GitHub OAuth token request failed")?
        .json()
        .await
        .context("decode GitHub OAuth token response")
}

fn save(
    store: &SecretStore,
    client_id: &str,
    response: TokenResponse,
) -> anyhow::Result<LlmGatewayCredential> {
    if !response.token_type.eq_ignore_ascii_case("bearer") || response.access_token.is_empty() {
        bail!("GitHub returned an invalid access token or token type");
    }
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let stored = StoredTokens {
        access_token: response.access_token,
        expires_at: response
            .expires_in
            .map_or(u64::MAX, |ttl| now.saturating_add(ttl)),
        refresh_token: response.refresh_token,
        refresh_expires_at: response
            .refresh_token_expires_in
            .map_or(0, |ttl| now.saturating_add(ttl)),
    };
    store.set(SECRET_SERVICE, client_id, &serde_json::to_string(&stored)?)?;
    Ok(LlmGatewayCredential {
        credential: stored.access_token,
        expires_at_unix_seconds: stored.expires_at,
    })
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use axum::{
        Router,
        extract::{Form, State},
        response::IntoResponse,
        routing::post,
    };
    use std::{
        collections::HashMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[tokio::test]
    async fn device_page_shows_code_and_tracks_completion_without_exposing_tokens() {
        let (url, status, server) = device_page("ABCD-1234", None).await.unwrap();
        let client = reqwest::Client::new();
        let response = client.get(&url).send().await.unwrap();
        assert_eq!(response.headers()["cache-control"], "no-store");
        let html = response.text().await.unwrap();
        assert!(html.contains("ABCD-1234"));
        assert!(!html.contains("{{user_code}}"));
        assert!(html.contains("https://github.com/login/device"));
        assert!(html.contains("Copy code"));
        assert_eq!(
            client
                .get(format!("{url}status"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "pending"
        );
        status.send_replace("connected");
        assert_eq!(
            client
                .get(format!("{url}status"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "connected"
        );
        status.send_replace("failed");
        assert_eq!(
            client
                .get(format!("{url}status"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "failed"
        );
        server.abort();
        let _ = server.await;

        // Reuse the IdP callback origin so the browser can continue in the same tab.
        let mut redirect = url::Url::parse(&url).unwrap();
        redirect.set_path("/callback");
        let (continued_url, _, continued_server) =
            device_page("EFGH-5678", Some(&(redirect.clone(), None)))
                .await
                .unwrap();
        assert_eq!(
            continued_url,
            format!("{}/", redirect.origin().ascii_serialization())
        );
        assert_eq!(
            client
                .get(format!("{continued_url}flow-ready"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NO_CONTENT
        );
        assert!(
            client
                .get(&continued_url)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
                .contains("EFGH-5678")
        );
        continued_server.abort();
    }

    #[tokio::test]
    async fn refresh_preserves_credentials_on_failure_and_persists_rotation() {
        let requests = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/login/oauth/access_token",
                post(
                    |State(requests): State<Arc<AtomicUsize>>,
                     Form(fields): Form<HashMap<String, String>>| async move {
                        assert_eq!(fields["client_id"], "test-app");
                        assert_eq!(fields["grant_type"], "refresh_token");
                        assert_eq!(fields["refresh_token"], "old-refresh");
                        assert!(!fields.contains_key("client_secret"));
                        if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                            return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                        }
                        axum::Json(
                            serde_json::json!({"access_token": "ghu_new", "token_type": "bearer",
                    "expires_in": 28800, "refresh_token": "rotated-refresh",
                    "refresh_token_expires_in": 15897600}),
                        )
                        .into_response()
                    },
                ),
            )
            .with_state(requests.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(dir.path()).unwrap();
        let old = serde_json::to_string(&StoredTokens {
            access_token: "expired".into(),
            expires_at: 0,
            refresh_token: Some("old-refresh".into()),
            refresh_expires_at: u64::MAX,
        })
        .unwrap();
        store.set(SECRET_SERVICE, "test-app", &old).unwrap();
        let client = reqwest::Client::new();
        assert!(
            acquire(&client, &base, "test-app", &store, None)
                .await
                .is_err()
        );
        assert_eq!(store.get(SECRET_SERVICE, "test-app").unwrap(), old);
        assert_eq!(
            acquire(&client, &base, "test-app", &store, None)
                .await
                .unwrap()
                .credential,
            "ghu_new"
        );
        let stored: StoredTokens =
            serde_json::from_str(&store.get(SECRET_SERVICE, "test-app").unwrap()).unwrap();
        assert_eq!(stored.refresh_token.as_deref(), Some("rotated-refresh"));
        assert_eq!(
            acquire(&client, &base, "test-app", &store, None)
                .await
                .unwrap()
                .credential,
            "ghu_new"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        server.abort();
    }

    #[tokio::test]
    async fn continued_flow_shows_failure_when_github_setup_fails() {
        let app = Router::new().route(
            "/login/device/code",
            post(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let github = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // The port the OIDC callback page used; the browser tab polls it.
        let callback = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", callback.local_addr().unwrap());
        drop(callback);
        let redirect = url::Url::parse(&format!("{origin}/callback")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(dir.path()).unwrap();
        let client = reqwest::Client::new();
        assert!(
            acquire(&client, &base, "test-app", &store, Some((redirect, None)))
                .await
                .is_err()
        );
        assert_eq!(
            client
                .get(format!("{origin}/flow-ready"))
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NO_CONTENT
        );
        assert_eq!(
            client
                .get(format!("{origin}/status"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "failed"
        );
        github.abort();
    }
}
