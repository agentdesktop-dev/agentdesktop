use std::{
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

use agentplane_core::{
    config::{self, ControllerConfig},
    model::Discovery as AgentDiscovery,
};
use agentplane_proto::fleet::{
    AgentMessage, ConfigState, ConfigStatus, Discovery, EnrollRequest, Heartbeat, Hello, Inventory,
    agent_message, controller_message, fleet_agent_client::FleetAgentClient,
};

use crate::{
    enrollment::EnrollmentState,
    identity::{self, Identity},
    oidc,
    reconcile::Reconciler,
};

pub async fn run(
    controller: ControllerConfig,
    discovered: AgentDiscovery,
    state_dir: PathBuf,
    enrollment_token: Option<String>,
    reconciler: Reconciler,
    enrollment: EnrollmentState,
) -> anyhow::Result<()> {
    let identity_path = state_dir.join("identity.json");
    let identity = match identity::load(&identity_path)? {
        Some(identity) => {
            enrollment.set("enrolled").await;
            identity
        }
        None => {
            let identity = match enrollment_token {
                Some(token) => {
                    enrollment.set("enrolling").await;
                    enroll_with_token(&controller, token).await?
                }
                None => oidc::enroll(&controller, &enrollment).await?,
            };
            identity::save(&identity_path, &identity)?;
            enrollment.set("enrolled").await;
            info!(device_id = %identity.device_id, "enrolled device");
            identity
        }
    };

    let mut delay = Duration::from_secs(1);
    loop {
        match connect(&controller, &identity, &discovered, &state_dir, &reconciler).await {
            Ok(()) => warn!("controller stream closed"),
            Err(error) => warn!(error = %error, "controller connection failed"),
        }

        time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(60));
    }
}

async fn enroll_with_token(
    controller: &ControllerConfig,
    token: String,
) -> anyhow::Result<Identity> {
    let mut client = client(controller).await?;
    let response = client
        .enroll(EnrollRequest {
            token,
            hostname: hostname(),
        })
        .await
        .context("enroll with controller")?
        .into_inner();

    Ok(Identity {
        device_id: response.device_id,
        credential: response.credential,
    })
}

async fn connect(
    controller: &ControllerConfig,
    identity: &Identity,
    discovered: &AgentDiscovery,
    state_dir: &Path,
    reconciler: &Reconciler,
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
            message = inbound.message() => {
                let Some(message) = message.context("read controller stream")? else {
                    return Ok(());
                };
                if let Some(controller_message::Message::DesiredConfig(desired)) = message.message {
                    info!(
                        revision = desired.revision,
                        bytes = desired.yaml.len(),
                        "received desired configuration"
                    );
                    let status = apply_desired_config(state_dir, desired, reconciler);
                    if status.error.is_empty() {
                        info!(revision = status.revision, "applied desired configuration");
                    } else {
                        warn!(
                            revision = status.revision,
                            error = %status.error,
                            "failed desired configuration"
                        );
                    }
                    send(&sender, agent_message::Message::ConfigStatus(status)).await?;
                }
            }
        }
    }
}

fn apply_desired_config(
    state_dir: &Path,
    desired: agentplane_proto::fleet::DesiredConfig,
    reconciler: &Reconciler,
) -> ConfigStatus {
    let result = (|| -> anyhow::Result<()> {
        let actual_hash = Sha256::digest(&desired.yaml);
        if actual_hash.as_slice() != desired.sha256 {
            bail!("configuration hash does not match payload");
        }
        debug!(
            revision = desired.revision,
            "verified desired configuration hash"
        );

        let yaml = std::str::from_utf8(&desired.yaml).context("configuration is not UTF-8")?;
        let config = config::parse(yaml)?;
        debug!(revision = desired.revision, "parsed desired configuration");
        reconciler.apply(&config)?;
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("create state directory {}", state_dir.display()))?;
        let path = state_dir.join("remote-config.yaml");
        let temporary = state_dir.join("remote-config.yaml.tmp");
        std::fs::write(&temporary, &desired.yaml)
            .with_context(|| format!("write remote configuration to {}", temporary.display()))?;
        std::fs::rename(&temporary, &path)
            .with_context(|| format!("install remote configuration at {}", path.display()))?;
        info!(
            revision = desired.revision,
            path = %path.display(),
            "persisted desired configuration"
        );
        Ok(())
    })();

    match result {
        Ok(()) => ConfigStatus {
            revision: desired.revision,
            state: ConfigState::Applied.into(),
            error: String::new(),
        },
        Err(error) => ConfigStatus {
            revision: desired.revision,
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
    controller: &ControllerConfig,
) -> anyhow::Result<FleetAgentClient<Channel>> {
    let mut endpoint = Endpoint::from_shared(controller.address.clone())?;
    if let Some(path) = &controller.ca_certificate_path {
        let pem = std::fs::read(path)
            .with_context(|| format!("read controller CA certificate from {}", path.display()))?;
        endpoint = endpoint
            .tls_config(ClientTlsConfig::new().ca_certificate(Certificate::from_pem(pem)))?;
    }
    let channel = endpoint.connect().await.context("connect to controller")?;
    Ok(FleetAgentClient::new(channel))
}

pub(crate) fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .or_else(|_| std::fs::read_to_string("/etc/hostname").map(|value| value.trim().to_string()))
        .unwrap_or_else(|_| "unknown".to_string())
}
