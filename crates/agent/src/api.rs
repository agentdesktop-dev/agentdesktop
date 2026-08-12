use std::path::PathBuf;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use agentdesktop_core::{
    config::DaemonConfig,
    model::{
        Discovery, EnrollmentStatus, InferenceGatewayCredential, TelemetryEvent, TelemetryEventKind,
    },
};

use crate::{enrollment::EnrollmentState, remote};

#[derive(Clone)]
pub struct AppState {
    pub config: DaemonConfig,
    pub discovery: Discovery,
    pub enrollment: EnrollmentState,
    pub state_dir: PathBuf,
    pub telemetry: Option<mpsc::Sender<TelemetryEvent>>,
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
        .route("/v1/discovery", get(discover))
        .route("/v1/enrollment", get(enrollment))
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

async fn discover(State(state): State<AppState>) -> Json<Discovery> {
    Json(state.discovery)
}

async fn enrollment(State(state): State<AppState>) -> Json<EnrollmentStatus> {
    Json(state.enrollment.get().await)
}

async fn inference_gateway_credential(
    State(state): State<AppState>,
    Query(query): Query<CredentialQuery>,
) -> Result<Json<InferenceGatewayCredential>, (StatusCode, String)> {
    let controller = state.config.controller.as_ref().ok_or_else(|| {
        (
            StatusCode::FAILED_DEPENDENCY,
            "daemon has no controller configured".to_string(),
        )
    })?;
    // TODO: The client ID is asserted, not authenticated. On Linux we could use
    // SO_PEERCRED to obtain the helper's PID and inspect its /proc parent chain
    // for the expected Codex, Claude Code, or OpenCode executable. Equivalent
    // peer-PID APIs exist for other local transports. This would provide useful
    // process evidence, but not a hard boundary against the same user or root.
    remote::inference_gateway_credential(controller, &state.state_dir, &query.client_id)
        .await
        .map(Json)
        .map_err(|error| (StatusCode::BAD_GATEWAY, format!("{error:#}")))
}
