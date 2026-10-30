use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use axum::{
    Extension, Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot, watch};

use agentdesktop_core::{
    config::{
        ControllerConnectionConfig, DaemonConfig, LlmGatewayAuthentication, ProgramAuthentication,
        VsCodeCopilotChat, valid_client_id,
    },
    model::{
        DaemonInfo, DeviceUsage, Discovery, EnrollmentStatus, Health, LlmGatewayCredential,
        LlmGatewayLoginStatus, ManagedProgram, ProgramChoice, ProgramManagement, TelemetryEvent,
        TelemetryEventKind,
    },
};
use agentdesktop_proto::fleet::DeviceUsageRequest;
use agentdesktop_usage::{UsageError, UsageStore};

use crate::{
    enrollment::EnrollmentState,
    gateway_oidc, program_choice,
    reconcile::{Reconciler, apply_program_choice, configured_programs},
    remote, subscription,
    tick::{self, CurrentConfig, TickStatus},
};

#[derive(Clone)]
pub struct AppState {
    pub config: DaemonConfig,
    pub daemon_info: DaemonInfo,
    pub discovery: watch::Receiver<Arc<Discovery>>,
    pub enrollment: EnrollmentState,
    pub controller_status: Option<remote::ControllerConnectionState>,
    pub state_dir: PathBuf,
    pub oidc_callback_listen: Option<SocketAddr>,
    pub telemetry: Option<mpsc::Sender<TelemetryEvent>>,
    pub logout: Option<mpsc::Sender<remote::LogoutRequest>>,
    /// This device's usage, recorded by the daemon itself (`daemon.usage`).
    pub usage: Option<UsageStore>,
    /// The device user's program choice; `None` serves no program routes.
    pub programs: Option<ProgramControl>,
    /// Distinct native runs of wrap-mode programs' managed executables,
    /// counted by the violation scan since daemon start (report-only).
    /// `Some` only in a Windows user-mode daemon whose `daemon.containment`
    /// is present; daemon-info fills per-program posture from it.
    pub(crate) violation_ledger:
        Option<Arc<std::sync::Mutex<crate::launch::wrap::ViolationLedger>>>,
}

/// Reads and changes the device user's program choice, and re-applies the
/// current configuration when it changes.
#[derive(Clone)]
pub struct ProgramControl {
    reconciler: Reconciler,
    current: watch::Receiver<Option<CurrentConfig>>,
    /// Where a re-apply's outcome goes for the controller; `None` without one.
    statuses: Option<watch::Sender<Option<TickStatus>>>,
    state_dir: PathBuf,
    /// Serializes changes, so the stored and the applied choice agree.
    changing: Arc<std::sync::Mutex<()>>,
}

impl ProgramControl {
    pub(crate) fn new(
        reconciler: Reconciler,
        current: watch::Receiver<Option<CurrentConfig>>,
        statuses: Option<watch::Sender<Option<TickStatus>>>,
        state_dir: PathBuf,
    ) -> Self {
        Self {
            reconciler,
            current,
            statuses,
            state_dir,
            changing: Arc::default(),
        }
    }

    /// The configured programs, whether each is managed under the device
    /// user's choice, and each one's outcome from the last apply. Waits for
    /// an apply in progress.
    fn management(&self) -> ProgramManagement {
        let choice = self.reconciler.program_choice();
        let current = self.current.borrow().clone();
        let Some(current) = current else {
            return ProgramManagement {
                user_choice_allowed: false,
                choice,
                programs: Vec::new(),
            };
        };
        let report = self.reconciler.last_report();
        let (_, opted_out) = apply_program_choice(&current.config, &choice);
        let programs = configured_programs(&current.config.programs)
            .into_iter()
            .map(|program| {
                let outcome = report
                    .as_ref()
                    .and_then(|report| report.programs.iter().find(|o| o.program == program));
                ManagedProgram {
                    program: program.to_owned(),
                    managed: !opted_out.contains(&program),
                    state: outcome.map(|outcome| outcome.state.as_str().to_owned()),
                    detail: outcome
                        .map(|outcome| outcome.detail.clone())
                        .unwrap_or_default(),
                }
            })
            .collect();
        ProgramManagement {
            user_choice_allowed: current.config.program_selection.allow_user_choice,
            choice,
            programs,
        }
    }

