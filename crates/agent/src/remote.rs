use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{Mutex, mpsc, oneshot},
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
        Discovery as AgentDiscovery, TelemetryEvent as ModelTelemetryEvent, TelemetryEventKind,
    },
};
use agentdesktop_proto::fleet::{
    AgentMessage, ConfigState, ConfigStatus, Discovery, Heartbeat, Hello,
    InferenceGatewayCredentialRequest, Inventory, RenewDeviceCertificateRequest, SessionNewEvent,
    TelemetryEvent, ToolUseEvent, agent_message, controller_message,
    fleet_agent_client::FleetAgentClient, telemetry_event,
};

use crate::{
    enrollment::EnrollmentState,
    identity::{self, Identity},
    oidc,
    reconcile::Reconciler,
    secure_fs,
};

static OAUTH_REFRESH_MUTEX: Mutex<()> = Mutex::const_new(());

pub struct LogoutRequest {
    pub completion: oneshot::Sender<Result<(), String>>,
}

pub struct Requests {
    pub telemetry: mpsc::Receiver<ModelTelemetryEvent>,
    pub logout: mpsc::Receiver<LogoutRequest>,
}

pub async fn run(
    controller: ControllerConnectionConfig,
    discovered: AgentDiscovery,
    state_dir: PathBuf,
    oidc_callback_listen: Option<SocketAddr>,
    reconciler: Reconciler,
    enrollment: EnrollmentState,
    requests: Requests,
) -> anyhow::Result<()> {
    let Requests {
        mut telemetry,
        mut logout,
    } = requests;
    let identity_path = state_dir.join("identity.json");
    loop {
        let mut identity = match identity::load(&identity_path)? {
            Some(identity) => {
                enrollment.set("enrolled").await;
                identity
            }
            None => {
                let identity = tokio::select! {
                    result = oidc::enroll(&controller, &enrollment, oidc_callback_listen) => result?,
                    Some(request) = logout.recv() => {
                        enrollment.set("starting").await;
                        let _ = request.completion.send(Ok(()));
                        continue;
                    }
                };
                identity::save(&identity_path, &identity)?;
                enrollment.set("enrolled").await;
                info!(device_id = %identity.device_id, "enrolled device");
                identity
            }
        };

        let mut delay = Duration::from_secs(1);
        loop {
            let refresh_result = tokio::select! {
                result = refresh_oauth_if_needed(&mut identity, &identity_path) => result,
                Some(request) = logout.recv() => {
                    if complete_logout(request, &identity_path, &identity, &enrollment).await {
                        break;
                    }
                    continue;
                }
            };
            refresh_result?;
            if certificate_needs_renewal(&identity) {
                match renew_device_certificate(&controller, &identity).await {
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
                    &controller,
                    &identity,
                    &discovered,
                    &state_dir,
                    &reconciler,
                    &mut telemetry,
                ) => Some(result),
                Some(request) = logout.recv() => {
                    if complete_logout(request, &identity_path, &identity, &enrollment).await {
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
                Ok(()) => warn!("controller stream closed"),
                Err(error) if is_unauthenticated(&error) => {
                    let error_chain = format!("{error:#}");
                    identity::delete(&identity_path, &identity.device_id)?;
                    enrollment.set("starting").await;
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
                    warn!(
                        controller = %controller.address,
                        retry_in_seconds = delay.as_secs(),
                        error = %error_chain,
                        "controller connection failed"
                    );
                }
            }

            let logged_out = tokio::select! {
                _ = time::sleep(delay) => false,
                Some(request) = logout.recv() => {
                    complete_logout(request, &identity_path, &identity, &enrollment).await
                }
            };
            if logged_out {
                break;
            }
            delay = (delay * 2).min(Duration::from_secs(60));
        }
    }
}

async fn complete_logout(
    request: LogoutRequest,
    identity_path: &Path,
    identity: &Identity,
    enrollment: &EnrollmentState,
) -> bool {
    let result = identity::delete(identity_path, &identity.device_id)
        .map_err(|error| format!("remove local organization identity: {error:#}"));
    if result.is_ok() {
        enrollment.set("starting").await;
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

pub async fn inference_gateway_credential(
    controller: &ControllerConnectionConfig,
    state_dir: &Path,
    client_id: &str,
) -> anyhow::Result<agentdesktop_core::model::InferenceGatewayCredential> {
    let mut identity =
        identity::load(&state_dir.join("identity.json"))?.context("device is not enrolled")?;
    let identity_path = state_dir.join("identity.json");
    refresh_oauth_if_needed(&mut identity, &identity_path).await?;
    let mut client = client(controller, Some(&identity)).await?;
    let mut request = Request::new(InferenceGatewayCredentialRequest {
        client_id: client_id.to_owned(),
    });
    authenticate_request(&identity, &mut request)?;
    let response = client
        .get_inference_gateway_credential(request)
        .await
        .context("request inference gateway credential")?
        .into_inner();
    Ok(agentdesktop_core::model::InferenceGatewayCredential {
        credential: response.credential,
        expires_at_unix_seconds: response.expires_at_unix_seconds,
    })
}

async fn connect(
    controller: &ControllerConnectionConfig,
    identity: &Identity,
    discovered: &AgentDiscovery,
    state_dir: &Path,
    reconciler: &Reconciler,
    telemetry: &mut mpsc::Receiver<ModelTelemetryEvent>,
) -> anyhow::Result<()> {
    let mut client = client(controller, Some(identity)).await?;
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
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
        }),
    )
    .await?;

    send(
        &sender,
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
        }),
    )
    .await?;
    info!(
        discoveries = discovered.agents.len(),
        "reported inventory to controller"
    );

    info!(address = %controller.address, "connected to controller");
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
            }
            Some(event) = telemetry.recv() => {
                send(&sender, agent_message::Message::Telemetry(telemetry_to_proto(event))).await?;
            }
            message = inbound.message() => {
                let Some(message) = message.context("read controller stream")? else {
                    return Ok(());
                };
                if let Some(controller_message::Message::DaemonConfig(config)) = message.message {
                    info!(
                        revision = config.revision,
                        bytes = config.yaml.len(),
                        "received daemon configuration"
                    );
                    let status = apply_daemon_config(state_dir, config, reconciler);
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

fn apply_daemon_config(
    state_dir: &Path,
    config: agentdesktop_proto::fleet::DaemonConfig,
    reconciler: &Reconciler,
) -> ConfigStatus {
    let result = (|| -> anyhow::Result<()> {
        let actual_hash = Sha256::digest(&config.yaml);
        if actual_hash.as_slice() != config.sha256 {
            bail!("configuration hash does not match payload");
        }
        debug!(
            revision = config.revision,
            "verified daemon configuration hash"
        );

        let yaml = std::str::from_utf8(&config.yaml).context("configuration is not UTF-8")?;
        let daemon_config = config::parse_daemon(yaml)?;
        debug!(revision = config.revision, "parsed daemon configuration");
        reconciler.apply(&daemon_config)?;
        secure_fs::ensure_private_dir(state_dir)?;
        let path = state_dir.join("remote-config.yaml");
        secure_fs::atomic_write(&path, &config.yaml, 0o600)?;
        info!(
            revision = config.revision,
            path = %path.display(),
            "persisted daemon configuration"
        );
        Ok(())
    })();

    match result {
        Ok(()) => ConfigStatus {
            revision: config.revision,
            state: ConfigState::Applied.into(),
            error: String::new(),
        },
        Err(error) => ConfigStatus {
            revision: config.revision,
            state: ConfigState::Failed.into(),
            error: format!("{error:#}"),
        },
    }
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
        if let Some(stored) = identity::load(identity_path)?
            && stored.oauth.expires_at_unix_seconds > unix_time_seconds().saturating_add(120)
        {
            *identity = stored;
            return Ok(());
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
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|value| value.trim().to_string()))
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::is_unauthenticated;

    #[test]
    fn recognizes_contextualized_unauthenticated_status() {
        let error = anyhow::Error::new(tonic::Status::unauthenticated("rejected"))
            .context("open controller stream");
        assert!(is_unauthenticated(&error));
    }
}
