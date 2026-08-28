use std::{net::SocketAddr, time::SystemTime};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::{
    daemon_config::{ActiveFleetConfiguration, FleetConfiguration, ReplaceFleetConfigurationError},
    database::{Database, DeviceDetail, DeviceSummary},
    gateway_jwt::{self, GatewayJwks},
};

#[derive(Clone)]
pub struct AdminState {
    database: Database,
    fleet_configuration: FleetConfiguration,
    settings: ControllerSettings,
}

#[derive(Clone, Serialize)]
pub struct ControllerSettings {
    pub fleet_listen: String,
    pub admin_listen: String,
    pub oidc_enabled: bool,
    pub tls_enabled: bool,
    pub gateway_jwt_enabled: bool,
}

impl AdminState {
    pub fn new(
        database: Database,
        fleet_configuration: FleetConfiguration,
        settings: ControllerSettings,
    ) -> Self {
        Self {
            database,
            fleet_configuration,
            settings,
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
        .route(
            "/api/v1/fleet-configuration",
            get(fleet_configuration).put(replace_fleet_configuration),
        )
        .route(
            "/api/v1/devices/{device_id}",
            get(device).delete(delete_device),
        )
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
        active_revision: state
            .fleet_configuration
            .store()
            .current()
            .map(|config| config.revision),
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FleetConfigurationResponse {
    yaml: Option<String>,
    revision: Option<u64>,
    version: Option<String>,
    source: Option<&'static str>,
    source_error: Option<String>,
    writable: bool,
}

async fn fleet_configuration(
    State(state): State<AdminState>,
) -> Result<Json<FleetConfigurationResponse>, AdminError> {
    let active = state.fleet_configuration.resolve().await;
    fleet_configuration_response(&state.fleet_configuration, active).map(Json)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReplaceFleetConfiguration {
    version: String,
    yaml: String,
}

async fn replace_fleet_configuration(
    State(state): State<AdminState>,
    Json(request): Json<ReplaceFleetConfiguration>,
) -> Result<Json<FleetConfigurationResponse>, AdminError> {
    let snapshot = state
        .fleet_configuration
        .replace(&request.version, request.yaml)
        .await?;
    fleet_configuration_response(
        &state.fleet_configuration,
        ActiveFleetConfiguration {
            daemon: Some(snapshot.daemon),
            version: Some(snapshot.version),
            source_error: None,
        },
    )
    .map(Json)
}

fn fleet_configuration_response(
    fleet: &FleetConfiguration,
    active: ActiveFleetConfiguration,
) -> Result<FleetConfigurationResponse, AdminError> {
    let (yaml, revision) = match active.daemon {
        Some(daemon) => (
            Some(String::from_utf8(daemon.yaml).context("daemon configuration is not UTF-8")?),
            Some(daemon.revision),
        ),
        None => (None, None),
    };
    let writable = active.version.is_some() && fleet.writable();
    Ok(FleetConfigurationResponse {
        yaml,
        revision,
        version: active.version,
        source: fleet.kind(),
        source_error: active.source_error,
        writable,
    })
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

#[derive(Debug)]
enum AdminError {
    Internal(anyhow::Error),
    NotFound,
    ReadOnly,
    Conflict,
    Invalid(anyhow::Error),
}

impl From<anyhow::Error> for AdminError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl From<ReplaceFleetConfigurationError> for AdminError {
    fn from(error: ReplaceFleetConfigurationError) -> Self {
        match error {
            ReplaceFleetConfigurationError::ReadOnly => Self::ReadOnly,
            ReplaceFleetConfigurationError::Conflict => Self::Conflict,
            ReplaceFleetConfigurationError::Invalid(error) => Self::Invalid(error),
            ReplaceFleetConfigurationError::Backend(error) => Self::Internal(error),
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "device not found").into_response(),
            Self::ReadOnly => (
                StatusCode::METHOD_NOT_ALLOWED,
                "fleet configuration source is read-only",
            )
                .into_response(),
            Self::Conflict => (
                StatusCode::CONFLICT,
                "fleet configuration changed; reload and retry",
            )
                .into_response(),
            Self::Invalid(error) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                format!("invalid fleet configuration: {error:#}"),
            )
                .into_response(),
            Self::Internal(error) => {
                tracing::error!(%error, "admin API operation failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "controller state error").into_response()
            }
        }
    }
}
