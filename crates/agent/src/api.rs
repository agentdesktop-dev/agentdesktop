use std::{net::SocketAddr, path::PathBuf};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

use agentdesktop_core::{
    config::{DaemonConfig, InferenceGatewayAuthentication, valid_client_id},
    model::{
        Discovery, EnrollmentStatus, InferenceGatewayCredential, TelemetryEvent, TelemetryEventKind,
    },
};

use crate::{enrollment::EnrollmentState, gateway_oidc, remote};

#[derive(Clone)]
pub struct AppState {
    pub config: DaemonConfig,
    pub discovery: Discovery,
    pub enrollment: EnrollmentState,
    pub state_dir: PathBuf,
    pub oidc_callback_listen: Option<SocketAddr>,
    pub telemetry: Option<mpsc::Sender<TelemetryEvent>>,
    pub logout: Option<mpsc::Sender<remote::LogoutRequest>>,
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

#[derive(Deserialize)]
struct CredentialQuery {
    client_id: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/config", get(config))
        .route("/v1/effective-config", get(effective_config))
        .route("/v1/remote-config", get(remote_config))
        .route("/v1/discovery", get(discover))
        .route("/v1/enrollment", get(enrollment))
        .route("/v1/logout", post(logout))
        .route("/v1/telemetry", post(telemetry))
        .route(
            "/v1/inference-gateway/credential",
            get(inference_gateway_credential),
        )
        .with_state(state)
}

async fn telemetry(
    State(state): State<AppState>,
    Json(event): Json<TelemetryEventKind>,
) -> Result<StatusCode, (StatusCode, String)> {
    let sender = state.telemetry.as_ref().ok_or_else(|| {
        (
            StatusCode::FAILED_DEPENDENCY,
            "daemon has no controller configured".to_owned(),
        )
    })?;
    validate_telemetry(&event)?;
    let event = TelemetryEvent {
        timestamp_unix_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX),
        event,
    };
    sender.try_send(event).map_err(|error| {
        let status = match error {
            mpsc::error::TrySendError::Full(_) => StatusCode::SERVICE_UNAVAILABLE,
            mpsc::error::TrySendError::Closed(_) => StatusCode::FAILED_DEPENDENCY,
        };
        (status, "telemetry pipeline is unavailable".to_owned())
    })?;
    Ok(StatusCode::NO_CONTENT)
}

fn validate_telemetry(event: &TelemetryEventKind) -> Result<(), (StatusCode, String)> {
    match event {
        TelemetryEventKind::SessionNew {
            client_id,
            session_id,
        } => {
            if client_id.is_empty() || client_id.len() > 64 {
                return Err((StatusCode::BAD_REQUEST, "invalid client ID".to_owned()));
            }
            if session_id.is_empty() || session_id.len() > 256 {
                return Err((StatusCode::BAD_REQUEST, "invalid session ID".to_owned()));
            }
        }
        TelemetryEventKind::ToolUse {
            client_id,
            tool_name,
            tool_use_id,
            tool_input,
        } => {
            if client_id.is_empty() || client_id.len() > 64 {
                return Err((StatusCode::BAD_REQUEST, "invalid client ID".to_owned()));
            }
            if tool_name.is_empty() || tool_name.len() > 128 {
                return Err((StatusCode::BAD_REQUEST, "invalid tool name".to_owned()));
            }
            if tool_use_id.as_ref().is_some_and(|id| id.len() > 256) {
                return Err((StatusCode::BAD_REQUEST, "invalid tool use ID".to_owned()));
            }
            if tool_input.as_ref().is_some_and(|input| {
                serde_json::to_vec(input).is_ok_and(|encoded| encoded.len() > 256 * 1024)
            }) {
                return Err((
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "tool input is too large".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn config(State(state): State<AppState>) -> Json<DaemonConfig> {
    Json(state.config)
}

async fn effective_config(
    State(state): State<AppState>,
) -> Result<Json<DaemonConfig>, (StatusCode, String)> {
    let path = state.state_dir.join("remote-config.yaml");
    match std::fs::read_to_string(&path) {
        Ok(contents) => agentdesktop_core::config::parse_daemon(&contents)
            .map(Json)
            .map_err(|error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("parse applied remote configuration: {error:#}"),
                )
            }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Json(state.config)),
        Err(error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read applied remote configuration: {error}"),
        )),
    }
}

async fn remote_config(
    State(state): State<AppState>,
) -> Result<Json<Option<String>>, (StatusCode, String)> {
    let path = state.state_dir.join("remote-config.yaml");
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Json(Some(contents))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Json(None)),
        Err(error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read applied remote configuration: {error}"),
        )),
    }
}

async fn discover(State(state): State<AppState>) -> Json<Discovery> {
    Json(state.discovery)
}

async fn enrollment(State(state): State<AppState>) -> Json<EnrollmentStatus> {
    Json(state.enrollment.get().await)
}

async fn logout(State(state): State<AppState>) -> Result<StatusCode, (StatusCode, String)> {
    let sender = state.logout.as_ref().ok_or_else(|| {
        (
            StatusCode::FAILED_DEPENDENCY,
            "daemon has no controller session to log out".to_owned(),
        )
    })?;
    let (completion, completed) = oneshot::channel();
    sender
        .send(remote::LogoutRequest { completion })
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "controller session is unavailable".to_owned(),
            )
        })?;
    completed
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "controller session stopped before logout completed".to_owned(),
            )
        })?
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn inference_gateway_credential(
    State(state): State<AppState>,
    Query(query): Query<CredentialQuery>,
) -> Result<Json<InferenceGatewayCredential>, (StatusCode, String)> {
    if !valid_client_id(&query.client_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid client ID".to_owned()));
    }
    let gateway = state.config.inference_gateway.as_ref().ok_or_else(|| {
        (
            StatusCode::FAILED_DEPENDENCY,
            "daemon has no inference gateway configured".to_owned(),
        )
    })?;
    match gateway.authentication.as_ref() {
        Some(InferenceGatewayAuthentication::ControllerJwt { .. }) => {
            let controller = state.config.controller.as_ref().ok_or_else(|| {
                (
                    StatusCode::FAILED_DEPENDENCY,
                    "controller JWT authentication requires a controller".to_owned(),
                )
            })?;
            // Local transport permissions authenticate the user, not the calling
            // process. The client ID selects an allowed policy within that boundary.
            remote::inference_gateway_credential(controller, &state.state_dir, &query.client_id)
                .await
        }
        Some(InferenceGatewayAuthentication::Oidc {
            issuer,
            client_id,
            redirect_uri,
            scopes,
            allow_insecure,
        }) => {
            gateway_oidc::credential(
                issuer,
                client_id,
                redirect_uri,
                scopes,
                *allow_insecure,
                &state.state_dir,
                state.oidc_callback_listen,
            )
            .await
        }
        None => Err(anyhow::anyhow!(
            "inference gateway has no authentication configured"
        )),
    }
    .map(Json)
    .map_err(|error| (StatusCode::BAD_GATEWAY, format!("{error:#}")))
}
