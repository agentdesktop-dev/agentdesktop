use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Result;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use tokio::net::{TcpListener, TcpStream};

use crate::config::DeploymentMode;
use crate::identity::oauth::ManagedIdentity;
use crate::platform::{PlatformCapabilities, capabilities};

use super::forwarder::{ForwarderMetrics, ForwarderMetricsSnapshot};
use super::hbone::HboneClient;

#[derive(Clone)]
struct StatusState {
    gateway: Option<HboneClient>,
    gateway_endpoint: SocketAddr,
    identity: Option<ManagedIdentity>,
    metrics: ForwarderMetrics,
    mode: DeploymentMode,
    max_in_flight: usize,
    connect_timeout_ms: u64,
    shutdown_timeout_ms: u64,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    mode: &'static str,
    gateway: &'static str,
}

#[derive(Serialize)]
struct StatusResponse {
    version: &'static str,
    mode: &'static str,
    gateway: &'static str,
    identity: &'static str,
    in_flight: usize,
    max_in_flight: usize,
    connect_timeout_ms: u64,
    shutdown_timeout_ms: u64,
    platform: PlatformCapabilities,
    metrics: ForwarderMetricsSnapshot,
}

pub async fn serve(
    listener: TcpListener,
    gateway: Option<HboneClient>,
    gateway_endpoint: SocketAddr,
    mode: DeploymentMode,
    identity: Option<ManagedIdentity>,
    metrics: ForwarderMetrics,
    max_in_flight: usize,
    connect_timeout_ms: u64,
    shutdown_timeout_ms: u64,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let state = StatusState {
        gateway,
        gateway_endpoint,
        identity,
        metrics,
        mode,
        max_in_flight,
        connect_timeout_ms,
        shutdown_timeout_ms,
    };
    let router = Router::new()
        .route("/_agentdesktop/healthz", get(health))
        .route("/_agentdesktop/status", get(status))
        .with_state(state);
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

async fn health(State(state): State<StatusState>) -> (StatusCode, Json<HealthResponse>) {
    let reachable = gateway_reachable(&state).await;
    (
        if reachable {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(HealthResponse {
            status: if reachable { "ok" } else { "degraded" },
            mode: state.mode.as_str(),
            gateway: if reachable {
                "reachable"
            } else {
                "unreachable"
            },
        }),
    )
}

async fn status(State(state): State<StatusState>) -> Json<StatusResponse> {
    let reachable = gateway_reachable(&state).await;
    let identity = match &state.identity {
        Some(identity) => identity.status().await.unwrap_or("unavailable"),
        None => "not-required",
    };
    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        mode: state.mode.as_str(),
        gateway: if reachable {
            "reachable"
        } else {
            "unreachable"
        },
        identity,
        in_flight: state.metrics.in_flight(),
        max_in_flight: state.max_in_flight,
        connect_timeout_ms: state.connect_timeout_ms,
        shutdown_timeout_ms: state.shutdown_timeout_ms,
        platform: capabilities(),
        metrics: state.metrics.snapshot(),
    })
}

async fn gateway_reachable(state: &StatusState) -> bool {
    match &state.gateway {
        Some(gateway) => gateway.is_reachable().await,
        None => tokio::time::timeout(
            Duration::from_secs(2),
            TcpStream::connect(state.gateway_endpoint),
        )
        .await
        .is_ok_and(|result| result.is_ok()),
    }
}

use std::future::Future;
