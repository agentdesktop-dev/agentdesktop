use std::time::SystemTime;

use agentdesktop_usage::{UsageError, UsageQuery, UsageReport};
use anyhow::Context as _;
use axum::{
    Json,
    body::to_bytes,
    extract::{Path, Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use tracing::info;

use super::{
    Settings, State as ManagementState,
    auth::{
        Context, Error,
        authorization::{self, AccessRequest, Verb},
    },
};
use crate::{
    database::{
        DaemonConfigAuthor, DaemonConfigRevision, DaemonConfigRevisionSummary, DeviceDetail,
        DeviceSummary,
    },
    usage,
};

#[derive(Serialize)]
pub(super) struct Overview {
    total_devices: usize,
    online_devices: usize,
    offline_devices: usize,
    config_failures: usize,
    active_revision: Option<u64>,
    recent_devices: Vec<DeviceSummary>,
}

pub(super) async fn api_not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "not_found" })),
    )
}

pub(super) async fn overview(
    State(state): State<ManagementState>,
    auth: Context,
) -> Result<Json<Overview>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::List,
        resource_class: authorization::DEVICES,
        resource: None,
    })
    .await?;
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

pub(super) async fn devices(
    State(state): State<ManagementState>,
    auth: Context,
) -> Result<Json<Vec<DeviceSummary>>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::List,
        resource_class: authorization::DEVICES,
        resource: None,
    })
    .await?;
    Ok(Json(state.database.list_devices().await?))
}

pub(super) async fn device(
    State(state): State<ManagementState>,
    auth: Context,
    Path(device_id): Path<String>,
) -> Result<Json<DeviceDetail>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Read,
        resource_class: authorization::DEVICES,
        resource: Some(&device_id),
    })
    .await?;
    state
        .database
        .get_device(&device_id)
        .await?
        .map(Json)
        .ok_or(HandlerError::NotFound)
}

pub(super) async fn delete_device(
    State(state): State<ManagementState>,
    auth: Context,
    Path(device_id): Path<String>,
) -> Result<StatusCode, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Delete,
        resource_class: authorization::DEVICES,
        resource: Some(&device_id),
    })
    .await?;
    if !state.database.delete_device(&device_id).await? {
        return Err(HandlerError::NotFound);
    }
    info!(%device_id, "deleted device from controller");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub(super) struct SettingsResponse {
    #[serde(flatten)]
    settings: Settings,
    version: &'static str,
}

pub(super) async fn settings(
    State(state): State<ManagementState>,
    auth: Context,
) -> Result<Json<SettingsResponse>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Read,
        resource_class: authorization::SETTINGS,
        resource: None,
    })
    .await?;
    Ok(Json(SettingsResponse {
        settings: state.settings,
        version: agentdesktop_core::VERSION,
    }))
}

pub(super) async fn usage_report(
    State(state): State<ManagementState>,
    auth: Context,
    Query(query): Query<UsageQuery>,
) -> Result<Json<UsageReport>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Read,
        resource_class: authorization::USAGE,
        resource: None,
    })
    .await?;
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
pub(super) struct ActiveDaemonConfig {
    controller_version: &'static str,
    config: Option<serde_json::Value>,
    yaml: Option<String>,
    revision: Option<u64>,
    sha256: Option<String>,
    source: Option<&'static str>,
    writable: bool,
    created_at: Option<i64>,
    author: Option<DaemonConfigAuthor>,
    message: Option<String>,
    reload_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    changed: Option<bool>,
}

pub(super) async fn daemon_config(
    State(state): State<ManagementState>,
    auth: Context,
) -> Result<Json<ActiveDaemonConfig>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Read,
        resource_class: authorization::DAEMON_CONFIG,
        resource: None,
    })
    .await?;
    Ok(Json(active_daemon_config(&state).await?))
}

const MAX_DAEMON_CONFIG_BYTES: usize = 256 * 1024;
const MAX_DAEMON_CONFIG_BODY_BYTES: usize = 2 * 1024 * 1024;
const MAX_DAEMON_CONFIG_MESSAGE_BYTES: usize = 1024;
const DEFAULT_REVISION_PAGE_SIZE: u32 = 50;
const MAX_REVISION_PAGE_SIZE: u32 = 100;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateDaemonConfigRequest {
    base_revision: i64,
    yaml: String,
    #[serde(default)]
    message: Option<String>,
}