    /// Stores `choice`, re-applies the current configuration with it, and
    /// reports the outcome to the controller. Refused while the current
    /// configuration does not allow a choice, or before there is one.
    fn change(&self, choice: ProgramChoice) -> Result<ProgramManagement, (StatusCode, String)> {
        program_choice::validate(&choice)
            .map_err(|error| (StatusCode::BAD_REQUEST, format!("{error:#}")))?;
        let _changing = self
            .changing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let allowed = self
            .current
            .borrow()
            .as_ref()
            .map(|current| current.config.program_selection.allow_user_choice);
        match allowed {
            None => {
                return Err((
                    StatusCode::CONFLICT,
                    "this device has no configuration yet; choose once it has one".to_owned(),
                ));
            }
            Some(false) => {
                return Err((
                    StatusCode::FORBIDDEN,
                    "this device's policy manages every configured program (programSelection.allowUserChoice is off)".to_owned(),
                ));
            }
            Some(true) => {}
        }
        program_choice::save(&self.state_dir, &choice).map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("save program choice: {error:#}"),
            )
        })?;
        tracing::info!(?choice, "program choice changed");
        self.reconciler.set_program_choice(choice);
        if let Some((revision, _, report, result)) =
            tick::apply_current(&self.reconciler, &self.current)
        {
            report.log();
            let error = result.err().map(|error| format!("{error:#}"));
            if let Some(error) = &error {
                tracing::warn!(%error, "applying the changed program choice failed");
            }
            if let Some(statuses) = &self.statuses {
                tick::publish(
                    statuses,
                    TickStatus {
                        revision,
                        report,
                        error,
                    },
                );
            }
        }
        // A choice that brings a gateway program back starts the sign-in that
        // startup or the push skipped while it was left out (idempotent).
        let current = self.current.borrow().clone();
        if let Some(current) = current
            && self.reconciler.gateway_sign_in_wanted(&current.config)
        {
            gateway_oidc::start_device_login_if_configured(&current.config, &self.state_dir);
        }
        Ok(self.management())
    }
}

#[derive(Deserialize)]
struct CredentialQuery {
    client_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageQuery {
    range: String,
    #[serde(default)]
    utc_offset_minutes: i32,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/daemon-info", get(daemon_info))
        .route("/v1/config", get(config))
        .route("/v1/effective-config", get(effective_config))
        .route("/v1/remote-config", get(remote_config))
        .route("/v1/discovery", get(discover))
        .route("/v1/enrollment", get(enrollment))
        .route("/v1/logout", post(logout))
        .route("/v1/telemetry", post(telemetry))
        .route("/v1/llm-gateway/credential", get(llm_gateway_credential))
        .route("/v1/llm-gateway/login", post(llm_gateway_login))
        .route("/v1/usage", get(device_usage))
        .route("/v1/programs", get(programs))
        .route("/v1/programs/choice", post(change_program_choice))
        .layer(Extension(Arc::new(
            crate::llm_proxy::CredentialCache::default(),
        )))
        .with_state(state)
}

fn program_control(state: &AppState) -> Result<ProgramControl, (StatusCode, String)> {
    state.programs.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "program management is not available".to_owned(),
        )
    })
}

/// Runs a program route's blocking work (it waits for an apply in progress).
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, (StatusCode, String)> + Send + 'static,
) -> Result<Json<T>, (StatusCode, String)> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?
        .map(Json)
}

async fn programs(
    State(state): State<AppState>,
) -> Result<Json<ProgramManagement>, (StatusCode, String)> {
    let control = program_control(&state)?;
    blocking(move || Ok(control.management())).await
}

async fn change_program_choice(
    State(state): State<AppState>,
    Json(choice): Json<ProgramChoice>,
) -> Result<Json<ProgramManagement>, (StatusCode, String)> {
    let control = program_control(&state)?;
    blocking(move || control.change(choice)).await
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

async fn health(State(state): State<AppState>) -> Json<Health> {
    let controller = match &state.controller_status {
        Some(controller_status) => Some(controller_status.get().await),
        None => None,
    };
    Json(Health {
        status: "ok".to_owned(),
        controller,
    })
}

async fn daemon_info(State(state): State<AppState>) -> Json<DaemonInfo> {
    let mut info = state.daemon_info;
    if cfg!(windows)
        && let Some(containment) = info.containment.as_mut()
    {
        match tokio::task::spawn_blocking(
            crate::provider::claude_code::managed::managed_posture_now,
        )
        .await
        {
            Ok(posture) => {
                containment.managed_settings_source = posture.source;
                containment.managed_hooks_locked = posture.hooks_locked;
                containment.managed_mcp_locked = posture.mcp_locked;
                containment.managed_settings_error = posture.error;
            }
            Err(error) => {
                tracing::debug!("failed to join managed-settings posture task: {error:#}");
            }
        }
    }
    // Per-program wrap posture, device-local: mode and executable from the
    // applied configuration, launcher presence, and the violation ledger.
    // Absent off Windows and outside a user-mode daemon with
    // `daemon.containment` (the ledger exists only there).
    if let Some(ledger) = state.violation_ledger.clone()
        && info.containment.is_some()
    {
        let config = state.config.clone();
        let state_dir = state.state_dir.clone();
        match tokio::task::spawn_blocking(move || wrap_postures(&config, &state_dir, &ledger)).await
        {
            Ok(programs) => {
                if let Some(containment) = info.containment.as_mut() {
                    containment.programs = programs;
                }
            }
            Err(error) => {
                tracing::debug!("failed to join wrap posture task: {error:#}");
            }
        }
    }
    Json(info)
}

/// The wrap-mode programs' posture for daemon-info, read per request from the
/// applied configuration and the violation ledger. An unreadable applied
/// configuration reports no posture (the launch route answers 500 for it).
fn wrap_postures(
    config: &DaemonConfig,
    state_dir: &std::path::Path,
    ledger: &std::sync::Mutex<crate::launch::wrap::ViolationLedger>,
) -> std::collections::BTreeMap<String, agentdesktop_core::model::ProgramPosture> {
    let effective = match load_effective_config(config, state_dir) {
        Ok(effective) => effective,
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "wrap posture skipped: the applied configuration is unreadable"
            );
            return std::collections::BTreeMap::new();
        }
    };
    let Some(policy) = effective.containment else {
        return std::collections::BTreeMap::new();
    };
    let ledger = ledger
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    crate::launch::wrap::program_postures(&policy, &ledger)
}

