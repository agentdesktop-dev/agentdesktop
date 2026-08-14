use std::{net::SocketAddr, sync::Arc, time::Duration};

use agentdesktop_core::config::ControllerConnectionConfig;
use agentdesktop_proto::fleet::{BeginEnrollmentRequest, CompleteEnrollmentRequest};
use anyhow::{Context, bail};
use axum::{
    Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::Rng;
use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, oneshot};
use url::Url;

use crate::{
    enrollment::EnrollmentState,
    identity::{Identity, OAuthCredentials},
    remote,
};

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const SUCCESS_PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Sign-in complete · Agentdesktop</title>
  <style>
    :root { color-scheme: light; font-family: ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
    * { box-sizing: border-box; }
    body { margin: 0; min-height: 100vh; display: grid; place-items: center; background: #fff; color: #18181b; }
    main { width: min(100% - 40px, 380px); padding: 32px; }
    svg { display: block; width: 42px; height: 42px; margin-bottom: 24px; }
    h1 { margin: 0 0 8px; font-size: 20px; line-height: 1.35; font-weight: 600; letter-spacing: -.01em; }
    p { margin: 0; color: #71717a; font-size: 14px; line-height: 1.55; }
  </style>
</head>
<body>
  <main>
    <svg viewBox="0 0 256 256" fill="none" aria-label="Agentdesktop">
      <path d="M72 61v134M184 61v134M72 94h112M72 162h112" stroke="#8023C3" stroke-width="22" stroke-linecap="round"/>
      <circle cx="72" cy="61" r="20" fill="#8023C3"/>
      <circle cx="184" cy="195" r="20" fill="#8023C3"/>
      <circle cx="128" cy="128" r="18" fill="#5B168E"/>
    </svg>
    <h1>Sign-in complete</h1>
    <p>You can close this window and return to Agentdesktop.</p>
  </main>
</body>
</html>"##;

const FAILURE_PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Sign-in failed · Agentdesktop</title>
  <style>
    :root { color-scheme: light; font-family: ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; }
    * { box-sizing: border-box; }
    body { margin: 0; min-height: 100vh; display: grid; place-items: center; background: #fff; color: #18181b; }
    main { width: min(100% - 40px, 380px); padding: 32px; }
    svg { display: block; width: 42px; height: 42px; margin-bottom: 24px; }
    h1 { margin: 0 0 8px; font-size: 20px; line-height: 1.35; font-weight: 600; letter-spacing: -.01em; }
    p { margin: 0; color: #71717a; font-size: 14px; line-height: 1.55; }
  </style>
</head>
<body>
  <main>
    <svg viewBox="0 0 256 256" fill="none" aria-label="Agentdesktop">
      <path d="M72 61v134M184 61v134M72 94h112M72 162h112" stroke="#8023C3" stroke-width="22" stroke-linecap="round"/>
      <circle cx="72" cy="61" r="20" fill="#8023C3"/>
      <circle cx="184" cy="195" r="20" fill="#8023C3"/>
      <circle cx="128" cy="128" r="18" fill="#5B168E"/>
    </svg>
    <h1>Sign-in failed</h1>
    <p>Return to Agentdesktop and try again. Details are available in the daemon logs.</p>
  </main>
</body>
</html>"##;

#[derive(Clone)]
struct CallbackState {
    expected_state: String,
    result: Arc<Mutex<Option<oneshot::Sender<anyhow::Result<String>>>>>,
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct TokenResponse {
    pub(crate) access_token: String,
    pub(crate) id_token: Option<String>,
    pub(crate) refresh_token: Option<String>,
    pub(crate) expires_in: u64,
    pub(crate) token_type: String,
}

pub async fn enroll(
    controller: &ControllerConnectionConfig,
    enrollment: &EnrollmentState,
    callback_listen: Option<SocketAddr>,
) -> anyhow::Result<Identity> {
    let (verifier, challenge) = pkce();
    let mut client = remote::client(controller, None).await?;
    let device_key = KeyPair::generate().context("generate device TLS private key")?;
    let mut certificate_params = CertificateParams::new(Vec::<String>::new())?;
    certificate_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    certificate_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let csr = certificate_params
        .serialize_request(&device_key)
        .context("create device certificate signing request")?;
    let begin = client
        .begin_enrollment(BeginEnrollmentRequest {
            hostname: remote::hostname(),
            code_challenge: challenge,
        })
        .await
        .context("begin OIDC enrollment")?
        .into_inner();

    let redirect_uri = Url::parse(&begin.redirect_uri).context("parse OIDC redirect URI")?;
    enrollment
        .awaiting_authentication(begin.authorization_url.clone())
        .await;
    let authorization_code = wait_for_authorization_code(
        &begin.authorization_url,
        &redirect_uri,
        begin.state,
        callback_listen,
    )
    .await?;
    enrollment.set("enrolling").await;

    let tokens = exchange_authorization_code(
        &begin.token_endpoint,
        &begin.client_id,
        &begin.redirect_uri,
        &authorization_code,
        &verifier,
    )
    .await?;
    let id_token = tokens
        .id_token
        .context("OIDC token response did not contain an ID token")?;
    let refresh_token = tokens
        .refresh_token
        .context("OIDC token response did not contain a refresh token")?;

    let mut request = tonic::Request::new(CompleteEnrollmentRequest {
        enrollment_id: begin.enrollment_id,
        authorization_code: String::new(),
        code_verifier: String::new(),
        certificate_signing_request_der: csr.der().as_ref().to_vec(),
        id_token,
    });
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", tokens.access_token)
            .parse()
            .context("encode OIDC access token")?,
    );
    let response = client
        .complete_enrollment(request)
        .await
        .context("complete OIDC enrollment")?
        .into_inner();

    let client_certificate_pem = String::from_utf8(response.client_certificate_pem)
        .context("controller returned a non-UTF-8 device certificate")?;
    if client_certificate_pem.is_empty() {
        anyhow::bail!("controller returned an empty device certificate");
    }
    Ok(Identity {
        device_id: response.device_id,
        client_certificate_pem,
        client_private_key_pem: device_key.serialize_pem(),
        client_certificate_expires_at_unix_seconds: response
            .client_certificate_expires_at_unix_seconds,
        oauth: OAuthCredentials {
            access_token: tokens.access_token,
            refresh_token,
            expires_at_unix_seconds: unix_time_seconds().saturating_add(tokens.expires_in),
        },
        oauth_token_endpoint: begin.token_endpoint,
        oauth_client_id: begin.client_id,
    })
}

pub(crate) async fn exchange_authorization_code(
    token_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    authorization_code: &str,
    code_verifier: &str,
) -> anyhow::Result<TokenResponse> {
    let response = reqwest::Client::new()
        .post(token_endpoint)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", authorization_code),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", code_verifier),
        ])
        .send()
        .await
        .context("exchange OIDC authorization code")?
        .error_for_status()
        .context("OIDC token endpoint rejected authorization code")?
        .json::<TokenResponse>()
        .await
        .context("decode OIDC token response")?;
    if !response.token_type.eq_ignore_ascii_case("Bearer") {
        bail!("OIDC token endpoint returned unsupported token type");
    }
    Ok(response)
}

pub async fn refresh(identity: &mut Identity) -> anyhow::Result<()> {
    let endpoint = &identity.oauth_token_endpoint;
    let client_id = &identity.oauth_client_id;
    let current = &identity.oauth;
    let response = refresh_access_token(endpoint, client_id, &current.refresh_token).await?;
    apply_refreshed_tokens(identity, response)
}

pub(crate) async fn refresh_access_token(
    endpoint: &str,
    client_id: &str,
    refresh_token: &str,
) -> anyhow::Result<TokenResponse> {
    let response = reqwest::Client::new()
        .post(endpoint)
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ])
        .send()
        .await
        .context("refresh OIDC access token")?
        .error_for_status()
        .context("OIDC token endpoint rejected refresh token")?
        .json::<TokenResponse>()
        .await
        .context("decode refreshed OIDC token response")?;
    if !response.token_type.eq_ignore_ascii_case("Bearer") {
        bail!("OIDC token endpoint returned unsupported token type");
    }
    Ok(response)
}

