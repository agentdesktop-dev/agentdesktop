use std::{path::Path, pin::Pin};

use agentplane_proto::fleet::{
    AgentMessage, BeginEnrollmentRequest, BeginEnrollmentResponse, CompleteEnrollmentRequest,
    ControllerMessage, DesiredConfig, EnrollRequest, EnrollResponse, agent_message,
    controller_message, fleet_agent_server::FleetAgent,
};
use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use futures_core::Stream;
use rand::RngCore;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{database::Database, oidc::OidcProvider};

#[derive(Clone)]
pub struct FleetAgentService {
    enrollment_token: Option<String>,
    oidc: Option<OidcProvider>,
    database: Database,
    desired_config: Option<DesiredConfig>,
}

impl FleetAgentService {
    pub fn new(
        enrollment_token: Option<String>,
        oidc: Option<OidcProvider>,
        database: Database,
        desired_config: Option<DesiredConfig>,
    ) -> Self {
        Self {
            enrollment_token,
            oidc,
            database,
            desired_config,
        }
    }
}

#[tonic::async_trait]
impl FleetAgent for FleetAgentService {
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

        self.enroll_device(&request.hostname, "", "").await
    }

    async fn begin_enrollment(
        &self,
        request: Request<BeginEnrollmentRequest>,
    ) -> Result<Response<BeginEnrollmentResponse>, Status> {
        let oidc = self
            .oidc
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("OIDC enrollment is disabled"))?;
        let request = request.into_inner();
        let response = oidc
            .begin(request.hostname, &request.code_challenge)
            .await
            .map_err(invalid_enrollment)?;
        Ok(Response::new(response))
    }

    async fn complete_enrollment(
        &self,
        request: Request<CompleteEnrollmentRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let oidc = self
            .oidc
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("OIDC enrollment is disabled"))?;
        let completed = oidc
            .complete(request.into_inner())
            .await
            .map_err(invalid_enrollment)?;
        self.enroll_device(&completed.hostname, &completed.issuer, &completed.subject)
            .await
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
                            warn!(
                                authenticated_device_id,
                                claimed_device_id = %hello.device_id,
                                "device credential and hello identity do not match"
                            );
                            break;
                        }
                        if let Err(err) =
                            handle_agent_message(&database, &authenticated_device_id, message).await
                        {
                            error!(
                                error = %err,
                                device_id = %authenticated_device_id,
                                "failed to store device state"
                            );
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        warn!(error = %err, device_id = %authenticated_device_id, "device stream failed");
                        break;
                    }
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

impl FleetAgentService {
    async fn enroll_device(
        &self,
        hostname: &str,
        enrolled_by_issuer: &str,
        enrolled_by_subject: &str,
    ) -> Result<Response<EnrollResponse>, Status> {
        let device_id = Uuid::new_v4().to_string();
        let credential = new_credential();
        self.database
            .enroll_device(
                &device_id,
                hostname,
                &credential_hash(&credential),
                enrolled_by_issuer,
                enrolled_by_subject,
            )
            .await
            .map_err(internal)?;
        info!(
            %device_id,
            hostname,
            enrolled_by_issuer,
            enrolled_by_subject,
            "enrolled device"
        );

        Ok(Response::new(EnrollResponse {
            device_id,
            credential,
        }))
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
            info!(device_id, hostname = %hello.hostname, "device connected");
        }
        Some(agent_message::Message::Heartbeat(heartbeat)) => {
            database
                .update_heartbeat(device_id, heartbeat.unix_time_seconds)
                .await?;
        }
        Some(agent_message::Message::Inventory(inventory)) => {
            let discoveries = inventory.discoveries.len();
            database.replace_inventory(device_id, &inventory).await?;
            info!(device_id, discoveries, "stored device inventory");
        }
        Some(agent_message::Message::ConfigStatus(status)) => {
            database.update_config_status(device_id, &status).await?;
            if status.error.is_empty() {
                info!(
                    device_id,
                    revision = status.revision,
                    "device applied configuration"
                );
            } else {
                warn!(
                    device_id,
                    revision = status.revision,
                    error = %status.error,
                    "device configuration failed"
                );
            }
        }
        None => {}
    }
    Ok(())
}

pub fn load_desired_config(
    path: Option<&Path>,
    revision: u64,
) -> anyhow::Result<Option<DesiredConfig>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let yaml = std::fs::read(path)
        .with_context(|| format!("read desired configuration from {}", path.display()))?;
    let text = std::str::from_utf8(&yaml).context("desired configuration is not UTF-8")?;
    agentplane_core::config::parse(text).context("validate desired configuration")?;
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

fn internal(err: anyhow::Error) -> Status {
    error!(error = %err, "controller state operation failed");
    Status::internal("controller state error")
}

fn invalid_enrollment(err: anyhow::Error) -> Status {
    warn!(error = %err, "OIDC enrollment failed");
    Status::unauthenticated("OIDC enrollment failed")
}
