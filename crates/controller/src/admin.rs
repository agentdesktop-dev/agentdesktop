use std::{collections::HashMap, net::SocketAddr, time::SystemTime};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rust_embed::RustEmbed;
use serde::Serialize;
use tracing::info;

use agentdesktop_core::{
    config::DaemonConfig,
    llm_usage::{InteractionsQuery, RangeQuery, UsageClient, UsageScope},
    model::{LlmFleetUsageSummary, LlmUsageInteractions, LlmUsageSummary},
};

use crate::{
    daemon_config::DaemonConfigStore,
    database::{Database, DeviceDetail, DeviceSummary},
    gateway_jwt::{self, GatewayJwks},
};

#[derive(Clone)]
pub struct AdminState {
    database: Database,
    daemon_config: DaemonConfigStore,
    settings: ControllerSettings,
    usage: Option<UsageClient>,
}

#[derive(Clone, Serialize)]
pub struct ControllerSettings {
    pub fleet_listen: String,
    pub admin_listen: String,
    pub oidc_enabled: bool,
    pub tls_enabled: bool,
    pub gateway_jwt_enabled: bool,
    pub llm_usage_enabled: bool,
}

impl AdminState {
    pub fn new(
        database: Database,
        daemon_config: DaemonConfigStore,
        settings: ControllerSettings,
        usage: Option<UsageClient>,
    ) -> Self {
        Self {
            database,
            daemon_config,
            settings,
            usage,
        }
    }
}

#[derive(RustEmbed)]
#[folder = "../../frontend/controller/dist/"]
struct AdminAssets;

#[derive(Serialize)]
struct Overview {
    total_devices: usize,
    online_devices: usize,
    offline_devices: usize,
    config_failures: usize,
    active_revision: Option<u64>,
    recent_devices: Vec<DeviceSummary>,
}

pub async fn serve(
    address: SocketAddr,
    state: AdminState,
    gateway_jwks: Option<GatewayJwks>,
) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/api/v1/overview", get(overview))
        .route("/api/v1/devices", get(devices))
        .route("/api/v1/daemon-config", get(daemon_config))
        .route(
            "/api/v1/devices/{device_id}",
            get(device).delete(delete_device),
        )
        .route("/api/v1/devices/{device_id}/usage", get(device_usage))
        .route(
            "/api/v1/devices/{device_id}/usage/interactions",
            get(device_usage_interactions),
        )
        .route("/api/v1/usage", get(fleet_usage))
        .route("/api/v1/settings", get(settings))
        .fallback(get(asset))
        .with_state(state)
        .merge(gateway_jwt::routes(gateway_jwks));
    let listener = tokio::net::TcpListener::bind(address).await?;
    info!(listen = %address, "controller admin UI listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn overview(State(state): State<AdminState>) -> Result<Json<Overview>, AdminError> {
    let devices = state.database.list_devices().await?;
    let now = unix_time_seconds();
    let online_devices = devices
        .iter()
        .filter(|device| {
            device
                .last_seen_at
                .is_some_and(|last_seen| now - last_seen <= 90)
        })
        .count();
    let config_failures = devices
        .iter()
        .filter(|device| device.config_state == Some(2))
        .count();
    Ok(Json(Overview {
        total_devices: devices.len(),
        online_devices,
        offline_devices: devices.len() - online_devices,
        config_failures,
        active_revision: state.daemon_config.current().map(|config| config.revision),
        recent_devices: devices.into_iter().take(5).collect(),
    }))
}

async fn devices(State(state): State<AdminState>) -> Result<Json<Vec<DeviceSummary>>, AdminError> {
    Ok(Json(state.database.list_devices().await?))
}

async fn device(
    State(state): State<AdminState>,
    Path(device_id): Path<String>,
) -> Result<Json<DeviceDetail>, AdminError> {
    state
        .database
        .get_device(&device_id)
        .await?
        .map(Json)
        .ok_or(AdminError::NotFound)
}

async fn delete_device(
    State(state): State<AdminState>,
    Path(device_id): Path<String>,
) -> Result<StatusCode, AdminError> {
    if !state.database.delete_device(&device_id).await? {
        return Err(AdminError::NotFound);
    }
    info!(%device_id, "deleted device from controller");
    Ok(StatusCode::NO_CONTENT)
}