fn apply_refreshed_tokens(identity: &mut Identity, response: TokenResponse) -> anyhow::Result<()> {
    if !response.token_type.eq_ignore_ascii_case("Bearer") {
        bail!("OIDC token endpoint returned unsupported token type");
    }
    let current_refresh_token = identity.oauth.refresh_token.clone();
    identity.oauth = OAuthCredentials {
        access_token: response.access_token,
        refresh_token: response.refresh_token.unwrap_or(current_refresh_token),
        expires_at_unix_seconds: unix_time_seconds().saturating_add(response.expires_in),
    };
    Ok(())
}

fn unix_time_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) async fn wait_for_authorization_code(
    authorization_url: &str,
    redirect_uri: &Url,
    expected_state: String,
    callback_listen: Option<SocketAddr>,
) -> anyhow::Result<String> {
    let listener = bind_callback(redirect_uri, callback_listen).await?;
    let (result_sender, result_receiver) = oneshot::channel();
    let state = CallbackState {
        expected_state,
        result: Arc::new(Mutex::new(Some(result_sender))),
    };
    let callback_path = redirect_uri.path().to_owned();
    let app = Router::new()
        .route(&callback_path, get(callback))
        .with_state(state);
    let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = shutdown_receiver.await;
            })
            .await
            .context("serve OIDC callback")
    });

    tracing::info!(%authorization_url, "opening browser for OIDC sign-in");
    println!("Open this URL to sign in to Agentdesktop:\n{authorization_url}");
    if let Err(error) = open::that(authorization_url) {
        tracing::warn!(%error, "could not open the browser automatically");
    }

    let authorization_code = tokio::time::timeout(CALLBACK_TIMEOUT, result_receiver)
        .await
        .context("timed out waiting for OIDC callback")?
        .context("OIDC callback server stopped")??;
    let _ = shutdown_sender.send(());
    server.await.context("join OIDC callback server")??;
    Ok(authorization_code)
}

