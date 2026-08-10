use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;

use agentplane_core::{
    config::Config,
    model::{Discovery, EnrollmentStatus},
};

use crate::enrollment::EnrollmentState;

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub discovery: Discovery,
    pub enrollment: EnrollmentState,
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/config", get(config))
        .route("/v1/discovery", get(discover))
        .route("/v1/enrollment", get(enrollment))
        .with_state(state)
}

async fn health() -> Json<Health> {
    Json(Health { status: "ok" })
}

async fn config(State(state): State<AppState>) -> Json<Config> {
    Json(state.config)
}

async fn discover(State(state): State<AppState>) -> Json<Discovery> {
    Json(state.discovery)
}

async fn enrollment(State(state): State<AppState>) -> Json<EnrollmentStatus> {
    Json(state.enrollment.get().await)
}
