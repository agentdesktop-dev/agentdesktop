use std::{net::SocketAddr, sync::Arc, time::Duration};

use agentplane_core::config::ControllerConfig;
use agentplane_proto::fleet::{BeginEnrollmentRequest, CompleteEnrollmentRequest};
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
    controller: &ControllerConfig,
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
        "Open this URL to enroll Agentplane:\n{}",
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
        Html("Agentplane enrollment is complete. You can close this window.").into_response()
    } else {
        (
            StatusCode::BAD_REQUEST,
            Html("Agentplane enrollment failed. Return to the daemon logs for details."),
        )
            .into_response()
    }
}

fn random_secret() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
