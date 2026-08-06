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

#[derive(Clone)]
struct StatusState {
    gateway: SocketAddr,
    identity: Option<ManagedIdentity>,
    mode: DeploymentMode,
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
    platform: PlatformCapabilities,
}

pub async fn serve(
    listener: TcpListener,
    gateway: SocketAddr,
    mode: DeploymentMode,
    identity: Option<ManagedIdentity>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let state = StatusState {
        gateway,
        identity,
        mode,
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
    let reachable = gateway_reachable(state.gateway).await;
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
    let reachable = gateway_reachable(state.gateway).await;
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
        platform: capabilities(),
    })
}

async fn gateway_reachable(gateway: SocketAddr) -> bool {
    tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(gateway))
        .await
        .is_ok_and(|result| result.is_ok())
}

use std::future::Future;
