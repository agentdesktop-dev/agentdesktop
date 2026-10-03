use std::{
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Mutex, RwLock, mpsc, oneshot, watch},
    time,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Request,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity as TlsIdentity},
};
use tracing::{debug, info, warn};

use agentdesktop_core::{
    config::{self, ControllerConnectionConfig},
    model::{
        ControllerConnectionError, ControllerConnectionStatus, Discovery as AgentDiscovery,
        TelemetryEvent as ModelTelemetryEvent, TelemetryEventKind,
    },
};
use agentdesktop_proto::fleet::{
    AgentMessage, ConfigState, ConfigStatus, Discovery, Heartbeat, Hello, Inventory,
    LlmGatewayCredentialRequest, ProgramState as ProtoProgramState, ProgramStatus,
    RenewDeviceCertificateRequest, SessionNewEvent, TelemetryEvent, ToolUseEvent, agent_message,
    controller_message, fleet_agent_client::FleetAgentClient, telemetry_event,
};

use crate::{
    enrollment::EnrollmentState,
    identity::{self, Identity},
    oidc,
    reconcile::Reconciler,
    secure_fs,
    tick::{CurrentConfig, TickStatus},
};

static OAUTH_REFRESH_MUTEX: Mutex<()> = Mutex::const_new(());
const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
/// Bound on the controller connection and call when fetching an LLM gateway
/// credential, so a controller that accepts and then stalls cannot hold the
/// caller (or a detached fetch) open indefinitely.
const CREDENTIAL_CALL_TIMEOUT: Duration = Duration::from_secs(15);

pub struct LogoutRequest {
    pub completion: oneshot::Sender<Result<(), String>>,
}

pub struct Requests {
    pub telemetry: mpsc::Receiver<ModelTelemetryEvent>,
    pub logout: mpsc::Receiver<LogoutRequest>,
    /// The daemon's current configuration, replaced by each pushed
    /// configuration that parses (read by the reconcile tick).
    pub(crate) current: watch::Sender<Option<CurrentConfig>>,
    /// Reconcile tick outcomes, when the tick is enabled.
    pub(crate) tick_statuses: Option<watch::Receiver<Option<TickStatus>>>,
}

/// The configuration channels shared with the reconcile tick.
struct ConfigChannels {
    current: watch::Sender<Option<CurrentConfig>>,
    tick_statuses: Option<watch::Receiver<Option<TickStatus>>>,
}

/// Tracks whether the daemon's connection to the controller is currently
/// live. This is the only source of truth for controller connectivity: it is
/// updated exclusively by the controller stream-management code (this
/// module and its caller in `daemon.rs`) and read by the local API
/// (`/v1/health`) so the desktop UI can distinguish "the daemon process is
/// healthy" from "the daemon is actually talking to the controller right
/// now" — a daemon can hold valid enrollment credentials and answer local
/// requests fine for an arbitrarily long time while its controller stream is
/// stuck retrying (auth rejection, network partition, etc.), and neither the
/// process itself nor its cached enrollment state ever reflects that on
/// their own.
#[derive(Clone)]
pub struct ControllerConnectionState {
    status: Arc<RwLock<ControllerConnectionStatus>>,
}

impl ControllerConnectionState {
    pub fn new() -> Self {
        Self {
            status: Arc::new(RwLock::new(Self::initial())),
        }
    }

    fn initial() -> ControllerConnectionStatus {
        ControllerConnectionStatus {
            connected: false,
            last_seen_unix_seconds: None,
            last_error: None,
        }
    }

    pub async fn get(&self) -> ControllerConnectionStatus {
        self.status.read().await.clone()
    }

    pub(crate) async fn mark_connected(&self) {
        let mut status = self.status.write().await;
        status.connected = true;
        status.last_seen_unix_seconds = Some(unix_time_seconds());
        status.last_error = None;
    }

    /// Records that the open stream is still alive (heartbeat sent or
    /// controller message received) without changing the connection state.
    pub(crate) async fn mark_seen(&self) {
        let mut status = self.status.write().await;
        if status.connected {
            status.last_seen_unix_seconds = Some(unix_time_seconds());
        }
    }

    /// Records a closed stream. A live stream that closes was, by definition,
    /// seen until now, so its last-seen time advances to the close.
    pub(crate) async fn mark_disconnected(&self, error: Option<ControllerConnectionError>) {
        let mut status = self.status.write().await;
        if status.connected {
            status.last_seen_unix_seconds = Some(unix_time_seconds());
        }
        status.connected = false;
        if error.is_some() {
            status.last_error = error;
        }
    }

    /// Forgets everything about the previous organization session, so a
    /// signed-out device does not keep reporting the old controller state.
    pub(crate) async fn reset(&self) {
        *self.status.write().await = Self::initial();
    }
}

impl Default for ControllerConnectionState {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    controller: ControllerConnectionConfig,
    mut discovered: watch::Receiver<Arc<AgentDiscovery>>,
    state_dir: PathBuf,
    oidc_callback_listen: Option<SocketAddr>,
    reconciler: Reconciler,
    enrollment: EnrollmentState,
    controller_status: ControllerConnectionState,
    requests: Requests,
) {
    let Requests {
        mut telemetry,
        mut logout,
        current,
        tick_statuses,
    } = requests;
    let mut channels = ConfigChannels {
        current,
        tick_statuses,
    };
    let mut delay = INITIAL_RETRY_DELAY;
    loop {
        let started = time::Instant::now();
        let Err(error) = run_session(
            &controller,
            &mut discovered,
            &state_dir,
            oidc_callback_listen,
            &reconciler,
            &enrollment,
            &controller_status,
            &mut telemetry,
            &mut logout,
            &mut channels,
        )
        .await;
        // A session that stayed up for a while failed for a new reason; do not
        // penalize it with the backoff accumulated by earlier failures.
        if started.elapsed() > MAX_RETRY_DELAY {
            delay = INITIAL_RETRY_DELAY;
        }
        enrollment.set("failed").await;
        controller_status
            .mark_disconnected(Some(ControllerConnectionError::LocalError))
            .await;
        tracing::error!(
            controller = %controller.address,
            retry_in_seconds = delay.as_secs(),
            error = %format!("{error:#}"),
            "controller integration failed; restarting"
        );
        tokio::select! {
            _ = time::sleep(delay) => delay = next_retry_delay(delay),
            Some(request) = logout.recv() => {
                logout_between_sessions(
                    request,
                    &state_dir.join("identity.json"),
                    &enrollment,
                    &controller_status,
                    &channels.current,
                )
                .await;
                delay = INITIAL_RETRY_DELAY;
            }
        }
    }
}

