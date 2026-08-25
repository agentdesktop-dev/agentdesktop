use std::pin::Pin;

use agentdesktop_proto::fleet::{
    AgentMessage, BeginEnrollmentRequest, BeginEnrollmentResponse, CompleteEnrollmentRequest,
    ControllerMessage, DeviceCertificateResponse, EnrollResponse,
    InferenceGatewayCredentialRequest, InferenceGatewayCredentialResponse,
    RenewDeviceCertificateRequest, agent_message, controller_message,
    fleet_agent_server::FleetAgent,
};
use futures_core::Stream;
use tokio::{sync::mpsc, time};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use x509_parser::{extensions::GeneralName, parse_x509_certificate};

use agentdesktop_core::config::InferenceGatewayAuthentication;

use crate::{
    daemon_config::DaemonConfigStore,
    database::{Database, DevicePrincipal},
    device_ca::DeviceCertificateIssuer,
    gateway_jwt::GatewayJwtIssuer,
    oidc::{OidcPrincipal, OidcProvider},
};

#[derive(Clone)]
pub struct FleetAgentService {
    oidc: Option<OidcProvider>,
    database: Database,
    daemon_config: DaemonConfigStore,
    gateway_jwt_issuer: Option<GatewayJwtIssuer>,
    device_certificate_issuer: Option<DeviceCertificateIssuer>,
}

