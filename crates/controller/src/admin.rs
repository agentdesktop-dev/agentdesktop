use std::{net::SocketAddr, time::SystemTime};

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

use agentdesktop_core::config::DaemonConfig;
use agentdesktop_usage::{UsageError, UsageQuery, UsageReport};

use crate::{
    daemon_config::DaemonConfigStore,
    database::{Database, DeviceDetail, DeviceSummary},
    gateway_jwt::{self, GatewayJwks},
    usage,
};

#[derive(Clone)]
pub struct AdminState {
    database: Database,
    daemon_config: DaemonConfigStore,
    settings: ControllerSettings,
    /// `browserEgress.compliance.pacUrl` from the controller configuration.
    expected_pac_url: Option<url::Url>,
}

#[derive(Clone, Serialize)]
pub struct ControllerSettings {
    pub fleet_listen: String,
    pub admin_listen: String,
    pub oidc_enabled: bool,
    pub tls_enabled: bool,
    pub gateway_jwt_enabled: bool,
    pub usage_enabled: bool,
}

impl AdminState {
    pub fn new(
        database: Database,
        daemon_config: DaemonConfigStore,
        settings: ControllerSettings,
        expected_pac_url: Option<url::Url>,
    ) -> Self {
        Self {
            database,
            daemon_config,
            settings,
            expected_pac_url,
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
    browser_pac: BrowserPacOverview,
}

/// Device counts by browser PAC value, and by compliance state when the
/// fleet configuration has a compliance profile.
#[derive(Serialize, Default)]
struct BrowserPacOverview {
    found: usize,
    absent: usize,
    differs: usize,
    not_reported: usize,
    /// `None` without a compliance profile.
    expected_pac_url: Option<String>,
    compliant: usize,
    not_compliant: usize,
    /// Devices that found a PAC, grouped by URL and SHA-256 of what they
    /// fetched: more than one group per URL means devices get different PACs.
    versions: Vec<crate::browser_pac::PacVersion>,
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
        .route("/api/v1/settings", get(settings))
        .route("/api/v1/usage", get(usage_report))
        .fallback(get(asset))
        .with_state(state)
        .merge(gateway_jwt::routes(gateway_jwks));
    let listener = tokio::net::TcpListener::bind(address).await?;
    info!(listen = %address, "controller admin UI listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Fills in each device's PAC value and compliance at read time, so a
/// profile change shows without waiting for the next inventory.
fn with_browser_pac(state: &AdminState, mut devices: Vec<DeviceSummary>) -> Vec<DeviceSummary> {
    let expected = state.expected_pac_url.as_ref();
    for device in &mut devices {
        device.browser_pac = crate::browser_pac::with_identity(
            crate::browser_pac::evaluate(&device.browser_pacs, expected),
            &device.pac_fetches,
        );
    }
    devices
}

async fn overview(State(state): State<AdminState>) -> Result<Json<Overview>, AdminError> {
    let devices = with_browser_pac(&state, state.database.list_devices().await?);
    let mut browser_pac = BrowserPacOverview {
        expected_pac_url: state.expected_pac_url.clone().map(String::from),
        ..BrowserPacOverview::default()
    };
    browser_pac.versions =
        crate::browser_pac::pac_versions(devices.iter().map(|device| &device.browser_pac));
    for device in &devices {
        match device.browser_pac.state {
            "found" => browser_pac.found += 1,
            "absent" => browser_pac.absent += 1,
            "differs" => browser_pac.differs += 1,
            _ => browser_pac.not_reported += 1,
        }
        match device
            .browser_pac
            .compliance
            .as_ref()
            .map(|compliance| compliance.state)
        {
            Some("compliant") => browser_pac.compliant += 1,
            Some("not_compliant") => browser_pac.not_compliant += 1,
            _ => {}
        }
    }
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
        browser_pac,
    }))
}

async fn devices(State(state): State<AdminState>) -> Result<Json<Vec<DeviceSummary>>, AdminError> {
    Ok(Json(with_browser_pac(
        &state,
        state.database.list_devices().await?,
    )))
}

async fn device(
    State(state): State<AdminState>,
    Path(device_id): Path<String>,
) -> Result<Json<DeviceDetail>, AdminError> {
    let mut detail = state
        .database
        .get_device(&device_id)
        .await?
        .ok_or(AdminError::NotFound)?;
    detail.device.browser_pac = crate::browser_pac::with_identity(
        crate::browser_pac::evaluate(&detail.device.browser_pacs, state.expected_pac_url.as_ref()),
        &detail.device.pac_fetches,
    );
    Ok(Json(detail))
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

async fn usage_report(
    State(state): State<AdminState>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<UsageReport>, AdminError> {
    let labels = usage::labels(&state.database).await?;
    Ok(Json(
        agentdesktop_usage::report(
            &state.database.usage(),
            &query,
            state.settings.usage_enabled,
            &labels,
        )
        .await?,
    ))
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
    NotFound,
    BadRequest(&'static str),
}

impl From<anyhow::Error> for AdminError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl From<UsageError> for AdminError {
    fn from(error: UsageError) -> Self {
        match error {
            UsageError::InvalidQuery(message) => Self::BadRequest(message),
            UsageError::Storage(error) => Self::Internal(error),
        }
    }
}

impl IntoResponse for AdminError {
    fn into_response(self) -> Response {
        match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "device not found").into_response(),
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message).into_response(),
            Self::Internal(error) => {
                tracing::error!(%error, "admin API operation failed");
                (StatusCode::INTERNAL_SERVER_ERROR, "controller state error").into_response()
            }
        }
    }
}
