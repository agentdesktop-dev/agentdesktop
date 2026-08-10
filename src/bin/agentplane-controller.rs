use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
};

use agentplane::{
    database::Database,
    fleet::{
        AgentMessage, ControllerMessage, DesiredConfig, EnrollRequest, EnrollResponse,
        agent_message, controller_message,
        fleet_agent_server::{FleetAgent, FleetAgentServer},
    },
};
use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::Parser;
use futures_core::Stream;
use rand::RngCore;
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

    #[arg(long, default_value = "sqlite://agentplane-controller.db?mode=rwc")]
    database_url: String,

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
    database: Database,
    desired_config: Option<DesiredConfig>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let desired_config =
        load_desired_config(args.desired_config.as_deref(), args.desired_config_revision)?;
    let database = Database::connect(&args.database_url).await?;
    let service = Service {
        enrollment_token: args.enrollment_token,
        database,
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
        let credential = new_credential();
        self.database
            .enroll_device(&device_id, &request.hostname, &credential_hash(&credential))
            .await
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
            .database
            .authenticate(&credential_hash(credential))
            .await
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

        let database = self.database.clone();
        let response_sender = sender.clone();
        tokio::spawn(async move {
            let _response_sender = response_sender;
            loop {
                match inbound.message().await {
                    Ok(Some(message)) => {
                        if let Some(agent_message::Message::Hello(hello)) = &message.message
                            && hello.device_id != authenticated_device_id
                        {
                            eprintln!("device credential and hello identity do not match");
                            break;
                        }
                        if let Err(error) =
                            handle_agent_message(&database, &authenticated_device_id, message).await
                        {
                            eprintln!("store device state: {error:#}");
                            break;
                        }
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

async fn handle_agent_message(
    database: &Database,
    device_id: &str,
    message: AgentMessage,
) -> anyhow::Result<()> {
    match message.message {
        Some(agent_message::Message::Hello(hello)) => {
            database.update_hello(device_id, &hello).await?;
            eprintln!("device {device_id} connected");
        }
        Some(agent_message::Message::Heartbeat(heartbeat)) => {
            database
                .update_heartbeat(device_id, heartbeat.unix_time_seconds)
                .await?;
        }
        Some(agent_message::Message::Inventory(inventory)) => {
            database.replace_inventory(device_id, &inventory).await?;
        }
        Some(agent_message::Message::ConfigStatus(status)) => {
            database.update_config_status(device_id, &status).await?;
            if !status.error.is_empty() {
                eprintln!("device configuration failed: {}", status.error);
            }
        }
        None => {}
    }
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

fn new_credential() -> String {
    let mut bytes = [0_u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn credential_hash(credential: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(credential.as_bytes()))
}

fn internal(error: anyhow::Error) -> Status {
    eprintln!("controller state: {error:#}");
    Status::internal("controller state error")
}
