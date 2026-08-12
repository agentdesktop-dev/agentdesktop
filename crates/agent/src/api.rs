use std::path::PathBuf;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::get,
};
use serde::{Deserialize, Serialize};

use agentdesktop_core::{
    config::{ControllerConnectionConfig, DaemonConfig},
    model::{Discovery, EnrollmentStatus, InferenceGatewayCredential},
};

use crate::{enrollment::EnrollmentState, remote};

#[derive(Clone)]
pub struct AppState {
    pub config: DaemonConfig,
    pub discovery: Discovery,
    pub enrollment: EnrollmentState,
    pub controller: Option<ControllerConnectionConfig>,
    pub state_dir: PathBuf,
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
        .route(
            "/v1/inference-gateway/credential",
            get(inference_gateway_credential),
        )
        .with_state(state)
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
    let controller = state.controller.as_ref().ok_or_else(|| {
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