async fn settings(State(state): State<AdminState>) -> Json<ControllerSettings> {
    Json(state.settings)
}

/// Tenant-wide usage grouped by device. Operator-only: the admin listener is loopback.
async fn fleet_usage(
    State(state): State<AdminState>,
    Query(query): Query<RangeQuery>,
) -> Result<Json<Option<LlmFleetUsageSummary>>, AdminError> {
    let Some(usage) = state.usage.as_ref() else {
        return Ok(Json(None));
    };
    let mut summary = usage
        .fleet_summary(query.range)
        .await
        .map_err(AdminError::Usage)?;
    let hostnames: HashMap<String, String> = state
        .database
        .list_devices()
        .await?
        .into_iter()
        .map(|device| (device.id, device.hostname))
        .collect();
    for row in &mut summary.devices {
        row.hostname = row
            .device_id
            .as_deref()
            .and_then(|device_id| hostnames.get(device_id).cloned());
    }
    Ok(Json(Some(summary)))
}

async fn device_usage(
    State(state): State<AdminState>,
    Path(device_id): Path<String>,
    Query(query): Query<RangeQuery>,
) -> Result<Json<Option<LlmUsageSummary>>, AdminError> {
    let Some(usage) = state.usage.as_ref() else {
        return Ok(Json(None));
    };
    if state.database.get_device(&device_id).await?.is_none() {
        return Err(AdminError::NotFound);
    }
    let summary = usage
        .summary(query.range, &UsageScope::device(device_id))
        .await
        .map_err(AdminError::Usage)?;
    Ok(Json(Some(summary)))
}

async fn device_usage_interactions(
    State(state): State<AdminState>,
    Path(device_id): Path<String>,
    Query(query): Query<InteractionsQuery>,
) -> Result<Json<Option<LlmUsageInteractions>>, AdminError> {
    query.validate().map_err(AdminError::BadRequest)?;
    let Some(usage) = state.usage.as_ref() else {
        return Ok(Json(None));
    };
    if state.database.get_device(&device_id).await?.is_none() {
        return Err(AdminError::NotFound);
    }
    let interactions = usage
        .interactions(&query, &UsageScope::device(device_id))
        .await
        .map_err(AdminError::Usage)?;
    Ok(Json(Some(interactions)))
}

#[derive(Serialize)]
struct ActiveDaemonConfig {
    config: Option<DaemonConfig>,
}

async fn daemon_config(
    State(state): State<AdminState>,
) -> Result<Json<ActiveDaemonConfig>, AdminError> {
    let config = state
        .daemon_config
        .current()
        .map(|active| {
            let yaml = std::str::from_utf8(&active.yaml).context("daemon config is not UTF-8")?;
            agentdesktop_core::config::parse_daemon(yaml)
        })
        .transpose()?;
    Ok(Json(ActiveDaemonConfig { config }))
}

async fn asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let (path, file) = AdminAssets::get(path)
        .map(|file| (path, file))
        .or_else(|| AdminAssets::get("index.html").map(|file| ("index.html", file)))
        .expect("embedded controller UI must contain index.html");
    let content_type = mime_guess::from_path(path).first_or_octet_stream();
    (
        [
            (header::CONTENT_TYPE, content_type.as_ref()),
            (
                header::CACHE_CONTROL,
                if path == "index.html" {
                    "no-cache"
                } else {
                    "public, max-age=31536000, immutable"
                },
            ),
        ],
        file.data,
    )
        .into_response()
}

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

enum AdminError {
    Internal(anyhow::Error),
    Usage(anyhow::Error),
    BadRequest(&'static str),
    NotFound,
}

impl From<anyhow::Error> for AdminError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "device not found").into_response(),
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message).into_response(),
            Self::Usage(error) => {
                tracing::warn!(error = %format!("{error:#}"), "LLM usage query failed");
                (StatusCode::BAD_GATEWAY, "LLM usage is unavailable").into_response()
            }
            Self::Internal(error) => {
                tracing::error!(%error, "admin API operation failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "controller state error").into_response()
            }
        }
    }
}