async fn config(State(state): State<AppState>) -> Json<DaemonConfig> {
    Json(state.config)
}

async fn effective_config(
    State(state): State<AppState>,
) -> Result<Json<DaemonConfig>, (StatusCode, String)> {
    load_effective_config(&state.config, &state.state_dir)
        .map(Json)
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read applied configuration: {error:#}"),
            )
        })
}

pub(crate) fn load_effective_config(
    config: &DaemonConfig,
    state_dir: &std::path::Path,
) -> anyhow::Result<DaemonConfig> {
    if config.controller.is_none() {
        return Ok(config.clone());
    }
    let path = state_dir.join("remote-config.yaml");
    match std::fs::read_to_string(&path) {
        Ok(contents) => agentdesktop_core::config::parse_daemon(&contents)
            .map_err(|error| error.context("parse applied remote configuration")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Before the first remote configuration is applied, the local
            // file still backs the gateway and program fields, but with a
            // controller configured only the controller-delivered
            // `containment` block ever applies: a local block must not make
            // wrapped launches (or the violation scan) behave differently
            // before and after the first push.
            let mut config = config.clone();
            config.containment = None;
            Ok(config)
        }
        Err(error) => Err(error.into()),
    }
}

async fn remote_config(
    State(state): State<AppState>,
) -> Result<Json<Option<String>>, (StatusCode, String)> {
    let path = state.state_dir.join("remote-config.yaml");
    match std::fs::read_to_string(path) {
        Ok(contents) => Ok(Json(Some(contents))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Json(None)),
        Err(error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read applied remote configuration: {error}"),
        )),
    }
}

async fn discover(State(state): State<AppState>) -> Json<Discovery> {
    Json((**state.discovery.borrow()).clone())
}

async fn enrollment(State(state): State<AppState>) -> Json<EnrollmentStatus> {
    Json(state.enrollment.get().await)
}

async fn logout(State(state): State<AppState>) -> Result<StatusCode, (StatusCode, String)> {
    let sender = state.logout.as_ref().ok_or_else(|| {
        (
            StatusCode::FAILED_DEPENDENCY,
            "daemon has no controller session to log out".to_owned(),
        )
    })?;
    let (completion, completed) = oneshot::channel();
    sender
        .send(remote::LogoutRequest { completion })
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "controller session is unavailable".to_owned(),
            )
        })?;
    completed
        .await
        .map_err(|_| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "controller session stopped before logout completed".to_owned(),
            )
        })?
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn llm_gateway_credential(
    State(state): State<AppState>,
    Extension(cache): Extension<Arc<crate::llm_proxy::CredentialCache>>,
    Query(query): Query<CredentialQuery>,
) -> Result<Json<LlmGatewayCredential>, (StatusCode, String)> {
    if !valid_client_id(&query.client_id) {
        return Err((StatusCode::BAD_REQUEST, "invalid client ID".to_owned()));
    }
    let effective = load_effective_config(&state.config, &state.state_dir).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read applied configuration: {error:#}"),
        )
    })?;
    // Pi executes its helper on every request. Reuse the proxy's 60 s cache
    // for controller identities; OIDC and subscription flows keep their own
    // refresh behavior. Read identity.json on each call so logout/re-enrollment
    // cannot reuse another device's token.
    if !program_uses_subscription(&effective, &query.client_id)
        && let Some(gateway) = effective.llm_gateway.as_ref()
        && let Some(LlmGatewayAuthentication::ControllerJwt {
            audience,
            allowed_client_ids,
        }) = &gateway.authentication
        && allowed_client_ids.contains(&query.client_id)
        && let Ok(Some(device_id)) =
            crate::identity::load_device_id(&state.state_dir.join("identity.json"))
    {
        let key = format!("{}\0{}\0{audience}", query.client_id, gateway.url);
        return cache
            .get_credential(&key, &device_id, || {
                gateway_credential(&state, &effective, &query.client_id, true)
            })
            .await
            .map(Json);
    }
    gateway_credential(&state, &effective, &query.client_id, true)
        .await
        .map(Json)
}

/// With the device grant selected, a missing or unrefreshable OIDC token is a
/// `401` instead of a browser sign-in: a background refresh never prompts.
pub(crate) fn non_interactive(mut effective: DaemonConfig) -> DaemonConfig {
    if let Some(gateway) = effective.llm_gateway.as_mut()
        && let Some(LlmGatewayAuthentication::Oidc {
            device_authorization,
            ..
        }) = gateway.authentication.as_mut()
    {
        *device_authorization = true;
    }
    effective
}