async fn bind_callback(
    redirect_uri: &Url,
    callback_listen: Option<SocketAddr>,
) -> anyhow::Result<tokio::net::TcpListener> {
    if redirect_uri.scheme() != "http" {
        bail!("OIDC native callback must use HTTP on loopback");
    }
    let host = redirect_uri
        .host_str()
        .context("OIDC callback has no host")?;
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1") {
        bail!("OIDC callback must use a loopback host");
    }
    let port = redirect_uri
        .port_or_known_default()
        .context("OIDC callback has no port")?;
    let advertised = format!("{host}:{port}");
    match callback_listen {
        Some(listen) => {
            if !listen.ip().is_loopback() {
                tracing::warn!(
                    %listen,
                    %advertised,
                    "OIDC callback server is listening beyond loopback; restrict access at the container or host boundary"
                );
            } else {
                tracing::info!(%listen, %advertised, "binding OIDC callback server");
            }
            tokio::net::TcpListener::bind(listen)
                .await
                .with_context(|| format!("bind OIDC callback at {listen}"))
        }
        None => {
            tracing::info!(listen = %advertised, %advertised, "binding OIDC callback server");
            tokio::net::TcpListener::bind((host, port))
                .await
                .with_context(|| format!("bind OIDC callback at {advertised}"))
        }
    }
}

async fn callback(
    State(state): State<CallbackState>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let result = if let Some(error) = query.error {
        Err(anyhow::anyhow!(
            "identity provider returned {error}: {}",
            query.error_description.unwrap_or_default()
        ))
    } else if query.state.as_deref() != Some(&state.expected_state) {
        Err(anyhow::anyhow!("OIDC state mismatch"))
    } else {
        query
            .code
            .context("OIDC callback has no authorization code")
    };
    let succeeded = result.is_ok();
    if let Some(sender) = state.result.lock().await.take() {
        let _ = sender.send(result);
    }

    if succeeded {
        Html(SUCCESS_PAGE).into_response()
    } else {
        (StatusCode::BAD_REQUEST, Html(FAILURE_PAGE)).into_response()
    }
}

pub(crate) fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub(crate) fn pkce() -> (String, String) {
    let verifier = random_secret();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

#[cfg(test)]
mod tests {
    use super::{TokenResponse, apply_refreshed_tokens};
    use crate::identity::{Identity, OAuthCredentials};

    #[test]
    fn refresh_replaces_rotated_oauth_credentials() {
        let mut identity = Identity {
            device_id: "device".to_owned(),
            client_certificate_pem: "certificate".to_owned(),
            client_private_key_pem: "key".to_owned(),
            client_certificate_expires_at_unix_seconds: u64::MAX,
            oauth: OAuthCredentials {
                access_token: "old-access".to_owned(),
                refresh_token: "old-refresh".to_owned(),
                expires_at_unix_seconds: 0,
            },
            oauth_token_endpoint: "https://idp.example/token".to_owned(),
            oauth_client_id: "client".to_owned(),
        };

        apply_refreshed_tokens(
            &mut identity,
            TokenResponse {
                access_token: "new-access".to_owned(),
                id_token: None,
                refresh_token: Some("new-refresh".to_owned()),
                expires_in: 600,
                token_type: "Bearer".to_owned(),
            },
        )
        .unwrap();
        let oauth = identity.oauth;
        assert_eq!(oauth.access_token, "new-access");
        assert_eq!(oauth.refresh_token, "new-refresh");
        assert!(oauth.expires_at_unix_seconds > 0);
    }
}