async fn logout_between_sessions(
    request: LogoutRequest,
    identity_path: &Path,
    enrollment: &EnrollmentState,
    controller_status: &ControllerConnectionState,
    current: &watch::Sender<Option<CurrentConfig>>,
) {
    match identity::load(identity_path) {
        Ok(Some(identity)) => {
            complete_logout(
                request,
                identity_path,
                &identity,
                enrollment,
                controller_status,
                current,
            )
            .await;
        }
        Ok(None) => {
            controller_status.reset().await;
            complete_unenrolled_logout(request, enrollment).await;
        }
        // An identity that cannot be read cannot be used either, so signing out
        // only has to remove it. Refusing would leave the device stuck with it.
        Err(error) if identity::is_unreadable(&error) => {
            warn!(
                identity_path = %identity_path.display(),
                error = %format!("{error:#}"),
                "removing unreadable device identity on sign-out"
            );
            match identity::discard(identity_path) {
                Ok(()) => {
                    controller_status.reset().await;
                    complete_unenrolled_logout(request, enrollment).await;
                }
                Err(error) => {
                    let _ = request.completion.send(Err(format!(
                        "remove local organization identity: {error:#}"
                    )));
                }
            }
        }
        Err(error) => {
            let _ = request
                .completion
                .send(Err(format!("read local organization identity: {error:#}")));
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_session(
    controller: &ControllerConnectionConfig,
    discovered: &mut watch::Receiver<Arc<AgentDiscovery>>,
    state_dir: &Path,
    oidc_callback_listen: Option<SocketAddr>,
    reconciler: &Reconciler,
    enrollment: &EnrollmentState,
    controller_status: &ControllerConnectionState,
    telemetry: &mut mpsc::Receiver<ModelTelemetryEvent>,
    logout: &mut mpsc::Receiver<LogoutRequest>,
    channels: &mut ConfigChannels,
) -> anyhow::Result<Infallible> {
    let identity_path = state_dir.join("identity.json");
    loop {
        let stored = match identity::load(&identity_path) {
            Ok(stored) => stored,
            Err(error) if identity::is_unreadable(&error) => {
                warn!(
                    identity_path = %identity_path.display(),
                    error = %format!("{error:#}"),
                    "stored device identity is unreadable, for example after the app's code signature changed; removing it and restarting enrollment"
                );
                identity::discard(&identity_path)?;
                enrollment.set("starting").await;
                None
            }
            Err(error) => return Err(error),
        };
        let mut identity = match stored {
            Some(identity) => {
                enrollment.set("enrolled").await;
                identity
            }
            None => {
                let identity = enroll_with_retry(
                    &controller.address,
                    enrollment,
                    logout,
                    INITIAL_RETRY_DELAY,
                    || oidc::enroll(controller, enrollment, oidc_callback_listen),
                )
                .await;
                identity::save(&identity_path, &identity)?;
                enrollment.set("enrolled").await;
                info!(device_id = %identity.device_id, "enrolled device");
                identity
            }
        };

        let mut delay = INITIAL_RETRY_DELAY;
        loop {
            let refresh_result = tokio::select! {
                result = refresh_oauth_if_needed(&mut identity, &identity_path) => result,
                Some(request) = logout.recv() => {
                    if complete_logout(request, &identity_path, &identity, enrollment, controller_status, &channels.current).await {
                        break;
                    }
                    continue;
                }
            };
            match refresh_result {
                Ok(()) => {}
                Err(error) if is_oauth_refresh_rejected(&error) => {
                    let error_chain = format!("{error:#}");
                    identity::delete(&identity_path, &identity.device_id)?;
                    enrollment.set("starting").await;
                    controller_status
                        .mark_disconnected(Some(ControllerConnectionError::SessionExpired))
                        .await;
                    warn!(
                        controller = %controller.address,
                        identity_path = %identity_path.display(),
                        error = %error_chain,
                        "OIDC refresh token was rejected; removed local identity and restarting enrollment"
                    );
                    break;
                }
                Err(error) if is_transient_refresh_failure(&error) => {
                    // Network failures and token endpoint outages are transient: the
                    // stored refresh token is still valid, so keep the identity and retry
                    // instead of tearing down the controller integration.
                    warn!(
                        controller = %controller.address,
                        retry_in_seconds = delay.as_secs(),
                        error = %format!("{error:#}"),
                        "OIDC access token refresh failed; retaining current identity and retrying"
                    );
                    if wait_before_retry(
                        delay,
                        logout,
                        &identity_path,
                        &identity,
                        enrollment,
                        controller_status,
                        &channels.current,
                    )
                    .await
                    {
                        break;
                    }
                    delay = next_retry_delay(delay);
                    continue;
                }
                // Local identity store failures and unsupported token types are not
                // fixed by waiting; let the supervisor report them as failed.
                Err(error) => return Err(error),
            }
            if certificate_needs_renewal(&identity) {
                match renew_device_certificate(controller, &identity).await {
                    Ok(renewed) => {
                        identity::save(&identity_path, &renewed)?;
                        identity = renewed;
                        info!(device_id = %identity.device_id, "renewed device certificate");
                    }
                    Err(error) => {
                        warn!(error = %format!("{error:#}"), "device certificate renewal failed; retaining current certificate")
                    }
                }
            }
            identity::save(&identity_path, &identity)?;
            let connection = tokio::select! {
                result = connect(
                    controller,
                    &identity,
                    discovered,
                    state_dir,
                    reconciler,
                    telemetry,
                    controller_status,
                    channels,
                ) => Some(result),
                Some(request) = logout.recv() => {
                    if complete_logout(request, &identity_path, &identity, enrollment, controller_status, &channels.current).await {
                        None
                    } else {
                        continue;
                    }
                }
            };
            let Some(connection) = connection else {
                break;
            };
            match connection {
                Ok(()) => {
                    controller_status.mark_disconnected(None).await;
                    warn!("controller stream closed");
                }
                Err(error) if is_unauthenticated(&error) => {
                    let error_chain = format!("{error:#}");
                    identity::delete(&identity_path, &identity.device_id)?;
                    enrollment.set("starting").await;
                    controller_status
                        .mark_disconnected(Some(ControllerConnectionError::IdentityRejected))
                        .await;
                    warn!(
                        controller = %controller.address,
                        identity_path = %identity_path.display(),
                        error = %error_chain,
                        "controller rejected the device identity; removed local identity and restarting enrollment"
                    );
                    break;
                }
                Err(error) => {
                    let error_chain = format!("{error:#}");
                    controller_status
                        .mark_disconnected(Some(ControllerConnectionError::Unreachable))
                        .await;
                    warn!(
                        controller = %controller.address,
                        retry_in_seconds = delay.as_secs(),
                        error = %error_chain,
                        "controller connection failed"
                    );
                }
            }

            if wait_before_retry(
                delay,
                logout,
                &identity_path,
                &identity,
                enrollment,
                controller_status,
                &channels.current,
            )
            .await
            {
                break;
            }
            delay = next_retry_delay(delay);
        }
    }
}

/// Sleeps for `delay`, completing any logout request that arrives meanwhile.
/// Returns `true` when the local session was logged out.
async fn wait_before_retry(
    delay: Duration,
    logout: &mut mpsc::Receiver<LogoutRequest>,
    identity_path: &Path,
    identity: &Identity,
    enrollment: &EnrollmentState,
    controller_status: &ControllerConnectionState,
    current: &watch::Sender<Option<CurrentConfig>>,
) -> bool {
    tokio::select! {
        _ = time::sleep(delay) => false,
        Some(request) = logout.recv() => {
            complete_logout(request, identity_path, identity, enrollment, controller_status, current).await
        }
    }
}

async fn enroll_with_retry<F, Fut, T>(
    controller: &str,
    enrollment: &EnrollmentState,
    logout: &mut mpsc::Receiver<LogoutRequest>,
    mut delay: Duration,
    mut enroll: F,
) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = anyhow::Result<T>>,
{
    loop {
        let result = tokio::select! {
            result = enroll() => result,
            Some(request) = logout.recv() => {
                complete_unenrolled_logout(request, enrollment).await;
                continue;
            }
        };
        match result {
            Ok(identity) => return identity,
            Err(error) => {
                enrollment.set("starting").await;
                warn!(
                    controller,
                    retry_in_seconds = delay.as_secs(),
                    error = %format!("{error:#}"),
                    "controller enrollment failed"
                );
            }
        }

        tokio::select! {
            _ = time::sleep(delay) => {}
            Some(request) = logout.recv() => {
                complete_unenrolled_logout(request, enrollment).await;
            }
        }
        delay = next_retry_delay(delay);
    }
}

async fn complete_unenrolled_logout(request: LogoutRequest, enrollment: &EnrollmentState) {
    enrollment.set("starting").await;
    let _ = request.completion.send(Ok(()));
}

fn next_retry_delay(delay: Duration) -> Duration {
    (delay * 2).min(MAX_RETRY_DELAY)
}

async fn complete_logout(
    request: LogoutRequest,
    identity_path: &Path,
    identity: &Identity,
    enrollment: &EnrollmentState,
    controller_status: &ControllerConnectionState,
    current: &watch::Sender<Option<CurrentConfig>>,
) -> bool {
    let result = identity::delete(identity_path, &identity.device_id)
        .map_err(|error| format!("remove local organization identity: {error:#}"));
    if result.is_ok() {
        // The organization's configuration is no longer enforced by the
        // reconcile tick; the managed files stay until the next configuration.
        current.send_replace(None);
        enrollment.set("starting").await;
        controller_status.reset().await;
        info!(device_id = %identity.device_id, "logged out local organization session");
    }
    let logged_out = result.is_ok();
    let _ = request.completion.send(result);
    logged_out
}

fn is_unauthenticated(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<tonic::Status>()
        .is_some_and(|status| status.code() == tonic::Code::Unauthenticated)
}

fn is_oauth_refresh_rejected(error: &anyhow::Error) -> bool {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .and_then(reqwest::Error::status)
        .is_some_and(|status| {
            matches!(
                status,
                reqwest::StatusCode::BAD_REQUEST
                    | reqwest::StatusCode::UNAUTHORIZED
                    | reqwest::StatusCode::FORBIDDEN
            )
        })
}

/// Refresh failures that came from talking to the token endpoint (connection
/// errors, timeouts, non-rejecting HTTP statuses) and may succeed on retry.
fn is_transient_refresh_failure(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<reqwest::Error>().is_some())
}

pub async fn llm_gateway_credential(
    controller: &ControllerConnectionConfig,
    state_dir: &Path,
    client_id: &str,
) -> anyhow::Result<agentdesktop_core::model::LlmGatewayCredential> {
    let mut identity =
        identity::load(&state_dir.join("identity.json"))?.context("device is not enrolled")?;
    let identity_path = state_dir.join("identity.json");
    refresh_oauth_if_needed(&mut identity, &identity_path).await?;
    // The refresh above is not cut off (its result is saved); the controller
    // connection and call are bounded together.
    let call = async {
        let mut client = client(controller, Some(&identity)).await?;
        let mut request = Request::new(LlmGatewayCredentialRequest {
            client_id: client_id.to_owned(),
        });
        authenticate_request(&identity, &mut request)?;
        client
            .get_llm_gateway_credential(request)
            .await
            .context("request LLM gateway credential")
    };
    let response = tokio::time::timeout(CREDENTIAL_CALL_TIMEOUT, call)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "controller at {} did not answer within {}s",
                controller.address,
                CREDENTIAL_CALL_TIMEOUT.as_secs()
            )
        })??
        .into_inner();
    Ok(agentdesktop_core::model::LlmGatewayCredential {
        credential: response.credential,
        expires_at_unix_seconds: response.expires_at_unix_seconds,
    })
}

