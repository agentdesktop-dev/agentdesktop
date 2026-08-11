use std::{net::SocketAddr, time::SystemTime};

use agentdesktop_proto::fleet::DesiredConfig;
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rust_embed::RustEmbed;
use serde::Serialize;
use tracing::info;

use crate::database::{Database, DeviceDetail, DeviceSummary};

#[derive(Clone)]
pub struct AdminState {
    database: Database,
    desired_config: Option<DesiredConfig>,
    settings: ControllerSettings,
}

#[derive(Clone, Serialize)]
pub struct ControllerSettings {
    pub fleet_listen: String,
    pub admin_listen: String,
    pub enrollment_token_enabled: bool,
    pub oidc_enabled: bool,
    pub tls_enabled: bool,
    pub gateway_jwt_enabled: bool,
}

impl AdminState {
    pub fn new(
        database: Database,
        desired_config: Option<DesiredConfig>,
        settings: ControllerSettings,
    ) -> Self {
        Self {
            database,
            desired_config,
            settings,
        }
    }
}

#[derive(RustEmbed)]
#[folder = "../../ui/dist/"]
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

#[derive(Serialize)]
struct Configuration {
    active: bool,
    revision: Option<u64>,
    sha256: Option<String>,
    yaml: Option<String>,
}

pub async fn serve(address: SocketAddr, state: AdminState) -> anyhow::Result<()> {
    let app = Router::new()
        .route("/api/v1/overview", get(overview))
        .route("/api/v1/devices", get(devices))
        .route(
            "/api/v1/devices/{device_id}",
            get(device).delete(delete_device),
        )
        .route("/api/v1/configuration", get(configuration))
        .route("/api/v1/settings", get(settings))
        .fallback(get(asset))
        .with_state(state);
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
        active_revision: state.desired_config.as_ref().map(|config| config.revision),
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

async fn configuration(State(state): State<AdminState>) -> Json<Configuration> {
    let config = state.desired_config.as_ref();
    Json(Configuration {
        active: config.is_some(),
        revision: config.map(|config| config.revision),
        sha256: config.map(|config| {
            config
                .sha256
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()
        }),
        yaml: config.and_then(|config| String::from_utf8(config.yaml.clone()).ok()),
    })
}

async fn settings(State(state): State<AdminState>) -> Json<ControllerSettings> {
    Json(state.settings)
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
    Database(anyhow::Error),
    NotFound,
}

impl From<anyhow::Error> for AdminError {
    fn from(error: anyhow::Error) -> Self {
        Self::Database(error)
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "device not found").into_response(),
            Self::Database(error) => {
                tracing::error!(%error, "admin API database operation failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "controller state error").into_response()
            }
        }
    }
}
