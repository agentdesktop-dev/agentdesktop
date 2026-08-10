use std::{
    collections::HashMap,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Mutex, RwLock},
};

use agentplane::fleet::{
    AgentMessage, ControllerMessage, DesiredConfig, EnrollRequest, EnrollResponse, agent_message,
    controller_message,
    fleet_agent_server::{FleetAgent, FleetAgentServer},
};
use anyhow::Context;
use clap::Parser;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Request, Response, Status,
    transport::{Identity, Server, ServerTlsConfig},
};
use uuid::Uuid;

#[derive(Parser)]
#[command(about = "Agentplane fleet controller")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8443")]
    listen: SocketAddr,

    #[arg(long)]
    enrollment_token: Option<String>,

    #[arg(long, default_value = "./controller-state.json")]
    state: PathBuf,

    #[arg(long)]
    desired_config: Option<PathBuf>,

    #[arg(long, default_value_t = 1)]
    desired_config_revision: u64,

    #[arg(long, requires = "tls_key")]
    tls_certificate: Option<PathBuf>,

    #[arg(long, requires = "tls_certificate")]
    tls_key: Option<PathBuf>,
}

#[derive(Clone)]
struct Service {
    enrollment_token: Option<String>,
    store: Arc<Store>,
    devices: Arc<RwLock<HashMap<String, DeviceSnapshot>>>,
    desired_config: Option<DesiredConfig>,
}

#[derive(Clone, Debug, Default)]
struct DeviceSnapshot {
    hostname: String,
    last_heartbeat: u64,
    discovery_count: usize,
    applied_config_revision: u64,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredState {
    credentials: HashMap<String, String>,
}

struct Store {
    path: PathBuf,
    state: Mutex<StoredState>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let desired_config =
        load_desired_config(args.desired_config.as_deref(), args.desired_config_revision)?;
    let service = Service {
        enrollment_token: args.enrollment_token,
        store: Arc::new(Store::load(args.state)?),
        devices: Arc::new(RwLock::new(HashMap::new())),
        desired_config,
    };

    let mut server = Server::builder();
    if let (Some(certificate), Some(key)) = (args.tls_certificate, args.tls_key) {
        let certificate = std::fs::read(&certificate)
            .with_context(|| format!("read TLS certificate from {}", certificate.display()))?;
        let key =
            std::fs::read(&key).with_context(|| format!("read TLS key from {}", key.display()))?;
        server = server
            .tls_config(ServerTlsConfig::new().identity(Identity::from_pem(certificate, key)))?;
    }

    eprintln!("agentplane-controller listening on {}", args.listen);
    server
        .add_service(FleetAgentServer::new(service))
        .serve(args.listen)
        .await
        .context("serve fleet gRPC API")
}

#[tonic::async_trait]
impl FleetAgent for Service {
    type ConnectStream = Pin<Box<dyn Stream<Item = Result<ControllerMessage, Status>> + Send>>;

    async fn enroll(
        &self,
        request: Request<EnrollRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let request = request.into_inner();
        let Some(expected) = &self.enrollment_token else {
            return Err(Status::failed_precondition("enrollment is disabled"));
        };
        if request.token != *expected {
            return Err(Status::unauthenticated("invalid enrollment token"));
        }

        let device_id = Uuid::new_v4().to_string();
        let credential = Uuid::new_v4().to_string();
        self.store
            .insert(device_id.clone(), credential.clone())
            .map_err(internal)?;
        eprintln!("enrolled {} ({})", device_id, request.hostname);

        Ok(Response::new(EnrollResponse {
            device_id,
            credential,
        }))
    }