#[allow(clippy::too_many_arguments)]
async fn connect(
    controller: &ControllerConnectionConfig,
    identity: &Identity,
    discovered: &mut watch::Receiver<Arc<AgentDiscovery>>,
    state_dir: &Path,
    reconciler: &Reconciler,
    telemetry: &mut mpsc::Receiver<ModelTelemetryEvent>,
    controller_status: &ControllerConnectionState,
    channels: &mut ConfigChannels,
) -> anyhow::Result<()> {
    let mut client = client(controller, Some(identity)).await?;
    // Tick outcomes produced before this stream are not sent on it; the
    // controller's configuration push on connect reports anyway.
    if let Some(statuses) = channels.tick_statuses.as_mut() {
        statuses.borrow_and_update();
    }
    // The revision of the last configuration received on this stream and the
    // last status sent on it, for deciding whether a tick outcome is news.
    let mut stream_revision = None;
    let mut last_sent: Option<ConfigStatus> = None;
    let mut pending_persist: Option<(u64, Vec<u8>)> = None;
    let (sender, receiver) = mpsc::channel(16);
    let mut request = Request::new(ReceiverStream::new(receiver));
    authenticate_request(identity, &mut request)?;

    let mut inbound = client
        .connect(request)
        .await
        .context("open controller stream")?
        .into_inner();

    send(
        &sender,
        agent_message::Message::Hello(Hello {
            device_id: identity.device_id.clone(),
            hostname: hostname(),
            os: std::env::consts::OS.to_string(),
            architecture: std::env::consts::ARCH.to_string(),
            agent_version: agentdesktop_core::VERSION.to_string(),
        }),
    )
    .await?;

    let snapshot = discovered.borrow_and_update().clone();
    send_inventory(&sender, &snapshot).await?;

    info!(address = %controller.address, "connected to controller");
    controller_status.mark_connected().await;
    let mut heartbeat = time::interval(controller.heartbeat_interval);
    let reconnect_at = identity.oauth.expires_at_unix_seconds.saturating_sub(60);
    let oauth_reconnect = time::sleep(Duration::from_secs(
        reconnect_at.saturating_sub(unix_time_seconds()),
    ));
    tokio::pin!(oauth_reconnect);
    loop {
        tokio::select! {
            _ = &mut oauth_reconnect => {
                info!("reconnecting controller stream to refresh OIDC access token");
                return Ok(());
            }
            _ = heartbeat.tick() => {
                send(&sender, agent_message::Message::Heartbeat(Heartbeat {
                    unix_time_seconds: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                })).await?;
                controller_status.mark_seen().await;
            }
            Some(event) = telemetry.recv() => {
                send(&sender, agent_message::Message::Telemetry(telemetry_to_proto(event))).await?;
            }
            snapshot = next_inventory(discovered) => {
                send_inventory(&sender, &snapshot).await?;
            }
            tick = next_tick_status(&mut channels.tick_statuses) => {
                let mut tick = tick;
                if tick.error.is_none()
                    && tick.revision.is_some()
                    && tick.revision == stream_revision
                    && let Some((revision, yaml)) = pending_persist.take()
                {
                    // A failed save stays visible: the tick is reported as
                    // failed and the save is retried on the next tick.
                    if let Err(error) = persist_config(state_dir, &yaml, revision) {
                        warn!(revision, error = %format!("{error:#}"), "saving the configuration after a reconcile tick failed; retrying on the next tick");
                        tick.error = Some(format!("{error:#}"));
                        pending_persist = Some((revision, yaml));
                    }
                }
                if let Some(status) = tick_config_status(last_sent.as_ref(), stream_revision, &tick) {
                    info!(revision = status.revision, "reporting a changed reconcile outcome");
                    last_sent = Some(status.clone());
                    send(&sender, agent_message::Message::ConfigStatus(status)).await?;
                }
            }
            message = inbound.message() => {
                let Some(message) = message.context("read controller stream")? else {
                    return Ok(());
                };
                controller_status.mark_seen().await;
                if let Some(controller_message::Message::DaemonConfig(config)) = message.message {
                    info!(
                        revision = config.revision,
                        bytes = config.yaml.len(),
                        "received daemon configuration"
                    );
                    stream_revision = Some(config.revision);
                    let parsed = replace_current_config(&channels.current, &config);
                    let parsed_ok = parsed.is_ok();
                    let status = apply_parsed(state_dir, &config, parsed, reconciler);
                    // A parsed configuration whose apply failed is the current
                    // one but not yet saved; a later tick that applies it
                    // saves it, so a restart does not go back to the previous one.
                    pending_persist = (parsed_ok && !status.error.is_empty())
                        .then(|| (config.revision, config.yaml.clone()));
                    last_sent = Some(status.clone());
                    if status.error.is_empty() {
                        info!(revision = status.revision, "applied daemon configuration");
                    } else {
                        warn!(
                            revision = status.revision,
                            error = %status.error,
                            "failed daemon configuration"
                        );
                    }
                    send(&sender, agent_message::Message::ConfigStatus(status)).await?;
                }
            }
        }
    }
}