impl FleetAgentService {
    pub fn new(
        oidc: Option<OidcProvider>,
        database: Database,
        daemon_config: DaemonConfigStore,
        gateway_jwt_issuer: Option<GatewayJwtIssuer>,
        device_certificate_issuer: Option<DeviceCertificateIssuer>,
    ) -> Self {
        Self {
            oidc,
            database,
            daemon_config,
            gateway_jwt_issuer,
            device_certificate_issuer,
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
        let access_token = bearer_credential(request.metadata())?.to_owned();
        let request = request.into_inner();
        let csr_der = request.certificate_signing_request_der.clone();
        let completed = oidc
            .complete(request, &access_token)
            .await
            .map_err(invalid_enrollment)?;
        self.enroll_device(
            &completed.hostname,
            &completed.issuer,
            &completed.subject,
            Some(&completed.idp_claims),
            &csr_der,
        )
        .await
    }

    async fn renew_device_certificate(
        &self,
        request: Request<RenewDeviceCertificateRequest>,
    ) -> Result<Response<DeviceCertificateResponse>, Status> {
        let device_id = self.authenticate_device(&request).await?;
        let issuer = self.device_certificate_issuer.as_ref().ok_or_else(|| {
            Status::failed_precondition("device certificate issuance is disabled")
        })?;
        let issued = issuer
            .issue(
                &device_id,
                &request.into_inner().certificate_signing_request_der,
            )
            .map_err(invalid_csr)?;
        info!(device_id, "renewed device certificate");
        Ok(Response::new(DeviceCertificateResponse {
            client_certificate_pem: issued.chain_pem,
            expires_at_unix_seconds: issued.expires_at_unix_seconds,
        }))
    }

    async fn get_inference_gateway_credential(
        &self,
        request: Request<InferenceGatewayCredentialRequest>,
    ) -> Result<Response<InferenceGatewayCredentialResponse>, Status> {
        let device_id = self.authenticate_device(&request).await?;
        let client_id = request.into_inner().client_id;
        if !agentdesktop_core::config::valid_client_id(&client_id) {
            return Err(Status::invalid_argument("invalid client_id"));
        }
        let daemon = self
            .daemon_config
            .current()
            .ok_or_else(|| Status::failed_precondition("no daemon configuration is active"))?;
        let yaml = std::str::from_utf8(&daemon.yaml)
            .map_err(|_| Status::internal("daemon configuration is not UTF-8"))?;
        let config = agentdesktop_core::config::parse_daemon(yaml).map_err(internal)?;
        let gateway = config
            .inference_gateway
            .as_ref()
            .ok_or_else(|| Status::not_found("inference gateway is not configured"))?;
        let (audience, allowed_client_ids) = match gateway.authentication.as_ref() {
            Some(InferenceGatewayAuthentication::ControllerJwt {
                audience,
                allowed_client_ids,
            }) => (audience, allowed_client_ids),
            Some(InferenceGatewayAuthentication::Oidc { .. }) | None => {
                return Err(Status::failed_precondition(
                    "inference gateway does not use controller JWT authentication",
                ));
            }
        };
        if !allowed_client_ids.contains(&client_id) {
            warn!(
                device_id,
                client_id, "rejected disallowed inference gateway client"
            );
            return Err(Status::permission_denied("client_id is not allowed"));
        }
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
        let authenticated_device_id = self.authenticate_device(&request).await?;
        let oauth_revalidation = (
            self.oidc.clone().ok_or_else(|| {
                Status::failed_precondition("OIDC access-token validation is disabled")
            })?,
            bearer_credential(request.metadata())?.to_owned(),
        );

        let mut inbound = request.into_inner();
        let (sender, receiver) = mpsc::channel(8);
        let mut daemon_updates = self.daemon_config.subscribe();
        let daemon_config = daemon_updates.borrow().clone();
        if let Some(daemon_config) = daemon_config {
            sender
                .send(Ok(ControllerMessage {
                    message: Some(controller_message::Message::DaemonConfig(daemon_config)),
                }))
                .await
                .map_err(|_| Status::unavailable("stream closed"))?;
        }

        let update_sender = sender.clone();
        tokio::spawn(async move {
            while daemon_updates.changed().await.is_ok() {
                let daemon = daemon_updates.borrow().clone();
                let Some(daemon) = daemon else {
                    continue;
                };
                if update_sender
                    .send(Ok(ControllerMessage {
                        message: Some(controller_message::Message::DaemonConfig(daemon)),
                    }))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });

        let database = self.database.clone();
        let auth_sender = sender.clone();
        tokio::spawn(async move {
            let enrolled_principal = match database.device_principal(&authenticated_device_id).await
            {
                Ok(principal) => principal,
                Err(error) => {
                    error!(%error, device_id = %authenticated_device_id, "failed to load stream identity");
                    return;
                }
            };
            let mut auth_interval = time::interval(std::time::Duration::from_secs(5 * 60));
            auth_interval.tick().await;
            loop {
                tokio::select! {
                _ = auth_interval.tick() => {
                    let (oidc, access_token) = &oauth_revalidation;
                    match oidc.authenticate_access_token(access_token).await {
                        Ok(principal) if oidc_principals_match(&principal, &enrolled_principal) => {}
                        Ok(_) => {
                            warn!(device_id = %authenticated_device_id, "closing stream after OIDC principal changed");
                            let _ = auth_sender
                                .send(Err(Status::permission_denied("OIDC principal changed")))
                                .await;
                            break;
                        }
                        Err(error) => {
                            let status = invalid_access_token(error);
                            if status.code() == tonic::Code::Unavailable {
                                warn!(device_id = %authenticated_device_id, "OIDC unavailable during stream revalidation; retaining connection");
                                continue;
                            }
                            warn!(device_id = %authenticated_device_id, "closing stream after OIDC access token revalidation failed");
                            let _ = auth_sender.send(Err(status)).await;
                            break;
                        }
                    }
                }
                message = inbound.message() => match message {
                    Ok(Some(message)) => {
                        if let Some(agent_message::Message::Hello(hello)) = &message.message
                            && hello.device_id != authenticated_device_id
                        {
                            warn!(
                                authenticated_device_id,
                                claimed_device_id = %hello.device_id,
                                "device certificate and hello identity do not match"
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
                },
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

impl FleetAgentService {
    async fn authenticate_device<T>(&self, request: &Request<T>) -> Result<String, Status> {
        let certificate_device_id = peer_certificate_device_id(request)?;
        self.authenticate_device_identity(certificate_device_id.as_deref(), request.metadata())
            .await
    }

    async fn authenticate_device_identity(
        &self,
        certificate_device_id: Option<&str>,
        metadata: &tonic::metadata::MetadataMap,
    ) -> Result<String, Status> {
        let device_id = certificate_device_id
            .ok_or_else(|| Status::unauthenticated("missing device certificate"))?;
        let access_token = bearer_credential(metadata)?;
        let oidc = self.oidc.as_ref().ok_or_else(|| {
            Status::failed_precondition("OIDC access-token validation is disabled")
        })?;
        let access_principal = oidc
            .authenticate_access_token(access_token)
            .await
            .map_err(invalid_access_token)?;
        self.authenticate_device_certificate_identity(device_id, &access_principal)
            .await
    }

    async fn authenticate_device_certificate_identity(
        &self,
        device_id: &str,
        access_principal: &OidcPrincipal,
    ) -> Result<String, Status> {
        let principal = self
            .database
            .device_principal(device_id)
            .await
            .map_err(|error| {
                warn!(%error, device_id, "certificate references an unknown device");
                Status::unauthenticated("unrecognized device certificate")
            })?;
        if !oidc_principals_match(access_principal, &principal) {
            return Err(Status::permission_denied(
                "access token principal does not match enrolled device user",
            ));
        }
        Ok(device_id.to_owned())
    }

    async fn enroll_device(
        &self,
        hostname: &str,
        enrolled_by_issuer: &str,
        enrolled_by_subject: &str,
        idp_claims: Option<&std::collections::BTreeMap<String, serde_json::Value>>,
        csr_der: &[u8],
    ) -> Result<Response<EnrollResponse>, Status> {
        let device_id = Uuid::new_v4().to_string();
        let issued_certificate = self
            .device_certificate_issuer
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("device certificate issuance is disabled"))?
            .issue(&device_id, csr_der)
            .map_err(invalid_csr)?;
        self.database
            .enroll_device(
                &device_id,
                hostname,
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
            client_certificate_pem: issued_certificate.chain_pem,
            client_certificate_expires_at_unix_seconds: issued_certificate.expires_at_unix_seconds,
        }))
    }
}

fn oidc_principals_match(authenticated: &OidcPrincipal, enrolled: &DevicePrincipal) -> bool {
    !enrolled.issuer.is_empty()
        && !enrolled.subject.is_empty()
        && authenticated.issuer == enrolled.issuer
        && authenticated.subject == enrolled.subject
}

fn peer_certificate_device_id<T>(request: &Request<T>) -> Result<Option<String>, Status> {
    let Some(certificates) = request.peer_certs() else {
        return Ok(None);
    };
    let certificate = certificates
        .first()
        .ok_or_else(|| Status::unauthenticated("missing device certificate"))?;
    device_id_from_certificate(certificate.as_ref()).map(Some)
}

fn device_id_from_certificate(certificate_der: &[u8]) -> Result<String, Status> {
    const SUFFIX: &str = ".device.agentdesktop.invalid";
    let (_, certificate) = parse_x509_certificate(certificate_der)
        .map_err(|_| Status::unauthenticated("invalid device certificate"))?;
    let subject_alt_name = certificate
        .subject_alternative_name()
        .map_err(|_| Status::unauthenticated("invalid device certificate subject"))?
        .ok_or_else(|| Status::unauthenticated("device certificate has no subject identity"))?;
    let mut device_ids =
        subject_alt_name
            .value
            .general_names
            .iter()
            .filter_map(|name| match name {
                GeneralName::DNSName(name) => name.strip_suffix(SUFFIX),
                _ => None,
            });
    let device_id = device_ids
        .next()
        .ok_or_else(|| Status::unauthenticated("device certificate has no device identity"))?;
    if device_ids.next().is_some() || Uuid::parse_str(device_id).is_err() {
        return Err(Status::unauthenticated(
            "device certificate has an invalid device identity",
        ));
    }
    Ok(device_id.to_owned())
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
            let model_runtimes = inventory.model_runtimes.len();
            let models = inventory
                .model_runtimes
                .iter()
                .map(|runtime| runtime.models.len())
                .sum::<usize>();
            database.replace_inventory(device_id, &inventory).await?;
            info!(
                device_id,
                discoveries, mcp_servers, skills, model_runtimes, models, "stored device inventory"
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
        Some(agent_message::Message::Telemetry(event)) => {
            database.insert_telemetry(device_id, &event).await?;
            debug!(device_id, "stored telemetry event");
        }
        None => {}
    }
    Ok(())
}

fn bearer_credential(metadata: &tonic::metadata::MetadataMap) -> Result<&str, Status> {
    let value = metadata
        .get("authorization")
        .ok_or_else(|| Status::unauthenticated("missing OIDC access token"))?
        .to_str()
        .map_err(|_| Status::unauthenticated("invalid OIDC access token"))?;
    value
        .strip_prefix("Bearer ")
        .ok_or_else(|| Status::unauthenticated("invalid OIDC access token"))
}

fn internal(err: anyhow::Error) -> Status {
    error!(error = %err, "controller state operation failed");
    Status::internal("controller state error")
}

fn invalid_enrollment(err: anyhow::Error) -> Status {
    warn!(error = %err, "OIDC enrollment failed");
    Status::unauthenticated("OIDC enrollment failed")
}

fn invalid_csr(err: anyhow::Error) -> Status {
    warn!(error = %err, "device certificate request failed");
    Status::invalid_argument("invalid certificate signing request")
}

fn invalid_access_token(err: anyhow::Error) -> Status {
    warn!(error = %err, "OIDC access token rejected");
    let request_error = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>());
    if let Some(request_error) = request_error {
        return match request_error.status() {
            Some(reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN) => {
                Status::unauthenticated("OIDC access token rejected")
            }
            _ => Status::unavailable("OIDC access-token validation is unavailable"),
        };
    }
    Status::unauthenticated("OIDC access token rejected")
}

#[cfg(test)]
mod tests {
    use rcgen::{CertificateParams, KeyPair};
    use tonic::Code;

    use super::{FleetAgentService, device_id_from_certificate};
    use crate::{daemon_config::DaemonConfigStore, database::Database, oidc::OidcPrincipal};

    #[tokio::test]
    async fn device_authentication_binds_oidc_issuer_and_subject() {
        let device_id = "7ca03414-bb20-4c80-98ef-7b0538b988ba";
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-mtls-{}-{}.db",
            std::process::id(),
            rand::random::<u64>()
        ));
        let database = Database::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("connect database");
        database
            .enroll_device(device_id, "host", "issuer", "subject", None)
            .await
            .expect("enroll device");
        let service =
            FleetAgentService::new(None, database, DaemonConfigStore::new(None), None, None);
        let principal = |issuer: &str, subject: &str| OidcPrincipal {
            issuer: issuer.to_owned(),
            subject: subject.to_owned(),
        };
        assert_eq!(
            service
                .authenticate_device_certificate_identity(
                    device_id,
                    &principal("issuer", "subject"),
                )
                .await
                .unwrap(),
            device_id
        );

        for mismatched in [
            principal("issuer", "other-subject"),
            principal("other-issuer", "subject"),
            principal("other-issuer", "other-subject"),
        ] {
            let status = service
                .authenticate_device_certificate_identity(device_id, &mismatched)
                .await
                .expect_err("mismatched OIDC principal must fail");
            assert_eq!(status.code(), Code::PermissionDenied);
        }

        let status = service
            .authenticate_device_certificate_identity(
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                &principal("issuer", "subject"),
            )
            .await
            .expect_err("unknown device certificate must fail");
        assert_eq!(status.code(), Code::Unauthenticated);
    }

    #[tokio::test]
    async fn legacy_device_without_enrolled_issuer_fails_closed() {
        let device_id = "7ca03414-bb20-4c80-98ef-7b0538b988ba";
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-empty-issuer-{}-{}.db",
            std::process::id(),
            rand::random::<u64>()
        ));
        let database = Database::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("connect database");
        database
            .enroll_device(device_id, "host", "", "subject", None)
            .await
            .expect("enroll legacy device");
        let service =
            FleetAgentService::new(None, database, DaemonConfigStore::new(None), None, None);

        let status = service
            .authenticate_device_certificate_identity(
                device_id,
                &OidcPrincipal {
                    issuer: String::new(),
                    subject: "subject".to_owned(),
                },
            )
            .await
            .expect_err("empty enrolled issuer must fail closed");
        assert_eq!(status.code(), Code::PermissionDenied);
    }

    #[test]
    fn extracts_controller_device_id_from_certificate_san() {
        let device_id = "7ca03414-bb20-4c80-98ef-7b0538b988ba";
        let key = KeyPair::generate().unwrap();
        let certificate =
            CertificateParams::new(vec![format!("{device_id}.device.agentdesktop.invalid")])
                .unwrap()
                .self_signed(&key)
                .unwrap();
        assert_eq!(
            device_id_from_certificate(certificate.der().as_ref()).unwrap(),
            device_id
        );
    }
}