pub(crate) async fn gateway_credential(
    state: &AppState,
    effective: &DaemonConfig,
    client_id: &str,
    include_subscription: bool,
) -> Result<LlmGatewayCredential, (StatusCode, String)> {
    let gateway = effective.llm_gateway.as_ref().ok_or_else(|| {
        (
            StatusCode::FAILED_DEPENDENCY,
            "daemon has no LLM gateway configured".to_owned(),
        )
    })?;
    let uses_subscription = include_subscription && program_uses_subscription(effective, client_id);
    let (identity, continue_in_browser) = match gateway.authentication.as_ref() {
        Some(LlmGatewayAuthentication::ControllerJwt { .. }) => {
            let controller = state.config.controller.as_ref().ok_or_else(|| {
                (
                    StatusCode::FAILED_DEPENDENCY,
                    "controller JWT authentication requires a controller".to_owned(),
                )
            })?;
            if uses_subscription {
                return controller_subscription_credential(state, controller, client_id).await;
            }
            // Local transport permissions authenticate the user, not the calling
            // process. The client ID selects an allowed policy within that boundary.
            remote::llm_gateway_credential(controller, &state.state_dir, client_id, false)
                .await
                .map(|credential| (credential, false))
        }
        Some(LlmGatewayAuthentication::Oidc {
            issuer,
            client_id,
            redirect_uri,
            scopes,
            allow_insecure,
            device_authorization,
        }) => gateway_oidc::credential(
            issuer,
            client_id,
            redirect_uri,
            scopes,
            *allow_insecure,
            &state.state_dir,
            gateway_oidc::LoginOptions {
                callback_listen: state.oidc_callback_listen,
                subscription_available: uses_subscription,
                github_client_id: if include_subscription {
                    None
                } else {
                    // Same reasoning as the daemon login path: a GitHub sign-in
                    // step only makes sense for the device flow.
                    gateway.github_oauth.as_ref().and_then(|github| {
                        matches!(
                            github.source,
                            agentdesktop_core::config::GitHubTokenSource::DeviceFlow
                        )
                        .then(|| github.client_id.clone())
                        .flatten()
                    })
                },
                device_authorization: *device_authorization,
            },
        )
        .await
        .map(|acquired| {
            (
                acquired.credential,
                acquired.interactive && uses_subscription,
            )
        }),
        None => Err(anyhow::anyhow!(
            "LLM gateway has no authentication configured"
        )),
    }
    .map_err(|error| {
        let status = if error.is::<gateway_oidc::SignInRequired>() {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::BAD_GATEWAY
        };
        (status, format!("{error:#}"))
    })?;
    if uses_subscription {
        subscription::compose(
            identity,
            &state.state_dir,
            state.oidc_callback_listen,
            continue_in_browser,
        )
        .await
    } else {
        Ok(identity)
    }
    .map_err(|error| (StatusCode::BAD_GATEWAY, format!("{error:#}")))
}

/// A subscription credential composed with a controller identity. The
/// subscription is resolved first so the controller marks the identity as
/// subscription-billed only when a subscription token is sent with it.
async fn controller_subscription_credential(
    state: &AppState,
    controller: &ControllerConnectionConfig,
    client_id: &str,
) -> Result<LlmGatewayCredential, (StatusCode, String)> {
    let bad_gateway = |error: anyhow::Error| (StatusCode::BAD_GATEWAY, format!("{error:#}"));
    let subscription =
        subscription::credential(&state.state_dir, state.oidc_callback_listen, false)
            .await
            .map_err(bad_gateway)?;
    let identity = remote::llm_gateway_credential(
        controller,
        &state.state_dir,
        client_id,
        subscription.is_some(),
    )
    .await
    .map_err(bad_gateway)?;
    Ok(subscription::combine(subscription, identity))
}

async fn device_usage(
    State(state): State<AppState>,
    Query(query): Query<UsageQuery>,
) -> Result<Json<DeviceUsage>, (StatusCode, String)> {
    if !matches!(query.range.as_str(), "24h" | "7d" | "30d") {
        return Err((
            StatusCode::BAD_REQUEST,
            "range must be 24h, 7d, or 30d".to_owned(),
        ));
    }
    let effective = load_effective_config(&state.config, &state.state_dir).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read applied configuration: {error:#}"),
        )
    })?;
    let mut usage = match (&state.usage, &state.config.controller) {
        (Some(store), _) => {
            let request = DeviceUsageRequest {
                range: query.range,
                utc_offset_minutes: query.utc_offset_minutes,
            };
            agentdesktop_usage::device_usage(store, None, &request, true)
                .await
                .map(remote::device_usage_from_proto)
                .map_err(|error| match error {
                    UsageError::InvalidQuery(message) => {
                        (StatusCode::BAD_REQUEST, message.to_owned())
                    }
                    UsageError::Storage(error) => (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("read usage: {error:#}"),
                    ),
                })?
        }
        (None, Some(controller)) => remote::device_usage(
            controller,
            &state.state_dir,
            &query.range,
            query.utc_offset_minutes,
        )
        .await
        .map_err(|error| (StatusCode::BAD_GATEWAY, format!("{error:#}")))?,
        (None, None) => {
            return Err((
                StatusCode::FAILED_DEPENDENCY,
                match &state.daemon_info.usage_receiver {
                    Some(receiver) => format!(
                        "usage database is unavailable: {}",
                        receiver.error.as_deref().unwrap_or("unknown error")
                    ),
                    None => "daemon has no controller or daemon.usage configured".to_owned(),
                },
            ));
        }
    };
    usage.unmetered_tools = unmetered_tools(&effective);
    Ok(Json(usage))
}