fn telemetry_to_proto(event: ModelTelemetryEvent) -> TelemetryEvent {
    let timestamp_unix_ms = event.timestamp_unix_ms;
    let event = match event.event {
        TelemetryEventKind::SessionNew {
            client_id,
            session_id,
        } => telemetry_event::Event::SessionNew(SessionNewEvent {
            client_id,
            session_id,
        }),
        TelemetryEventKind::ToolUse {
            client_id,
            tool_name,
            tool_use_id,
            tool_input,
        } => telemetry_event::Event::ToolUse(ToolUseEvent {
            client_id,
            tool_name,
            tool_use_id: tool_use_id.unwrap_or_default(),
            input_json: tool_input
                .map(|input| serde_json::to_vec(&input).expect("tool input is JSON-compatible"))
                .unwrap_or_default(),
        }),
    };
    TelemetryEvent {
        timestamp_unix_ms,
        event: Some(event),
    }
}

/// Parses and applies a pushed configuration in one step (the connection
/// loop parses first to update the current configuration).
#[cfg(test)]
fn apply_daemon_config(
    state_dir: &Path,
    config: agentdesktop_proto::fleet::DaemonConfig,
    reconciler: &Reconciler,
) -> ConfigStatus {
    let parsed = parse_pushed_config(&config);
    apply_parsed(state_dir, &config, parsed, reconciler)
}

/// Applies a pushed configuration already checked by [`parse_pushed_config`]
/// and persists it once its apply succeeded.
fn apply_parsed(
    state_dir: &Path,
    config: &agentdesktop_proto::fleet::DaemonConfig,
    parsed: anyhow::Result<agentdesktop_core::config::DaemonConfig>,
    reconciler: &Reconciler,
) -> ConfigStatus {
    let mut programs = Vec::new();
    let result = (|| -> anyhow::Result<()> {
        let daemon_config = parsed?;
        debug!(revision = config.revision, "parsed daemon configuration");
        let (report, applied) = reconciler.apply_with_report(&daemon_config);
        report.log();
        programs = program_statuses(&report);
        applied?;
        persist_config(state_dir, &config.yaml, config.revision)
    })();
    // `programs_reported` says this agent reports per-program status; the
    // list is empty when the configuration was not applied at all (hash or
    // parse error), and holds the apply's outcomes otherwise, also when a
    // later step (persisting the configuration) failed.
    config_status(
        config.revision,
        programs,
        result.map_err(|error| format!("{error:#}")),
    )
}

/// Saves the controller's configuration as the one to restore at startup.
fn persist_config(state_dir: &Path, yaml: &[u8], revision: u64) -> anyhow::Result<()> {
    secure_fs::ensure_private_dir(state_dir)?;
    let path = state_dir.join("remote-config.yaml");
    secure_fs::atomic_write(&path, yaml, 0o600)?;
    info!(revision, path = %path.display(), "persisted daemon configuration");
    Ok(())
}

fn program_statuses(report: &crate::reconcile::ApplyReport) -> Vec<ProgramStatus> {
    report
        .programs
        .iter()
        .map(|outcome| ProgramStatus {
            program: outcome.program.to_owned(),
            state: program_state_proto(outcome.state).into(),
            detail: outcome.detail.clone(),
        })
        .collect()
}

fn config_status(
    revision: u64,
    programs: Vec<ProgramStatus>,
    result: Result<(), String>,
) -> ConfigStatus {
    let (state, error) = match result {
        Ok(()) => (ConfigState::Applied, String::new()),
        Err(error) => (ConfigState::Failed, error),
    };
    ConfigStatus {
        revision,
        state: state.into(),
        error,
        programs,
        programs_reported: true,
    }
}

/// The proto value of a program's outcome.
fn program_state_proto(state: crate::reconcile::ProgramState) -> ProtoProgramState {
    use crate::reconcile::ProgramState;
    match state {
        ProgramState::Applied => ProtoProgramState::Applied,
        ProgramState::Unchanged => ProtoProgramState::Unchanged,
        ProgramState::Removed => ProtoProgramState::Removed,
        ProgramState::Conflict => ProtoProgramState::Conflict,
        ProgramState::Inactive => ProtoProgramState::Inactive,
        ProgramState::Blocked => ProtoProgramState::Blocked,
        ProgramState::Failed => ProtoProgramState::Failed,
    }
}

/// The `ConfigStatus` a tick's outcome should send on this stream, if any.
/// A status is sent only when the tick's revision is `Some` and
/// equals the revision of the last `DaemonConfig` received on this stream,
/// and the device state, the error, or any program's `(program, state,
/// detail)` differs from `last_sent` (the last `ConfigStatus` sent on this
/// stream, whether by a push or an earlier tick), where a program going from
/// `applied` or `removed` to `unchanged` is not a difference. A failed tick
/// is always reported (when its revision matches) as `FAILED` with the
/// error and `programs_reported: true`.
pub(crate) fn tick_config_status(
    last_sent: Option<&ConfigStatus>,
    stream_revision: Option<u64>,
    tick: &TickStatus,
) -> Option<ConfigStatus> {
    if tick.revision.is_none() || tick.revision != stream_revision {
        return None;
    }
    let status = config_status(
        tick.revision.unwrap_or_default(),
        program_statuses(&tick.report),
        tick.error.clone().map_or(Ok(()), Err),
    );
    let Some(last_sent) = last_sent else {
        return Some(status);
    };
    let differs = last_sent.state != status.state
        || last_sent.error != status.error
        || crate::reconcile::outcomes_differ(
            &status_keys(&last_sent.programs),
            &status_keys(&status.programs),
        );
    differs.then_some(status)
}

