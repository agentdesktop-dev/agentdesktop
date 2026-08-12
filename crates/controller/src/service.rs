use std::{path::Path, pin::Pin};

use agentdesktop_proto::fleet::{
    AgentMessage, BeginEnrollmentRequest, BeginEnrollmentResponse, CompleteEnrollmentRequest,
    ControllerMessage, DesiredConfig, EnrollResponse, InferenceGatewayCredentialRequest,
    InferenceGatewayCredentialResponse, agent_message, controller_message,
    fleet_agent_server::FleetAgent,
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

use agentdesktop_core::config::InferenceGatewayAuthentication;

use crate::{database::Database, gateway_jwt::GatewayJwtIssuer, oidc::OidcProvider};

#[derive(Clone)]
pub struct FleetAgentService {
    oidc: Option<OidcProvider>,
    database: Database,
    desired_config: Option<DesiredConfig>,
    gateway_jwt_issuer: Option<GatewayJwtIssuer>,
}

impl FleetAgentService {
    pub fn new(
        oidc: Option<OidcProvider>,
        database: Database,
        desired_config: Option<DesiredConfig>,
        gateway_jwt_issuer: Option<GatewayJwtIssuer>,
    ) -> Self {
        Self {
            oidc,
            database,
            desired_config,
            gateway_jwt_issuer,
        }
    }
}

#[tonic::async_trait]
impl FleetAgent for FleetAgentService {
    type ConnectStream = Pin<Box<dyn Stream<Item = Result<ControllerMessage, Status>> + Send>>;

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
        self.enroll_device(
            &completed.hostname,
            &completed.issuer,
            &completed.subject,
            Some(&completed.idp_claims),
        )
        .await
    }

    async fn get_inference_gateway_credential(
        &self,
        request: Request<InferenceGatewayCredentialRequest>,
    ) -> Result<Response<InferenceGatewayCredentialResponse>, Status> {
        let device_id = self.authenticate_device(request.metadata()).await?;
        let client_id = request.into_inner().client_id;
        if client_id.is_empty()
            || client_id.len() > 64
            || !client_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(Status::invalid_argument("invalid client_id"));
        }
        let desired = self
            .desired_config
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("no desired configuration is active"))?;
        let yaml = std::str::from_utf8(&desired.yaml)
            .map_err(|_| Status::internal("desired configuration is not UTF-8"))?;
        let config = agentdesktop_core::config::parse_desired(yaml).map_err(internal)?;
        let gateway = config
            .inference_gateway
            .as_ref()
            .ok_or_else(|| Status::not_found("inference gateway is not configured"))?;
        let audience = match gateway.authentication.as_ref() {
            Some(InferenceGatewayAuthentication::ControllerJwt { audience }) => audience,
            None => {
                return Err(Status::failed_precondition(
                    "inference gateway does not use controller JWT authentication",
                ));
            }
        };
        let issuer = self.gateway_jwt_issuer.as_ref().ok_or_else(|| {
            Status::failed_precondition("controller gateway JWT issuer is not configured")
        })?;
        let principal = self
            .database
            .device_principal(&device_id)
            .await
            .map_err(internal)?;
        let subject = if principal.subject.is_empty() {
            device_id.as_str()
        } else {
            principal.subject.as_str()
        };
        let (credential, expires_at_unix_seconds) = issuer
            .issue(
                subject,
                &device_id,
                &client_id,
                audience,
                principal.idp_claims.as_ref(),
            )
            .map_err(internal)?;
        info!(
            device_id,
            expires_at_unix_seconds,
            enrolled_by_issuer = principal.issuer,
            client_id,
            "issued inference gateway credential"
        );
        Ok(Response::new(InferenceGatewayCredentialResponse {
            credential,
            expires_at_unix_seconds,
        }))
    }

    async fn connect(
        &self,
        request: Request<tonic::Streaming<AgentMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let authenticated_device_id = self.authenticate_device(request.metadata()).await?;

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
    async fn authenticate_device(
        &self,
        metadata: &tonic::metadata::MetadataMap,
    ) -> Result<String, Status> {
        let credential = bearer_credential(metadata)?;
        self.database
            .authenticate(&credential_hash(credential))
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::unauthenticated("unknown device credential"))
    }

    async fn enroll_device(
        &self,
        hostname: &str,
        enrolled_by_issuer: &str,
        enrolled_by_subject: &str,
        idp_claims: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
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
                idp_claims,
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
            let mcp_servers = inventory
                .discoveries
                .iter()
                .map(|discovery| discovery.mcp_servers.len())
                .sum::<usize>();
            let skills = inventory
                .discoveries
                .iter()
                .map(|discovery| discovery.skills.len())
                .sum::<usize>();
            database.replace_inventory(device_id, &inventory).await?;
            info!(
                device_id,
                discoveries, mcp_servers, skills, "stored device inventory"
            );
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
    agentdesktop_core::config::parse_desired(text).context("validate desired configuration")?;
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