pub(super) async fn update_daemon_config(
    State(state): State<ManagementState>,
    auth: Context,
    request: Request,
) -> Result<Json<ActiveDaemonConfig>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::Update,
        resource_class: authorization::DAEMON_CONFIG,
        resource: None,
    })
    .await?;
    if state.daemon_config_source != super::DaemonConfigSource::Database {
        return Err(HandlerError::ReadOnlySource);
    }

    let body = to_bytes(request.into_body(), MAX_DAEMON_CONFIG_BODY_BYTES)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "failed to read daemon configuration request body");
            HandlerError::TooLarge
        })?;
    let request: UpdateDaemonConfigRequest =
        serde_json::from_slice(&body).map_err(|_| HandlerError::BadRequest("invalid_request"))?;
    if request.base_revision < 0 {
        return Err(HandlerError::BadRequest("invalid_request"));
    }
    if request.yaml.len() > MAX_DAEMON_CONFIG_BYTES {
        return Err(HandlerError::TooLarge);
    }
    if request
        .message
        .as_ref()
        .is_some_and(|message| message.len() > MAX_DAEMON_CONFIG_MESSAGE_BYTES)
    {
        return Err(HandlerError::BadRequest("invalid_request"));
    }
    agentdesktop_core::config::parse_daemon(&request.yaml)
        .map_err(|error| HandlerError::InvalidDaemonConfig(format!("{error:#}")))?;

    let latest = state.database.latest_daemon_config().await?;
    let latest_revision = latest.as_ref().map_or(0, |row| row.summary.revision);
    if request.base_revision != latest_revision {
        return Err(HandlerError::RevisionConflict {
            revision: latest_revision,
        });
    }
    if let Some(latest) = latest
        && latest.yaml == request.yaml
    {
        return Ok(Json(active_database_daemon_config(latest, Some(false))?));
    }

    let author = principal_as_config_author(auth.principal());
    let inserted = state
        .database
        .insert_daemon_config_revision(
            request.base_revision,
            &request.yaml,
            &author,
            request.message.as_deref().unwrap_or_default(),
        )
        .await?;
    let Some(inserted) = inserted else {
        let revision = state
            .database
            .latest_daemon_config_revision()
            .await?
            .unwrap_or(0);
        return Err(HandlerError::RevisionConflict { revision });
    };
    let proto =
        crate::daemon_config::from_database(inserted.summary.revision, inserted.yaml.clone())?;
    state.daemon_config.publish(proto);
    info!(
        old_revision = request.base_revision,
        new_revision = inserted.summary.revision,
        sha256 = %inserted.summary.sha256,
        "daemon configuration saved"
    );
    Ok(Json(active_database_daemon_config(inserted, Some(true))?))
}

#[derive(Deserialize)]
pub(super) struct RevisionListQuery {
    before: Option<u64>,
    limit: Option<u32>,
}

#[derive(Serialize)]
pub(super) struct DaemonConfigRevisionList {
    revisions: Vec<DaemonConfigRevisionSummary>,
}

pub(super) async fn daemon_config_revisions(
    State(state): State<ManagementState>,
    auth: Context,
    Query(query): Query<RevisionListQuery>,
) -> Result<Json<DaemonConfigRevisionList>, HandlerError> {
    auth.require(AccessRequest {
        verb: Verb::List,
        resource_class: authorization::DAEMON_CONFIG,
        resource: None,
    })
    .await?;
    if state.daemon_config_source != super::DaemonConfigSource::Database {
        return Err(HandlerError::HistoryUnavailable);
    }
    let before = query
        .before
        .map(i64::try_from)
        .transpose()
        .map_err(|_| HandlerError::BadRequest("invalid_request"))?;
    let limit = i64::from(
        query
            .limit
            .unwrap_or(DEFAULT_REVISION_PAGE_SIZE)
            .clamp(1, MAX_REVISION_PAGE_SIZE),
    );
    Ok(Json(DaemonConfigRevisionList {
        revisions: state
            .database
            .list_daemon_config_revisions(before, limit)
            .await?,
    }))
}

pub(super) async fn daemon_config_revision(
    State(state): State<ManagementState>,
    auth: Context,
    Path(revision): Path<u64>,
) -> Result<Json<DaemonConfigRevision>, HandlerError> {
    let revision_i64 =
        i64::try_from(revision).map_err(|_| HandlerError::BadRequest("invalid_request"))?;
    let revision_resource = revision.to_string();
    auth.require(AccessRequest {
        verb: Verb::Read,
        resource_class: authorization::DAEMON_CONFIG,
        resource: Some(&revision_resource),
    })
    .await?;
    if state.daemon_config_source != super::DaemonConfigSource::Database {
        return Err(HandlerError::HistoryUnavailable);
    }
    state
        .database
        .daemon_config_revision(revision_i64)
        .await?
        .map(Json)
        .ok_or(HandlerError::RevisionNotFound)
}