/// `(program, state, detail)` of each reported program, for
/// [`crate::reconcile::outcomes_differ`].
fn status_keys(programs: &[ProgramStatus]) -> Vec<(&str, crate::reconcile::ProgramState, &str)> {
    use crate::reconcile::ProgramState;
    programs
        .iter()
        .map(|program| {
            let state = match ProtoProgramState::try_from(program.state) {
                Ok(ProtoProgramState::Applied) => ProgramState::Applied,
                Ok(ProtoProgramState::Unchanged) => ProgramState::Unchanged,
                Ok(ProtoProgramState::Removed) => ProgramState::Removed,
                Ok(ProtoProgramState::Conflict) => ProgramState::Conflict,
                Ok(ProtoProgramState::Inactive) => ProgramState::Inactive,
                Ok(ProtoProgramState::Blocked) => ProgramState::Blocked,
                Ok(ProtoProgramState::Failed | ProtoProgramState::Unspecified) | Err(_) => {
                    ProgramState::Failed
                }
            };
            (program.program.as_str(), state, program.detail.as_str())
        })
        .collect()
}

/// Sends the current inventory snapshot to the controller.
///
/// The controller replaces a device's stored inventory on each message, so
/// resending a refreshed snapshot is safe and is how post-boot changes reach
/// the fleet view.
async fn send_inventory(
    sender: &mpsc::Sender<AgentMessage>,
    discovered: &AgentDiscovery,
) -> anyhow::Result<()> {
    send(
        sender,
        agent_message::Message::Inventory(Inventory {
            discoveries: discovered
                .agents
                .iter()
                .map(|agent| Discovery {
                    kind: agent.kind.clone(),
                    version: agent.version.clone().unwrap_or_default(),
                    path: agent.executable.display().to_string(),
                    mcp_servers: agent
                        .mcp_servers
                        .iter()
                        .map(|server| agentdesktop_proto::fleet::McpServer {
                            name: server.name.clone(),
                            transport: server.transport.clone(),
                            command: server.command.clone().unwrap_or_default(),
                            url: server.url.clone().unwrap_or_default(),
                            enabled: server.enabled,
                            source: server.source.display().to_string(),
                        })
                        .collect(),
                    skills: agent
                        .skills
                        .iter()
                        .map(|skill| agentdesktop_proto::fleet::Skill {
                            path: skill.path.display().to_string(),
                            front_matter_json: serde_json::to_vec(&skill.front_matter)
                                .expect("skill front matter is JSON-compatible"),
                        })
                        .collect(),
                })
                .collect(),
            model_runtimes: discovered
                .model_runtimes
                .iter()
                .map(|runtime| agentdesktop_proto::fleet::ModelRuntime {
                    kind: runtime.kind.clone(),
                    models: runtime
                        .models
                        .iter()
                        .map(|model| agentdesktop_proto::fleet::LocalModel {
                            name: model.name.clone(),
                        })
                        .collect(),
                })
                .collect(),
        }),
    )
    .await?;
    info!(
        discoveries = discovered.agents.len(),
        model_runtimes = discovered.model_runtimes.len(),
        "reported inventory to controller"
    );
    Ok(())
}

/// The next reconcile tick outcome; never resolves when the tick is off or
/// has stopped.
async fn next_tick_status(
    statuses: &mut Option<watch::Receiver<Option<TickStatus>>>,
) -> TickStatus {
    if let Some(statuses) = statuses.as_mut() {
        while statuses.changed().await.is_ok() {
            if let Some(tick) = statuses.borrow_and_update().clone() {
                return tick;
            }
        }
    }
    std::future::pending().await
}

/// Makes a pushed configuration the current one for the reconcile tick if it
/// parses, whether or not its apply then succeeds; a push that does not
/// parse leaves the current configuration as it is.
fn replace_current_config(
    current: &watch::Sender<Option<CurrentConfig>>,
    config: &agentdesktop_proto::fleet::DaemonConfig,
) -> anyhow::Result<agentdesktop_core::config::DaemonConfig> {
    let parsed = parse_pushed_config(config)?;
    current.send_replace(Some(CurrentConfig {
        revision: Some(config.revision),
        config: Arc::new(parsed.clone()),
    }));
    Ok(parsed)
}

/// Hash, UTF-8 and parse checks of a pushed configuration.
fn parse_pushed_config(
    config: &agentdesktop_proto::fleet::DaemonConfig,
) -> anyhow::Result<agentdesktop_core::config::DaemonConfig> {
    let actual_hash = Sha256::digest(&config.yaml);
    if actual_hash.as_slice() != config.sha256 {
        bail!("configuration hash does not match payload");
    }
    debug!(
        revision = config.revision,
        "verified daemon configuration hash"
    );
    let yaml = std::str::from_utf8(&config.yaml).context("configuration is not UTF-8")?;
    config::parse_daemon(yaml)
}

/// Resolves with the next inventory snapshot, and never resolves once the
/// refresher has stopped, so the controller stream keeps running without it.
async fn next_inventory(
    discovered: &mut watch::Receiver<Arc<AgentDiscovery>>,
) -> Arc<AgentDiscovery> {
    if discovered.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
    discovered.borrow_and_update().clone()
}

async fn send(
    sender: &mpsc::Sender<AgentMessage>,
    message: agent_message::Message,
) -> anyhow::Result<()> {
    sender
        .send(AgentMessage {
            message: Some(message),
        })
        .await
        .context("controller stream closed")
}

pub(crate) async fn client(
    controller: &ControllerConnectionConfig,
    identity: Option<&Identity>,
) -> anyhow::Result<FleetAgentClient<Channel>> {
    let mut endpoint = Endpoint::from_shared(controller.address.clone())
        .with_context(|| format!("parse controller address {}", controller.address))?;
    let mut tls_config = ClientTlsConfig::new();
    let mut custom_tls = false;
    if let Some(path) = &controller.ca_certificate_path {
        let pem = std::fs::read(path)
            .with_context(|| format!("read controller CA certificate from {}", path.display()))?;
        tls_config = tls_config.ca_certificate(Certificate::from_pem(pem));
        custom_tls = true;
    }
    if let Some(identity) = identity {
        tls_config = tls_config.identity(TlsIdentity::from_pem(
            &identity.client_certificate_pem,
            &identity.client_private_key_pem,
        ));
        custom_tls = true;
    }
    if custom_tls {
        endpoint = endpoint.tls_config(tls_config)?;
    }
    let channel = endpoint
        .connect()
        .await
        .with_context(|| format!("connect to controller at {}", controller.address))?;
    Ok(FleetAgentClient::new(channel))
}