/// Configured tools whose gateway traffic has no usage records: VS Code on
/// GitHub's own models reaches GitHub through the gateway's pass-through
/// route, which is not an LLM route.
fn unmetered_tools(config: &DaemonConfig) -> Vec<String> {
    config
        .programs
        .vscode
        .as_ref()
        .filter(|vscode| {
            config.llm_gateway.is_some()
                && vscode.use_llm_gateway
                && vscode.copilot_chat == VsCodeCopilotChat::GithubModels
        })
        .map(|_| vec!["vscode-copilot".to_owned()])
        .unwrap_or_default()
}

async fn llm_gateway_login(
    State(state): State<AppState>,
) -> Result<Json<LlmGatewayLoginStatus>, (StatusCode, String)> {
    let effective = load_effective_config(&state.config, &state.state_dir).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read applied configuration: {error:#}"),
        )
    })?;
    let Some(LlmGatewayAuthentication::Oidc {
        issuer,
        client_id,
        scopes,
        allow_insecure,
        device_authorization: true,
        ..
    }) = effective
        .llm_gateway
        .as_ref()
        .and_then(|gateway| gateway.authentication.as_ref())
    else {
        return Err((
            StatusCode::FAILED_DEPENDENCY,
            "LLM gateway sign-in with device authorization is not configured".to_owned(),
        ));
    };
    gateway_oidc::device_login(issuer, client_id, scopes, *allow_insecure, &state.state_dir)
        .await
        .map(Json)
        .map_err(|error| (StatusCode::BAD_GATEWAY, format!("{error:#}")))
}