    async fn connect(
        &self,
        request: Request<tonic::Streaming<AgentMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let credential = bearer_credential(request.metadata())?;
        let authenticated_device_id = self
            .store
            .device_id(credential)
            .map_err(internal)?
            .ok_or_else(|| Status::unauthenticated("unknown device credential"))?;

        let mut inbound = request.into_inner();
        let (sender, receiver) = mpsc::channel(8);
        if let Some(desired_config) = self.desired_config.clone() {
            sender
                .send(Ok(ControllerMessage {
                    message: Some(controller_message::Message::DesiredConfig(desired_config)),
                }))
                .await
                .map_err(|_| Status::unavailable("stream closed"))?;
        }

        let devices = self.devices.clone();
        let response_sender = sender.clone();
        tokio::spawn(async move {
            let _response_sender = response_sender;
            let mut device_id = None;
            loop {
                match inbound.message().await {
                    Ok(Some(message)) => {
                        if let Some(agent_message::Message::Hello(hello)) = &message.message
                            && hello.device_id != authenticated_device_id
                        {
                            eprintln!("device credential and hello identity do not match");
                            break;
                        }
                        handle_agent_message(&devices, &mut device_id, message);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        eprintln!("device stream: {error}");
                        break;
                    }
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

fn handle_agent_message(
    devices: &RwLock<HashMap<String, DeviceSnapshot>>,
    device_id: &mut Option<String>,
    message: AgentMessage,
) {
    let Ok(mut devices) = devices.write() else {
        return;
    };
    match message.message {
        Some(agent_message::Message::Hello(hello)) => {
            *device_id = Some(hello.device_id.clone());
            devices.entry(hello.device_id.clone()).or_default().hostname = hello.hostname;
            eprintln!("device {} connected", hello.device_id);
        }
        Some(agent_message::Message::Heartbeat(heartbeat)) => {
            if let Some(device) = device_id.as_ref().and_then(|id| devices.get_mut(id)) {
                device.last_heartbeat = heartbeat.unix_time_seconds;
            }
        }
        Some(agent_message::Message::Inventory(inventory)) => {
            if let Some(device) = device_id.as_ref().and_then(|id| devices.get_mut(id)) {
                device.discovery_count = inventory.discoveries.len();
            }
        }
        Some(agent_message::Message::ConfigStatus(status)) => {
            if let Some(device) = device_id.as_ref().and_then(|id| devices.get_mut(id)) {
                if status.error.is_empty() {
                    device.applied_config_revision = status.revision;
                } else {
                    eprintln!("device configuration failed: {}", status.error);
                }
            }
        }
        None => {}
    }
}

impl Store {
    fn load(path: PathBuf) -> anyhow::Result<Self> {
        let state = match std::fs::read(&path) {
            Ok(contents) => serde_json::from_slice(&contents)
                .with_context(|| format!("parse controller state from {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => StoredState::default(),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read controller state from {}", path.display()));
            }
        };
        Ok(Self {
            path,
            state: Mutex::new(state),
        })
    }

    fn insert(&self, device_id: String, credential: String) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        state.credentials.insert(device_id, credential);
        save_json_atomically(&self.path, &*state)
    }

    fn device_id(&self, credential: &str) -> anyhow::Result<Option<String>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("state lock poisoned"))?;
        Ok(state
            .credentials
            .iter()
            .find_map(|(device_id, stored)| (stored == credential).then(|| device_id.clone())))
    }
}

fn save_json_atomically(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn load_desired_config(
    path: Option<&Path>,
    revision: u64,
) -> anyhow::Result<Option<DesiredConfig>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let yaml = std::fs::read(path)
        .with_context(|| format!("read desired configuration from {}", path.display()))?;
    let text = std::str::from_utf8(&yaml).context("desired configuration is not UTF-8")?;
    agentplane::config::parse(text).context("validate desired configuration")?;
    let sha256 = Sha256::digest(&yaml).to_vec();
    Ok(Some(DesiredConfig {
        revision,
        yaml,
        sha256,
    }))
}

fn bearer_credential(metadata: &tonic::metadata::MetadataMap) -> Result<&str, Status> {
    let value = metadata
        .get("authorization")
        .ok_or_else(|| Status::unauthenticated("missing device credential"))?
        .to_str()
        .map_err(|_| Status::unauthenticated("invalid device credential"))?;
    value
        .strip_prefix("Bearer ")
        .ok_or_else(|| Status::unauthenticated("invalid device credential"))
}

fn internal(error: anyhow::Error) -> Status {
    eprintln!("controller state: {error:#}");
    Status::internal("controller state error")
}