fn authenticate_request<T>(identity: &Identity, request: &mut Request<T>) -> anyhow::Result<()> {
    let access_token = &identity.oauth.access_token;
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {access_token}")
            .parse()
            .context("encode OIDC access token")?,
    );
    Ok(())
}

async fn renew_device_certificate(
    controller: &ControllerConnectionConfig,
    identity: &Identity,
) -> anyhow::Result<Identity> {
    let key_pem = &identity.client_private_key_pem;
    let key = KeyPair::from_pem(key_pem).context("parse device TLS private key")?;
    let mut params = CertificateParams::new(Vec::<String>::new())?;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let csr = params
        .serialize_request(&key)
        .context("create replacement device certificate signing request")?;
    let mut client = client(controller, Some(identity)).await?;
    let mut request = Request::new(RenewDeviceCertificateRequest {
        certificate_signing_request_der: csr.der().as_ref().to_vec(),
    });
    authenticate_request(identity, &mut request)?;
    let response = client
        .renew_device_certificate(request)
        .await
        .context("renew device certificate")?
        .into_inner();
    let certificate = String::from_utf8(response.client_certificate_pem)
        .context("controller returned a non-UTF-8 device certificate")?;
    if certificate.is_empty() {
        bail!("controller returned an empty device certificate");
    }
    Ok(Identity {
        device_id: identity.device_id.clone(),
        client_certificate_pem: certificate,
        client_private_key_pem: key_pem.to_owned(),
        client_certificate_expires_at_unix_seconds: response.expires_at_unix_seconds,
        oauth: identity.oauth.clone(),
        oauth_token_endpoint: identity.oauth_token_endpoint.clone(),
        oauth_client_id: identity.oauth_client_id.clone(),
    })
}

async fn refresh_oauth_if_needed(
    identity: &mut Identity,
    identity_path: &Path,
) -> anyhow::Result<()> {
    let oauth = &identity.oauth;
    if oauth.expires_at_unix_seconds <= unix_time_seconds().saturating_add(120) {
        let _guard = OAUTH_REFRESH_MUTEX.lock().await;
        match identity::load(identity_path) {
            Ok(Some(stored))
                if stored.oauth.expires_at_unix_seconds
                    > unix_time_seconds().saturating_add(120) =>
            {
                *identity = stored;
                return Ok(());
            }
            Ok(_) => {}
            // The identity in memory still works. Refresh it, and the save below
            // replaces the stored copy that can no longer be read.
            Err(error) if identity::is_unreadable(&error) => {
                warn!(
                    device_id = %identity.device_id,
                    error = %format!("{error:#}"),
                    "stored device identity is unreadable; refreshing from the in-memory copy"
                );
            }
            Err(error) => return Err(error),
        }
        oidc::refresh(identity).await?;
        identity::save(identity_path, identity).context("persist rotated OIDC refresh token")?;
        info!(device_id = %identity.device_id, "refreshed OIDC access token");
    }
    Ok(())
}

fn certificate_needs_renewal(identity: &Identity) -> bool {
    identity.client_certificate_expires_at_unix_seconds
        <= unix_time_seconds().saturating_add(24 * 60 * 60)
}

