use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};
use tokio::{sync::mpsc, time};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Request,
    transport::{Certificate, Channel, ClientTlsConfig, Endpoint},
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
    InferenceGatewayCredentialRequest, Inventory, SessionNewEvent, TelemetryEvent, ToolUseEvent,
    agent_message, controller_message, fleet_agent_client::FleetAgentClient, telemetry_event,
};

use crate::{
    enrollment::EnrollmentState,
    identity::{self, Identity},
    oidc,
    reconcile::Reconciler,
    secure_fs,
};

pub async fn run(
    controller: ControllerConnectionConfig,
    discovered: AgentDiscovery,
    state_dir: PathBuf,
    oidc_callback_listen: Option<SocketAddr>,
    reconciler: Reconciler,
    enrollment: EnrollmentState,
    mut telemetry: mpsc::Receiver<ModelTelemetryEvent>,
) -> anyhow::Result<()> {
    let identity_path = state_dir.join("identity.json");
    loop {
        let identity = match identity::load(&identity_path)? {
            Some(identity) => {
                enrollment.set("enrolled").await;
                identity
            }
            None => {
                let identity = oidc::enroll(&controller, &enrollment, oidc_callback_listen).await?;
                identity::save(&identity_path, &identity)?;
                enrollment.set("enrolled").await;
                info!(device_id = %identity.device_id, "enrolled device");
                identity
            }
        };

        let mut delay = Duration::from_secs(1);
        loop {
            match connect(
                &controller,
                &identity,
                &discovered,
                &state_dir,
                &reconciler,
                &mut telemetry,
            )
            .await
            {
                Ok(()) => warn!("controller stream closed"),
                Err(error) if is_unauthenticated(&error) => {
                    let error_chain = format!("{error:#}");
                    invalidate_identity(&identity_path)?;
                    enrollment.set("starting").await;
                    warn!(
                        controller = %controller.address,
                        identity_path = %identity_path.display(),
                        error = %error_chain,
                        "controller rejected the device credential; removed local identity and restarting enrollment"
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

            time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_secs(60));
        }
    }
}

fn is_unauthenticated(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<tonic::Status>()
        .is_some_and(|status| status.code() == tonic::Code::Unauthenticated)
}

fn invalidate_identity(path: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("remove rejected identity {}", path.display()))
        }
    }
}

pub async fn inference_gateway_credential(
    controller: &ControllerConnectionConfig,
    state_dir: &Path,
    client_id: &str,
) -> anyhow::Result<agentdesktop_core::model::InferenceGatewayCredential> {
    let identity =
        identity::load(&state_dir.join("identity.json"))?.context("device is not enrolled")?;
    let mut client = client(controller).await?;
    let mut request = Request::new(InferenceGatewayCredentialRequest {
        client_id: client_id.to_owned(),
    });
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", identity.credential)
            .parse()
            .context("encode device credential")?,
    );
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
    let mut client = client(controller).await?;
    let (sender, receiver) = mpsc::channel(16);
    let mut request = Request::new(ReceiverStream::new(receiver));
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", identity.credential)
            .parse()
            .context("encode device credential")?,
    );

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
    loop {
        tokio::select! {
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
) -> anyhow::Result<FleetAgentClient<Channel>> {
    let mut endpoint = Endpoint::from_shared(controller.address.clone())
        .with_context(|| format!("parse controller address {}", controller.address))?;
    if let Some(path) = &controller.ca_certificate_path {
        let pem = std::fs::read(path)
            .with_context(|| format!("read controller CA certificate from {}", path.display()))?;
        endpoint = endpoint
            .tls_config(ClientTlsConfig::new().ca_certificate(Certificate::from_pem(pem)))?;
    }
    let channel = endpoint
        .connect()
        .await
        .with_context(|| format!("connect to controller at {}", controller.address))?;
    Ok(FleetAgentClient::new(channel))
}

pub(crate) fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|value| value.trim().to_string()))
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::{invalidate_identity, is_unauthenticated};

    #[test]
    fn recognizes_contextualized_unauthenticated_status() {
        let error = anyhow::Error::new(tonic::Status::unauthenticated("rejected"))
            .context("open controller stream");
        assert!(is_unauthenticated(&error));
    }

    #[test]
    fn removes_rejected_identity_and_accepts_an_absent_file() {
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-rejected-identity-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, b"identity").expect("write identity");

        invalidate_identity(&path).expect("remove identity");
        assert!(!path.exists());
        invalidate_identity(&path).expect("already absent identity");
    }
}
