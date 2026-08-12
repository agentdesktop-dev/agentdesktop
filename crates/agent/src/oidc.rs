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
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, oneshot};
use url::Url;

use crate::{enrollment::EnrollmentState, identity::Identity, remote};

const CALLBACK_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const SUCCESS_PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Enrollment complete · AgentDesktop</title>
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
    <svg viewBox="0 0 256 256" fill="none" aria-label="AgentDesktop">
      <path d="M72 61v134M184 61v134M72 94h112M72 162h112" stroke="#8023C3" stroke-width="22" stroke-linecap="round"/>
      <circle cx="72" cy="61" r="20" fill="#8023C3"/>
      <circle cx="184" cy="195" r="20" fill="#8023C3"/>
      <circle cx="128" cy="128" r="18" fill="#5B168E"/>
    </svg>
    <h1>Enrollment complete</h1>
    <p>You can close this window and return to AgentDesktop.</p>
  </main>
</body>
</html>"##;

const FAILURE_PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>Enrollment failed · AgentDesktop</title>
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
    <svg viewBox="0 0 256 256" fill="none" aria-label="AgentDesktop">
      <path d="M72 61v134M184 61v134M72 94h112M72 162h112" stroke="#8023C3" stroke-width="22" stroke-linecap="round"/>
      <circle cx="72" cy="61" r="20" fill="#8023C3"/>
      <circle cx="184" cy="195" r="20" fill="#8023C3"/>
      <circle cx="128" cy="128" r="18" fill="#5B168E"/>
    </svg>
    <h1>Enrollment failed</h1>
    <p>Return to AgentDesktop and try again. Details are available in the daemon logs.</p>
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

pub async fn enroll(
    controller: &ControllerConnectionConfig,
    enrollment: &EnrollmentState,
    callback_listen: Option<SocketAddr>,
) -> anyhow::Result<Identity> {
    let verifier = random_secret();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let mut client = remote::client(controller).await?;
    let begin = client
        .begin_enrollment(BeginEnrollmentRequest {
            hostname: remote::hostname(),
            code_challenge: challenge,
        })
        .await
        .context("begin OIDC enrollment")?
        .into_inner();

    let redirect_uri = Url::parse(&begin.redirect_uri).context("parse OIDC redirect URI")?;
    let listener = bind_callback(&redirect_uri, callback_listen).await?;
    let (result_sender, result_receiver) = oneshot::channel();
    let state = CallbackState {
        expected_state: begin.state,
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

    enrollment
        .awaiting_authentication(begin.authorization_url.clone())
        .await;
    tracing::info!(
        authorization_url = %begin.authorization_url,
        "open this URL to enroll the device"
    );
    println!(
        "Open this URL to enroll AgentDesktop:\n{}",
        begin.authorization_url
    );

    let authorization_code = tokio::time::timeout(CALLBACK_TIMEOUT, result_receiver)
        .await
        .context("timed out waiting for OIDC callback")?
        .context("OIDC callback server stopped")??;
    let _ = shutdown_sender.send(());
    server.await.context("join OIDC callback server")??;
    enrollment.set("enrolling").await;

    let response = client
        .complete_enrollment(CompleteEnrollmentRequest {
            enrollment_id: begin.enrollment_id,
            authorization_code,
            code_verifier: verifier,
        })
        .await
        .context("complete OIDC enrollment")?
        .into_inner();

    Ok(Identity {
        device_id: response.device_id,
        credential: response.credential,
    })
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

fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