pub(crate) fn program_uses_subscription(config: &DaemonConfig, client_id: &str) -> bool {
    match client_id {
        "claude-code" => config
            .programs
            .claude_code
            .as_ref()
            .is_some_and(|program| program.auth == Some(ProgramAuthentication::Subscription)),
        "claude-desktop" => config
            .programs
            .claude_desktop
            .as_ref()
            .is_some_and(|program| program.auth == Some(ProgramAuthentication::Subscription)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use agentdesktop_core::config::parse_daemon;

    use super::{
        ProgramControl, load_effective_config, program_uses_subscription, unmetered_tools,
    };

    #[tokio::test]
    async fn credential_endpoint_reuses_controller_tokens_only_for_the_same_identity_and_policy() {
        use agentdesktop_core::model::{DaemonInfo, DaemonScope, Discovery, LlmGatewayCredential};
        use axum::{
            Extension,
            extract::{Query, State},
        };
        use std::{
            sync::Arc,
            time::{Duration, SystemTime, UNIX_EPOCH},
        };
        let root = tempfile::tempdir().unwrap();
        let config = parse_daemon("llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: controllerJwt\n    audience: test\n    allowedClientIds: [pi, codex]\n").unwrap();
        let identity = |id: &str| {
            serde_json::json!({
                "deviceId": id, "clientCertificatePem": "test", "clientCertificateExpiresAtUnixSeconds": 0,
                "oauthTokenEndpoint": "test", "oauthClientId": "test"
            })
        };
        fs::write(
            root.path().join("identity.json"),
            identity("device-1").to_string(),
        )
        .unwrap();
        let (_, discovery) = tokio::sync::watch::channel(Arc::new(Discovery {
            agents: vec![],
            model_runtimes: vec![],
        }));
        let mut state = super::AppState {
            config,
            daemon_info: DaemonInfo {
                version: "test".into(),
                scope: DaemonScope::User,
                config_path: String::new(),
                state_directory: String::new(),
                inventory_interval: Duration::from_secs(60),
                controller: None,
                llm_proxy: None,
                usage_receiver: None,
                containment: None,
            },
            discovery,
            enrollment: crate::enrollment::EnrollmentState::new(false),
            controller_status: None,
            state_dir: root.path().to_owned(),
            oidc_callback_listen: None,
            telemetry: None,
            logout: None,
            usage: None,
            programs: None,
            violation_ledger: None,
        };
        let cache = Arc::new(crate::llm_proxy::CredentialCache::default());
        let expiry = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600;
        cache
            .get_credential(
                "pi\0https://gateway.example.com/\0test",
                "device-1",
                || async {
                    Ok::<_, ()>(LlmGatewayCredential {
                        credential: "cached-token".into(),
                        expires_at_unix_seconds: expiry,
                    })
                },
            )
            .await
            .unwrap();
        for _ in 0..2 {
            let credential = super::llm_gateway_credential(
                State(state.clone()),
                Extension(cache.clone()),
                Query(super::CredentialQuery {
                    client_id: "pi".into(),
                }),
            )
            .await
            .unwrap()
            .0;
            assert_eq!(credential.credential, "cached-token");
            assert_eq!(credential.expires_at_unix_seconds, expiry);
        }
        // Policy changes must not reuse a cached token for the old policy.
        if let Some(agentdesktop_core::config::LlmGatewayAuthentication::ControllerJwt {
            audience,
            ..
        }) = &mut state.config.llm_gateway.as_mut().unwrap().authentication
        {
            *audience = "changed".into();
        }
        assert!(
            super::llm_gateway_credential(
                State(state.clone()),
                Extension(cache.clone()),
                Query(super::CredentialQuery {
                    client_id: "pi".into()
                })
            )
            .await
            .is_err()
        );
        if let Some(agentdesktop_core::config::LlmGatewayAuthentication::ControllerJwt {
            audience,
            allowed_client_ids,
        }) = &mut state.config.llm_gateway.as_mut().unwrap().authentication
        {
            *audience = "test".into();
            allowed_client_ids.remove("pi");
        }
        assert!(
            super::llm_gateway_credential(
                State(state.clone()),
                Extension(cache.clone()),
                Query(super::CredentialQuery {
                    client_id: "pi".into()
                })
            )
            .await
            .is_err()
        );
        if let Some(agentdesktop_core::config::LlmGatewayAuthentication::ControllerJwt {
            allowed_client_ids,
            ..
        }) = &mut state.config.llm_gateway.as_mut().unwrap().authentication
        {
            allowed_client_ids.insert("pi".into());
        }
        // No controller is configured, so any miss fails instead of contacting a server.
        assert!(
            super::llm_gateway_credential(
                State(state.clone()),
                Extension(cache.clone()),
                Query(super::CredentialQuery {
                    client_id: "codex".into()
                })
            )
            .await
            .is_err()
        );
        state.config.llm_gateway.as_mut().unwrap().url =
            "https://other.example.com".parse().unwrap();
        assert!(
            super::llm_gateway_credential(
                State(state.clone()),
                Extension(cache.clone()),
                Query(super::CredentialQuery {
                    client_id: "pi".into()
                })
            )
            .await
            .is_err()
        );
        state.config.llm_gateway.as_mut().unwrap().url =
            "https://gateway.example.com".parse().unwrap();
        fs::write(
            root.path().join("identity.json"),
            identity("device-2").to_string(),
        )
        .unwrap();
        assert!(
            super::llm_gateway_credential(
                State(state.clone()),
                Extension(cache.clone()),
                Query(super::CredentialQuery {
                    client_id: "pi".into()
                })
            )
            .await
            .is_err()
        );
        fs::remove_file(root.path().join("identity.json")).unwrap();
        assert!(
            super::llm_gateway_credential(
                State(state),
                Extension(cache),
                Query(super::CredentialQuery {
                    client_id: "pi".into()
                })
            )
            .await
            .is_err()
        );
    }

    #[test]
    fn vscode_on_github_models_is_reported_as_unmetered() {
        let github_models = parse_daemon(
            "llmGateway:\n  url: https://gateway.example\n  proxyUrl: https://gateway.example/copilot-proxy\nprograms:\n  vscode:\n    copilotChat: githubModels\n",
        )
        .unwrap();
        assert_eq!(unmetered_tools(&github_models), vec!["vscode-copilot"]);

        let own_models = parse_daemon(
            "llmGateway:\n  url: https://gateway.example\nprograms:\n  vscode:\n    models:\n      gpt-4.1-mini: {}\n",
        )
        .unwrap();
        assert!(unmetered_tools(&own_models).is_empty());
    }

    #[test]
    fn subscription_is_selected_by_requesting_agent() {
        let config = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: https://login.example.com
    clientId: agentdesktop
programs:
  claudeCode:
    auth: subscription
  claudeDesktop: {}
"#,
        )
        .unwrap();
        assert!(program_uses_subscription(&config, "claude-code"));
        assert!(!program_uses_subscription(&config, "claude-desktop"));
        assert!(!program_uses_subscription(&config, "codex"));
    }

    #[test]
    fn controller_configuration_is_the_effective_gateway_configuration() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-api-effective-config-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&root).unwrap();
        let local = parse_daemon(
            r#"
controller:
  address: https://controller.example.com
"#,
        )
        .unwrap();
        fs::write(
            root.join("remote-config.yaml"),
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-code]
"#,
        )
        .unwrap();

        let effective = load_effective_config(&local, &root).unwrap();

        assert_eq!(
            effective.llm_gateway.unwrap().url.as_str(),
            "https://gateway.example.com/"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn standalone_configuration_ignores_stale_remote_configuration() {
        let root = std::env::temp_dir().join(format!(
            "agentdesktop-api-standalone-config-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&root).unwrap();
        let local = parse_daemon(
            r#"
llmGateway:
  url: http://127.0.0.1:4001
  authentication:
    type: oidc
    issuer: http://127.0.0.1:5557/dex
    clientId: agentdesktop-local
    allowInsecure: true
"#,
        )
        .unwrap();
        fs::write(
            root.join("remote-config.yaml"),
            r#"
llmGateway:
  url: https://stale.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-code]
"#,
        )
        .unwrap();

        let effective = load_effective_config(&local, &root).unwrap();

        let gateway = effective.llm_gateway.unwrap();
        assert_eq!(gateway.url.as_str(), "http://127.0.0.1:4001/");
        assert!(matches!(
            gateway.authentication,
            Some(agentdesktop_core::config::LlmGatewayAuthentication::Oidc { .. })
        ));
        fs::remove_dir_all(root).unwrap();
    }

    // PR 6 (AC-1, N1): applied-config wiring for the `containment` block.

    fn temp_state_dir(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "agentdesktop-api-containment-{name}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ))
    }

    #[test]
    fn a_controller_delivered_containment_block_applies_over_a_local_file_without_one() {
        let root = temp_state_dir("remote-wraps");
        fs::create_dir_all(&root).unwrap();
        let local = parse_daemon(
            r#"
controller:
  address: https://controller.example.com
"#,
        )
        .unwrap();
        fs::write(
            root.join("remote-config.yaml"),
            r#"
containment:
  programs:
    claudeCode:
      executable: 'C:\Program Files\Anthropic\ClaudeCode\claude.exe'
"#,
        )
        .unwrap();

        let effective = load_effective_config(&local, &root).unwrap();

        let claude = effective
            .containment
            .expect("the controller-delivered block applies")
            .programs
            .claude_code
            .expect("claudeCode present");
        assert_eq!(
            claude.mode,
            agentdesktop_core::config::ContainmentMode::Wrap
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_local_containment_block_is_ignored_once_a_controller_is_configured() {
        let root = temp_state_dir("local-ignored");
        fs::create_dir_all(&root).unwrap();
        // N1: a controller-configured daemon's local `containment` block is
        // never consulted; `load_effective_config` reads only the
        // controller-delivered `remote-config.yaml`.
        let local = parse_daemon(
            r#"
controller:
  address: https://controller.example.com
containment:
  programs:
    claudeCode:
      executable: 'C:\should\not\apply\claude.exe'
"#,
        );
        // The local file itself is accepted (containment is a valid
        // top-level field); what matters is that it never reaches `effective`
        // once a controller is configured and a remote config is applied.
        let local = local.unwrap_or_else(|error| {
            panic!("a local containment block parses before any controller applies: {error:#}")
        });
        fs::write(
            root.join("remote-config.yaml"),
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-code]
"#,
        )
        .unwrap();

        let effective = load_effective_config(&local, &root).unwrap();

        assert!(
            effective.containment.is_none(),
            "the local containment block is ignored once a controller applies a remote config"
        );
        fs::remove_dir_all(root).unwrap();
    }

    /// Adversarial m4: with a controller configured, the local `containment`
    /// block must not apply BEFORE the first `remote-config.yaml` exists
    /// either; the other local fields keep backing the fallback as before.
    #[test]
    fn a_local_containment_block_does_not_apply_before_the_first_remote_config() {
        let root = temp_state_dir("local-ignored-before-first-push");
        fs::create_dir_all(&root).unwrap();
        let local = parse_daemon(
            r#"
controller:
  address: https://controller.example.com
llmGateway:
  url: http://127.0.0.1:4001
  authentication:
    type: oidc
    issuer: https://login.example.com
    clientId: local-dev
containment:
  programs:
    claudeCode:
      executable: 'C:\should\not\apply\claude.exe'
"#,
        )
        .unwrap();
        // No remote-config.yaml in `root`.

        let effective = load_effective_config(&local, &root).unwrap();

        assert!(
            effective.containment.is_none(),
            "no containment applies before the first controller push"
        );
        assert!(
            effective.llm_gateway.is_some(),
            "the pre-existing fallback for the other local fields is unchanged"
        );
        fs::remove_dir_all(root).unwrap();
    }

    mod programs {
        use std::{path::Path, sync::Arc};

        use agentdesktop_core::{config::parse_daemon, model::ProgramChoice};
        use axum::http::StatusCode;
        use tokio::sync::watch;

        use super::ProgramControl;
        use crate::{
            reconcile::{ProgramState, Reconciler},
            tick::{CurrentConfig, TickStatus},
        };

        const ALLOWED: &str = r#"
programSelection: { allowUserChoice: true }
llmGateway:
  url: https://gateway.example.com
programs:
  claudeCode: {}
  codex: {}
"#;

        fn reconciler(root: &Path) -> Reconciler {
            Reconciler::new(
                true,
                root.join("claude/settings.json"),
                root.join("claude-desktop/settings.json"),
                root.join("claude-desktop/helper"),
                root.join("codex/config.toml"),
                root.join("opencode/config.json"),
                root.join("opencode/plugin.js"),
                root.join("grok/managed_config.toml"),
                None,
                None,
                None,
                Some(root.join("pi/models.json")),
                Some(root.join("pi/settings.json")),
                root.join("bin/agentdesktop"),
                root.join("agentdesktop.sock"),
            )
        }

        /// A control over `yaml` as the current configuration (none when
        /// `None`), already applied, and the receiver of its reports.
        fn control(
            root: &Path,
            yaml: Option<&str>,
        ) -> (ProgramControl, watch::Receiver<Option<TickStatus>>) {
            let reconciler = reconciler(root);
            let current = yaml.map(|yaml| CurrentConfig {
                revision: Some(7),
                config: Arc::new(parse_daemon(yaml).unwrap()),
            });
            if let Some(current) = &current {
                reconciler.apply(&current.config).unwrap();
            }
            let (_current, current) = watch::channel(current);
            let (statuses, reports) = watch::channel(None);
            (
                ProgramControl::new(reconciler, current, Some(statuses), root.join("state")),
                reports,
            )
        }

        fn only(program: &str) -> ProgramChoice {
            ProgramChoice::Selected {
                programs: [program.to_owned()].into(),
            }
        }

        #[test]
        fn a_choice_is_stored_applied_and_reported() {
            let root = tempfile::tempdir().unwrap();
            let (control, reports) = control(root.path(), Some(ALLOWED));

            let management = control.change(only("claude-code")).unwrap();
            assert!(management.user_choice_allowed);
            assert_eq!(management.choice, only("claude-code"));
            let codex = management
                .programs
                .iter()
                .find(|program| program.program == "codex")
                .unwrap();
            assert!(!codex.managed);
            assert_eq!(codex.state.as_deref(), Some("opted_out"));
            assert!(!root.path().join("codex/config.toml").exists());
            assert_eq!(
                crate::program_choice::load(&root.path().join("state")),
                only("claude-code")
            );

            let report = reports.borrow().clone().expect("the re-apply is reported");
            assert_eq!(report.revision, Some(7));
            assert!(report.error.is_none());
            assert!(report.report.programs.iter().any(|outcome| {
                outcome.program == "codex" && outcome.state == ProgramState::OptedOut
            }));
        }

        #[test]
        fn a_choice_is_refused_unless_the_policy_allows_it() {
            let root = tempfile::tempdir().unwrap();
            let (locked, _) = control(
                root.path(),
                Some(&ALLOWED.replace("allowUserChoice: true", "")),
            );
            let (status, _) = locked.change(ProgramChoice::None).unwrap_err();
            assert_eq!(status, StatusCode::FORBIDDEN);
            assert!(!root.path().join("state/program-choice.json").exists());
            assert!(root.path().join("codex/config.toml").exists());
            assert!(
                locked
                    .management()
                    .programs
                    .iter()
                    .all(|program| program.managed)
            );

            let (unconfigured, _) = control(root.path(), None);
            let (status, _) = unconfigured.change(ProgramChoice::None).unwrap_err();
            assert_eq!(status, StatusCode::CONFLICT);
            assert!(unconfigured.management().programs.is_empty());
        }

        #[test]
        fn a_choice_naming_an_unmanaged_program_is_a_bad_request() {
            let root = tempfile::tempdir().unwrap();
            let (control, _) = control(root.path(), Some(ALLOWED));
            let (status, message) = control.change(only("cursor")).unwrap_err();
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(message.contains("cursor"), "{message}");
        }
    }

    #[test]
    fn non_interactive_forces_device_authorization_for_oidc() {
        use agentdesktop_core::config::LlmGatewayAuthentication;

        let original = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: https://login.example.com
    clientId: agentdesktop
    deviceAuthorization: false
"#,
        )
        .unwrap();

        let transformed = super::non_interactive(original.clone());

        // Original has deviceAuthorization: false
        if let Some(LlmGatewayAuthentication::Oidc {
            device_authorization,
            ..
        }) = &original.llm_gateway.as_ref().unwrap().authentication
        {
            assert!(!device_authorization);
        } else {
            panic!("expected OIDC authentication");
        }

        // Transformed has deviceAuthorization: true
        if let Some(LlmGatewayAuthentication::Oidc {
            device_authorization,
            ..
        }) = &transformed.llm_gateway.as_ref().unwrap().authentication
        {
            assert!(device_authorization);
        } else {
            panic!("expected OIDC authentication");
        }
    }

    #[test]
    fn non_interactive_leaves_controller_jwt_unchanged() {
        let original = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
  authentication:
    type: controllerJwt
    audience: agentgateway
    allowedClientIds: [claude-code]
"#,
        )
        .unwrap();

        let transformed = super::non_interactive(original.clone());

        // ControllerJwt should be unchanged
        assert!(matches!(
            transformed.llm_gateway.as_ref().unwrap().authentication,
            Some(agentdesktop_core::config::LlmGatewayAuthentication::ControllerJwt { .. })
        ));
    }

    #[test]
    fn non_interactive_leaves_no_gateway_unchanged() {
        let original = parse_daemon("{}").unwrap();
        let transformed = super::non_interactive(original.clone());
        assert!(transformed.llm_gateway.is_none());
    }
}