fn unix_time_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn hostname() -> String {
    hostname::get()
        .ok()
        .and_then(normalize_hostname)
        .or_else(|| std::env::var_os("HOSTNAME").and_then(normalize_hostname))
        .or_else(|| std::env::var_os("COMPUTERNAME").and_then(normalize_hostname))
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .and_then(normalize_hostname)
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn normalize_hostname(value: impl AsRef<std::ffi::OsStr>) -> Option<String> {
    let value = value.as_ref().to_string_lossy();
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tokio::sync::mpsc;

    use super::{
        ControllerConnectionState, MAX_RETRY_DELAY, enroll_with_retry, is_oauth_refresh_rejected,
        is_transient_refresh_failure, is_unauthenticated, next_inventory, next_retry_delay,
        normalize_hostname,
    };
    use crate::enrollment::EnrollmentState;
    use agentdesktop_core::model::{ControllerConnectionError, Discovery as AgentDiscovery};
    use tokio::{sync::watch, time};

    fn empty_inventory() -> AgentDiscovery {
        AgentDiscovery {
            agents: Vec::new(),
            model_runtimes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn reports_each_refreshed_inventory_snapshot() {
        let (sender, mut receiver) = watch::channel(Arc::new(empty_inventory()));
        let mut refreshed = empty_inventory();
        refreshed
            .model_runtimes
            .push(agentdesktop_core::model::ModelRuntime {
                kind: "ollama".to_owned(),
                models: Vec::new(),
            });
        sender.send(Arc::new(refreshed)).unwrap();

        let snapshot = next_inventory(&mut receiver).await;
        assert_eq!(snapshot.model_runtimes.len(), 1);
    }

    /// A stopped refresher must not turn the controller stream into a busy loop.
    #[tokio::test(start_paused = true)]
    async fn never_reports_again_once_the_refresher_stops() {
        let (sender, mut receiver) = watch::channel(Arc::new(empty_inventory()));
        drop(sender);
        assert!(
            time::timeout(Duration::from_secs(3600), next_inventory(&mut receiver))
                .await
                .is_err()
        );
    }

    #[test]
    fn recognizes_contextualized_unauthenticated_status() {
        let error = anyhow::Error::new(tonic::Status::unauthenticated("rejected"))
            .context("open controller stream");
        assert!(is_unauthenticated(&error));
    }

    #[test]
    fn distinguishes_rejected_refresh_tokens_from_transient_errors() {
        fn status_error(status: reqwest::StatusCode) -> anyhow::Error {
            reqwest::Response::from(
                axum::http::Response::builder()
                    .status(status)
                    .body("")
                    .unwrap(),
            )
            .error_for_status()
            .unwrap_err()
            .into()
        }

        assert!(is_oauth_refresh_rejected(&status_error(
            reqwest::StatusCode::BAD_REQUEST
        )));
        assert!(is_oauth_refresh_rejected(&status_error(
            reqwest::StatusCode::UNAUTHORIZED
        )));
        assert!(!is_oauth_refresh_rejected(&status_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        )));
        assert!(!is_oauth_refresh_rejected(&anyhow::anyhow!(
            "network unavailable"
        )));
        assert!(is_transient_refresh_failure(
            &status_error(reqwest::StatusCode::SERVICE_UNAVAILABLE)
                .context("OIDC token endpoint rejected refresh token")
        ));
    }

    /// Identity store failures are not retried in place; they must reach the
    /// supervisor so enrollment is reported as failed.
    #[test]
    fn local_identity_failures_are_not_transient() {
        let error = anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            .context("persist rotated OIDC refresh token");
        assert!(!is_transient_refresh_failure(&error));
        assert!(!is_transient_refresh_failure(&anyhow::anyhow!(
            "OIDC token endpoint returned unsupported token type"
        )));
    }

    #[tokio::test]
    async fn network_failures_during_refresh_are_transient() {
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let closed_address = closed.local_addr().unwrap();
        drop(closed);
        let connect_error: anyhow::Error = reqwest::Client::new()
            .post(format!("http://{closed_address}/token"))
            .send()
            .await
            .unwrap_err()
            .into();
        let connect_error = connect_error.context("refresh OIDC access token");
        assert!(!is_oauth_refresh_rejected(&connect_error));
        assert!(is_transient_refresh_failure(&connect_error));

        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let silent_address = silent.local_addr().unwrap();
        let _accepted = tokio::spawn(async move {
            let _connection = silent.accept().await;
            std::future::pending::<()>().await;
        });
        let timeout_error: anyhow::Error = reqwest::Client::builder()
            .timeout(Duration::from_millis(50))
            .build()
            .unwrap()
            .post(format!("http://{silent_address}/token"))
            .send()
            .await
            .unwrap_err()
            .into();
        let timeout_error = timeout_error.context("refresh OIDC access token");
        assert!(!is_oauth_refresh_rejected(&timeout_error));
        assert!(is_transient_refresh_failure(&timeout_error));
    }

    #[test]
    fn retry_delay_doubles_until_capped() {
        assert_eq!(
            next_retry_delay(Duration::from_secs(1)),
            Duration::from_secs(2)
        );
        assert_eq!(next_retry_delay(MAX_RETRY_DELAY), MAX_RETRY_DELAY);
    }

    #[test]
    fn normalizes_native_hostname() {
        assert_eq!(
            normalize_hostname(" postaguest1\n"),
            Some("postaguest1".to_owned())
        );
        assert_eq!(normalize_hostname(" \n\t"), None);
    }

    #[tokio::test]
    async fn failed_enrollment_is_retried() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let enrollment = EnrollmentState::new(true);
        let (_logout_sender, mut logout) = mpsc::channel(1);

        let identity = enroll_with_retry(
            "https://controller.example",
            &enrollment,
            &mut logout,
            Duration::ZERO,
            || {
                let attempts = attempts.clone();
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err(anyhow::anyhow!("controller unavailable"))
                    } else {
                        Ok("identity")
                    }
                }
            },
        )
        .await;

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert_eq!(identity, "identity");
    }

    #[tokio::test]
    async fn controller_connection_state_starts_disconnected() {
        let status = ControllerConnectionState::new().get().await;
        assert!(!status.connected);
        assert_eq!(status.last_seen_unix_seconds, None);
        assert_eq!(status.last_error, None);
    }

    #[tokio::test]
    async fn controller_connection_state_reports_a_successful_connection() {
        let state = ControllerConnectionState::new();
        state.mark_connected().await;

        let status = state.get().await;
        assert!(status.connected);
        assert!(status.last_seen_unix_seconds.is_some());
        assert_eq!(status.last_error, None);
    }

    /// Backdates the last-seen time so tests can observe it advancing without
    /// sleeping across a one-second boundary.
    async fn backdate_last_seen(state: &ControllerConnectionState) {
        state.status.write().await.last_seen_unix_seconds = Some(1);
    }

    #[tokio::test]
    async fn controller_connection_state_mark_seen_advances_only_while_connected() {
        let state = ControllerConnectionState::new();
        state.mark_seen().await;
        assert_eq!(state.get().await.last_seen_unix_seconds, None);

        state.mark_connected().await;
        backdate_last_seen(&state).await;
        state.mark_seen().await;
        assert!(state.get().await.last_seen_unix_seconds > Some(1));

        state.mark_disconnected(None).await;
        backdate_last_seen(&state).await;
        state.mark_seen().await;
        assert_eq!(state.get().await.last_seen_unix_seconds, Some(1));
    }

    #[tokio::test]
    async fn controller_connection_state_disconnect_records_when_the_stream_was_last_seen() {
        let state = ControllerConnectionState::new();
        state.mark_connected().await;
        backdate_last_seen(&state).await;

        state
            .mark_disconnected(Some(ControllerConnectionError::Unreachable))
            .await;

        let status = state.get().await;
        assert!(!status.connected);
        assert!(status.last_seen_unix_seconds > Some(1));
        assert_eq!(
            status.last_error,
            Some(ControllerConnectionError::Unreachable)
        );

        // Further failures while already disconnected must not look like
        // fresh contact with the controller.
        backdate_last_seen(&state).await;
        state
            .mark_disconnected(Some(ControllerConnectionError::Unreachable))
            .await;
        assert_eq!(state.get().await.last_seen_unix_seconds, Some(1));
    }

    #[tokio::test]
    async fn controller_connection_state_reconnect_clears_the_previous_error() {
        let state = ControllerConnectionState::new();
        state
            .mark_disconnected(Some(ControllerConnectionError::Unreachable))
            .await;
        state.mark_connected().await;

        let status = state.get().await;
        assert!(status.connected);
        assert_eq!(status.last_error, None);
    }

    #[tokio::test]
    async fn controller_connection_state_disconnect_without_an_error_keeps_the_previous_one() {
        let state = ControllerConnectionState::new();
        state
            .mark_disconnected(Some(ControllerConnectionError::IdentityRejected))
            .await;
        // A clean stream close (e.g. the periodic OIDC-refresh reconnect)
        // reports no error; it should not silently erase the last real one.
        state.mark_disconnected(None).await;

        assert_eq!(
            state.get().await.last_error,
            Some(ControllerConnectionError::IdentityRejected)
        );
    }

    #[tokio::test]
    async fn controller_connection_state_reset_forgets_the_previous_session() {
        let state = ControllerConnectionState::new();
        state.mark_connected().await;
        state
            .mark_disconnected(Some(ControllerConnectionError::SessionExpired))
            .await;

        state.reset().await;

        assert_eq!(
            state.get().await,
            ControllerConnectionState::new().get().await
        );
    }
    // --- Per-program configuration status -----------------------------------

    fn test_reconciler(root: &std::path::Path) -> crate::reconcile::Reconciler {
        crate::reconcile::Reconciler::new(
            true,
            root.join("claude/settings.json"),
            root.join("claude-desktop/settings.json"),
            root.join("claude-desktop/helper"),
            root.join("codex/config.toml"),
            root.join("opencode/config.json"),
            root.join("opencode/plugin.js"),
            root.join("grok/managed_config.toml"),
            Some(root.join("copilot/providers.json")),
            None,
            None,
            root.join("bin/agentdesktop"),
            root.join("agentdesktop.sock"),
        )
    }

    #[test]
    fn hash_error_reports_empty_programs_and_programs_reported_true() {
        let state_dir = std::env::temp_dir().join(format!(
            "agentdesktop-remote-hash-error-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let reconciler = test_reconciler(&state_dir);
        let config = agentdesktop_proto::fleet::DaemonConfig {
            revision: 7,
            yaml: b"programs: {}\n".to_vec(),
            sha256: vec![0u8; 32],
        };

        let status = super::apply_daemon_config(&state_dir, config, &reconciler);

        assert_eq!(
            status.state,
            agentdesktop_proto::fleet::ConfigState::Failed as i32
        );
        assert!(status.error.contains("hash"));
        assert!(status.programs.is_empty());
        assert!(status.programs_reported);
        let _ = std::fs::remove_dir_all(&state_dir);
    }

    #[test]
    fn mapping_every_program_state_to_the_proto_enum() {
        use crate::reconcile::ProgramState;
        use agentdesktop_proto::fleet::ProgramState as ProtoProgramState;

        let cases = [
            (ProgramState::Applied, ProtoProgramState::Applied),
            (ProgramState::Unchanged, ProtoProgramState::Unchanged),
            (ProgramState::Removed, ProtoProgramState::Removed),
            (ProgramState::Conflict, ProtoProgramState::Conflict),
            (ProgramState::Inactive, ProtoProgramState::Inactive),
            (ProgramState::Blocked, ProtoProgramState::Blocked),
            (ProgramState::Failed, ProtoProgramState::Failed),
        ];
        for (state, expected) in cases {
            assert_eq!(super::program_state_proto(state), expected, "{state:?}");
        }
    }

    // --- Tick reporting dedup --------------------------------------------------

    fn program_report(
        state: crate::reconcile::ProgramState,
        detail: &str,
    ) -> crate::reconcile::ApplyReport {
        crate::reconcile::ApplyReport {
            programs: vec![crate::reconcile::ProgramOutcome {
                program: "claude-code",
                state,
                detail: detail.to_owned(),
            }],
        }
    }

    fn sent_status(report: &crate::reconcile::ApplyReport, revision: u64) -> super::ConfigStatus {
        use super::{ConfigState, ConfigStatus, ProgramStatus};
        ConfigStatus {
            revision,
            state: ConfigState::Applied.into(),
            error: String::new(),
            programs: report
                .programs
                .iter()
                .map(|outcome| ProgramStatus {
                    program: outcome.program.to_owned(),
                    state: super::program_state_proto(outcome.state).into(),
                    detail: outcome.detail.clone(),
                })
                .collect(),
            programs_reported: true,
        }
    }

    fn tick_status(
        revision: Option<u64>,
        report: crate::reconcile::ApplyReport,
        error: Option<&str>,
    ) -> crate::tick::TickStatus {
        use crate::tick::TickStatus;
        TickStatus {
            revision,
            report,
            error: error.map(str::to_owned),
        }
    }

    #[test]
    fn tick_config_status_is_none_when_the_revision_is_none_or_mismatched() {
        use crate::reconcile::ProgramState;
        let previous = program_report(ProgramState::Unchanged, "");
        let last_sent = sent_status(&previous, 3);
        let current = program_report(ProgramState::Failed, "boom");

        assert!(
            super::tick_config_status(
                Some(&last_sent),
                Some(3),
                &tick_status(None, current.clone(), None)
            )
            .is_none(),
            "a tick with no revision is never reported"
        );
        assert!(
            super::tick_config_status(
                Some(&last_sent),
                Some(3),
                &tick_status(Some(2), current, None)
            )
            .is_none(),
            "a tick whose revision does not match the stream's last push is never reported"
        );
    }

    #[test]
    fn tick_config_status_is_none_when_nothing_differs() {
        use crate::reconcile::ProgramState;
        let report = program_report(ProgramState::Unchanged, "");
        let last_sent = sent_status(&report, 4);
        assert!(
            super::tick_config_status(
                Some(&last_sent),
                Some(4),
                &tick_status(Some(4), report, None)
            )
            .is_none()
        );
    }

    #[test]
    fn tick_config_status_is_none_when_applied_or_removed_settles_to_unchanged() {
        use crate::reconcile::ProgramState;
        for state in [ProgramState::Applied, ProgramState::Removed] {
            let previous = program_report(state, "");
            let last_sent = sent_status(&previous, 5);
            let current = program_report(ProgramState::Unchanged, "");
            assert!(
                super::tick_config_status(
                    Some(&last_sent),
                    Some(5),
                    &tick_status(Some(5), current, None)
                )
                .is_none(),
                "{state:?} -> Unchanged must not be reported"
            );
        }
    }

    #[test]
    fn tick_config_status_is_some_when_a_programs_detail_changes() {
        use crate::reconcile::ProgramState;
        let previous = program_report(ProgramState::Conflict, "conflict at a");
        let last_sent = sent_status(&previous, 6);
        let current = program_report(ProgramState::Conflict, "conflict at b");

        let status = super::tick_config_status(
            Some(&last_sent),
            Some(6),
            &tick_status(Some(6), current, None),
        )
        .expect("a changed detail must be reported");
        assert_eq!(status.revision, 6);
        assert_eq!(status.state, super::ConfigState::Applied as i32);
        assert!(status.programs_reported);
    }

    #[test]
    fn tick_config_status_is_some_with_no_baseline_on_this_stream() {
        use crate::reconcile::ProgramState;
        let current = program_report(ProgramState::Applied, "");
        let status = super::tick_config_status(None, Some(1), &tick_status(Some(1), current, None))
            .expect(
                "nothing sent yet on this stream must still be reported once the revision matches",
            );
        assert_eq!(status.revision, 1);
    }

    #[test]
    fn tick_config_status_reports_a_failed_tick_as_failed_with_programs_reported() {
        use crate::reconcile::ProgramState;
        let previous = program_report(ProgramState::Unchanged, "");
        let last_sent = sent_status(&previous, 7);
        let current = program_report(ProgramState::Unchanged, "");

        let status = super::tick_config_status(
            Some(&last_sent),
            Some(7),
            &tick_status(Some(7), current, Some("disk full")),
        )
        .expect("a failed tick must be reported when its revision matches");
        assert_eq!(status.revision, 7);
        assert_eq!(status.state, super::ConfigState::Failed as i32);
        assert_eq!(status.error, "disk full");
        assert!(status.programs_reported);
    }

    #[test]
    fn only_a_push_that_parses_replaces_the_current_configuration() {
        use sha2::{Digest, Sha256};
        let pushed = |revision: u64, yaml: &str| agentdesktop_proto::fleet::DaemonConfig {
            revision,
            yaml: yaml.as_bytes().to_vec(),
            sha256: Sha256::digest(yaml.as_bytes()).to_vec(),
        };
        let (current, receiver) = tokio::sync::watch::channel(None);
        let _ = super::replace_current_config(&current, &pushed(1, "programs: {}\n"));
        assert_eq!(receiver.borrow().as_ref().and_then(|c| c.revision), Some(1));
        // Does not parse (unknown key): the current configuration stays.
        let _ = super::replace_current_config(&current, &pushed(2, "notAKey: 1\n"));
        assert_eq!(receiver.borrow().as_ref().and_then(|c| c.revision), Some(1));
        // Wrong hash: stays.
        let mut tampered = pushed(3, "programs: {}\n");
        tampered.sha256 = vec![0; 32];
        let _ = super::replace_current_config(&current, &tampered);
        assert_eq!(receiver.borrow().as_ref().and_then(|c| c.revision), Some(1));
        // Parses (its apply may still fail later): replaced.
        let _ = super::replace_current_config(&current, &pushed(4, "programs:\n  grok: {}\n"));
        assert_eq!(receiver.borrow().as_ref().and_then(|c| c.revision), Some(4));
    }
}