async fn active_daemon_config(state: &ManagementState) -> Result<ActiveDaemonConfig, HandlerError> {
    match state.daemon_config_source {
        super::DaemonConfigSource::Database => match state.database.latest_daemon_config().await? {
            Some(revision) => active_database_daemon_config(revision, None),
            None => Ok(ActiveDaemonConfig {
                controller_version: agentdesktop_core::VERSION,
                config: None,
                yaml: None,
                revision: None,
                sha256: None,
                source: Some("database"),
                writable: true,
                created_at: None,
                author: None,
                message: None,
                reload_error: None,
                changed: None,
            }),
        },
        super::DaemonConfigSource::File => {
            let active = state.daemon_config.current();
            match active {
                Some(active) => {
                    let yaml =
                        String::from_utf8(active.yaml).context("daemon config is not UTF-8")?;
                    let config = agentdesktop_core::config::parse_daemon(&yaml)?;
                    Ok(ActiveDaemonConfig {
                        controller_version: agentdesktop_core::VERSION,
                        config: Some(serde_json::to_value(config)?),
                        yaml: Some(yaml),
                        revision: Some(active.revision),
                        sha256: Some(hex(&active.sha256)),
                        source: Some("file"),
                        writable: false,
                        created_at: None,
                        author: None,
                        message: None,
                        reload_error: state.daemon_config.reload_error(),
                        changed: None,
                    })
                }
                None => Ok(ActiveDaemonConfig {
                    controller_version: agentdesktop_core::VERSION,
                    config: None,
                    yaml: None,
                    revision: None,
                    sha256: None,
                    source: Some("file"),
                    writable: false,
                    created_at: None,
                    author: None,
                    message: None,
                    reload_error: state.daemon_config.reload_error(),
                    changed: None,
                }),
            }
        }
        super::DaemonConfigSource::None => Ok(ActiveDaemonConfig {
            controller_version: agentdesktop_core::VERSION,
            config: None,
            yaml: None,
            revision: None,
            sha256: None,
            source: None,
            writable: false,
            created_at: None,
            author: None,
            message: None,
            reload_error: None,
            changed: None,
        }),
    }
}

fn active_database_daemon_config(
    revision: DaemonConfigRevision,
    changed: Option<bool>,
) -> Result<ActiveDaemonConfig, HandlerError> {
    let config = agentdesktop_core::config::parse_daemon(&revision.yaml)?;
    Ok(ActiveDaemonConfig {
        controller_version: agentdesktop_core::VERSION,
        config: Some(serde_json::to_value(config)?),
        yaml: Some(revision.yaml),
        revision: Some(
            u64::try_from(revision.summary.revision)
                .context("daemon configuration revision is out of range")?,
        ),
        sha256: Some(revision.summary.sha256),
        source: Some("database"),
        writable: true,
        created_at: Some(revision.summary.created_at),
        author: Some(revision.summary.author),
        message: Some(revision.summary.message),
        reload_error: None,
        changed,
    })
}

fn principal_as_config_author(
    principal: &super::auth::authentication::Principal,
) -> DaemonConfigAuthor {
    use super::auth::authentication::PrincipalKind;

    let (kind, client_id) = match &principal.kind {
        PrincipalKind::Local => ("local", None),
        PrincipalKind::Human => ("human", None),
        PrincipalKind::Service { client_id } => ("service", Some(client_id.clone())),
    };
    let display = ["email", "preferred_username", "name"]
        .into_iter()
        .find_map(|claim| {
            principal
                .claims
                .get(claim)?
                .as_str()
                .filter(|value| !value.is_empty())
        })
        .map(str::to_owned);
    DaemonConfigAuthor {
        kind: kind.to_owned(),
        issuer: principal.issuer.clone(),
        subject: principal.subject.clone(),
        client_id,
        display,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unix_time_seconds() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

pub(super) enum HandlerError {
    Auth(Error),
    Internal(anyhow::Error),
    NotFound,
    BadRequest(&'static str),
    ReadOnlySource,
    HistoryUnavailable,
    RevisionNotFound,
    RevisionConflict { revision: i64 },
    TooLarge,
    InvalidDaemonConfig(String),
}

impl From<Error> for HandlerError {
    fn from(error: Error) -> Self {
        Self::Auth(error)
    }
}
impl From<anyhow::Error> for HandlerError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}
impl From<serde_json::Error> for HandlerError {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(error.into())
    }
}

impl From<UsageError> for HandlerError {
    fn from(error: UsageError) -> Self {
        match error {
            UsageError::InvalidQuery(message) => Self::BadRequest(message),
            UsageError::Storage(error) => Self::Internal(error),
        }
    }
}

impl IntoResponse for HandlerError {
    fn into_response(self) -> Response {
        match self {
            Self::Auth(error) => error.into_response(),
            Self::NotFound => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "device not found" })),
            )
                .into_response(),
            Self::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": message })),
            )
                .into_response(),
            Self::ReadOnlySource => {
                let mut response = (
                    StatusCode::METHOD_NOT_ALLOWED,
                    Json(serde_json::json!({ "error": "read_only_source" })),
                )
                    .into_response();
                response
                    .headers_mut()
                    .insert(header::ALLOW, HeaderValue::from_static("GET"));
                response
            }
            Self::HistoryUnavailable => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "history_unavailable" })),
            )
                .into_response(),
            Self::RevisionNotFound => (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "revision_not_found" })),
            )
                .into_response(),
            Self::RevisionConflict { revision } => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "revision_conflict",
                    "revision": revision
                })),
            )
                .into_response(),
            Self::TooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({ "error": "too_large" })),
            )
                .into_response(),
            Self::InvalidDaemonConfig(detail) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "invalid_daemon_config",
                    "detail": detail
                })),
            )
                .into_response(),
            Self::Internal(error) => {
                tracing::error!(%error, "admin API operation failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "controller state error" })),
                )
                    .into_response()
            }
        }
    }
}
