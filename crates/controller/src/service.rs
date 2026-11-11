use std::pin::Pin;

use agentdesktop_proto::fleet::{
    AgentMessage, BeginEnrollmentRequest, BeginEnrollmentResponse, CompleteEnrollmentRequest,
    ConfigStatus, ControllerMessage, DeviceCertificateResponse, DeviceUsageRequest,
    DeviceUsageResponse, EnrollResponse, LlmGatewayCredentialRequest, LlmGatewayCredentialResponse,
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

use agentdesktop_core::config::{LlmGatewayAuthentication, ProgramsConfig};
use agentdesktop_usage::UsageError;

use crate::{
    audit::{Actor, AuditEvent, AuditLog, EventKind, Outcome, RequestInfo},
    daemon_config::DaemonConfigStore,
    database::{Database, DevicePrincipal},
    device_ca::DeviceCertificateIssuer,
    gateway_jwt::GatewayJwtIssuer,
    management::auth::authorization::DEVICES,
    oidc::{CompletedEnrollment, OidcPrincipal, OidcProvider},
};

/// gRPC method paths, recorded as `request.path` in audit events.
const COMPLETE_ENROLLMENT: &str = "/agentdesktop.fleet.v1.FleetAgent/CompleteEnrollment";
const RENEW_DEVICE_CERTIFICATE: &str = "/agentdesktop.fleet.v1.FleetAgent/RenewDeviceCertificate";
/// The `action` of enrollment's attempted and completed audit events.
const ENROLL_DEVICE: &str = "devices.enroll";

#[derive(Clone)]
pub struct FleetAgentService {
    oidc: Option<OidcProvider>,
    database: Database,
    daemon_config: DaemonConfigStore,
    gateway_jwt_issuer: Option<GatewayJwtIssuer>,
    device_certificate_issuer: Option<DeviceCertificateIssuer>,
    usage_enabled: bool,
    audit: AuditLog,
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
            audit: AuditLog::new(database.clone()),
            database,
            daemon_config,
            gateway_jwt_issuer,
            device_certificate_issuer,
            usage_enabled: false,
        }
    }

    #[cfg(test)]
    fn with_audit(mut self, audit: AuditLog) -> Self {
        self.audit = audit;
        self
    }

    /// Tells devices that the controller collects gateway usage.
    pub fn with_usage_collection(mut self, enabled: bool) -> Self {
        self.usage_enabled = enabled;
        self
    }
}

fn subscription_is_configured_for_client(programs: &ProgramsConfig, client_id: &str) -> bool {
    programs.subscription_auth(client_id).is_some()
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
        let response = if request.device_authorization {
            oidc.begin_device(request.hostname).await
        } else {
            oidc.begin(request.hostname, &request.code_challenge).await
        }
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
        let info = grpc_request_info(&request, COMPLETE_ENROLLMENT);
        let request = request.into_inner();
        let csr_der = request.certificate_signing_request_der.clone();
        let completed = match oidc.complete(request, &access_token).await {
            Ok(completed) => completed,
            Err(rejected) => {
                if let Some(verified) = &rejected.verified {
                    self.record_enrollment_denied(verified, rejected.reason, &info)
                        .await;
                }
                return Err(invalid_enrollment(rejected.error));
            }
        };
        self.enroll_device(&completed, &csr_der, info).await
    }

    async fn renew_device_certificate(
        &self,
        request: Request<RenewDeviceCertificateRequest>,
    ) -> Result<Response<DeviceCertificateResponse>, Status> {
        let info = grpc_request_info(&request, RENEW_DEVICE_CERTIFICATE);
        let device_id = self.authenticate_device(&request).await?;
        self.renew_certificate(
            &device_id,
            &request.into_inner().certificate_signing_request_der,
            info,
        )
        .await
        .map(Response::new)
    }

    async fn get_llm_gateway_credential(
        &self,
        request: Request<LlmGatewayCredentialRequest>,
    ) -> Result<Response<LlmGatewayCredentialResponse>, Status> {
        let device_id = self.authenticate_device(&request).await?;
        let LlmGatewayCredentialRequest {
            client_id,
            subscription,
        } = request.into_inner();
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
            .llm_gateway
            .as_ref()
            .ok_or_else(|| Status::not_found("LLM gateway is not configured"))?;
        let (audience, allowed_client_ids) = match gateway.authentication.as_ref() {
            Some(LlmGatewayAuthentication::ControllerJwt {
                audience,
                allowed_client_ids,
            }) => (audience, allowed_client_ids),
            Some(LlmGatewayAuthentication::Oidc { .. }) | None => {
                return Err(Status::failed_precondition(
                    "LLM gateway does not use controller JWT authentication",
                ));
            }
        };
        if !allowed_client_ids.contains(&client_id) {
            warn!(
                device_id,
                client_id, "rejected disallowed LLM gateway client"
            );
            return Err(Status::permission_denied("client_id is not allowed"));
        }
        let subscription_requested = subscription;
        let subscription = subscription_requested
            && subscription_is_configured_for_client(&config.programs, &client_id);
        if subscription_requested && !subscription {
            warn!(
                device_id,
                client_id,
                "ignored subscription billing claim without subscription auth configuration"
            );
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
                subscription,
            )
            .map_err(internal)?;
        info!(
            device_id,
            expires_at_unix_seconds,
            enrolled_by_issuer = principal.issuer,
            client_id,
            subscription,
            "issued LLM gateway credential"
        );
        Ok(Response::new(LlmGatewayCredentialResponse {
            credential,
            expires_at_unix_seconds,
        }))
    }

    async fn get_device_usage(
        &self,
        request: Request<DeviceUsageRequest>,
    ) -> Result<Response<DeviceUsageResponse>, Status> {
        let device_id = self.authenticate_device(&request).await?;
        agentdesktop_usage::device_usage(
            &self.database.usage(),
            Some(&device_id),
            request.get_ref(),
            self.usage_enabled,
        )
        .await
        .map(Response::new)
        .map_err(|error| match error {
            UsageError::InvalidQuery(message) => Status::invalid_argument(message),
            UsageError::Storage(error) => internal(error),
        })
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
        completed: &CompletedEnrollment,
        csr_der: &[u8],
        info: RequestInfo,
    ) -> Result<Response<EnrollResponse>, Status> {
        let device_id = Uuid::new_v4().to_string();
        let issuer = self.device_certificate_issuer.as_ref().ok_or_else(|| {
            Status::failed_precondition("device certificate issuance is disabled")
        })?;
        let issued_certificate = match issuer.issue(&device_id, csr_der) {
            Ok(issued) => issued,
            Err(error) => {
                self.record_enrollment_denied(completed, "invalid_certificate_request", &info)
                    .await;
                return Err(invalid_csr(error));
            }
        };
        let actor = Actor::oidc_user(&completed.issuer, &completed.subject, &completed.idp_claims);
        let event = |kind, outcome| {
            AuditEvent::new(kind, actor.clone(), outcome)
                .action(ENROLL_DEVICE)
                .target(DEVICES, Some(&device_id))
                .target_name(&completed.hostname)
                .request(info.clone())
        };
        // Nothing is admitted until the attempt is recorded (fail closed).
        if let Err(error) = self
            .audit
            .record(event(EventKind::ActionAttempted, Outcome::Authorized))
            .await
        {
            error!(error = format!("{error:#}"), %device_id,
                "refusing device enrollment that could not be audited");
            return Err(audit_unavailable());
        }
        // The insert and its completion record run in their own task, so a client
        // that disconnects cannot drop the handler between them.
        let stored = tokio::spawn({
            let database = self.database.clone();
            let audit = self.audit.clone();
            let completed = completed.clone();
            let device_id = device_id.clone();
            let completion = event(EventKind::ActionCompleted, Outcome::Succeeded);
            async move {
                let stored = database
                    .enroll_device(
                        &device_id,
                        &completed.hostname,
                        &completed.issuer,
                        &completed.subject,
                        Some(&completed.idp_claims),
                    )
                    .await;
                let completion = AuditEvent {
                    outcome: if stored.is_ok() {
                        Outcome::Succeeded
                    } else {
                        Outcome::Failed
                    },
                    ..completion
                };
                if let Err(error) = audit.record(completion).await {
                    error!(error = format!("{error:#}"), %device_id,
                        "failed to record device enrollment completion");
                }
                stored
            }
        })
        .await
        .map_err(|error| internal(error.into()))?;
        stored.map_err(internal)?;
        info!(
            %device_id,
            hostname = completed.hostname,
            enrolled_by_issuer = completed.issuer,
            enrolled_by_subject = completed.subject,
            "enrolled device"
        );

        Ok(Response::new(EnrollResponse {
            device_id,
            client_certificate_pem: issued_certificate.chain_pem,
            client_certificate_expires_at_unix_seconds: issued_certificate.expires_at_unix_seconds,
        }))
    }

    /// Records a refused enrollment of a verified user. Best effort: the
    /// enrollment is refused either way.
    async fn record_enrollment_denied(
        &self,
        attempt: &CompletedEnrollment,
        reason: &'static str,
        info: &RequestInfo,
    ) {
        let denied = AuditEvent::new(
            EventKind::DeviceEnrollmentDenied,
            Actor::oidc_user(&attempt.issuer, &attempt.subject, &attempt.idp_claims),
            Outcome::Denied,
        )
        .target(DEVICES, None)
        .target_name(&attempt.hostname)
        .reason(reason)
        .request(info.clone());
        if let Err(error) = self.audit.record(denied).await {
            error!(
                error = format!("{error:#}"),
                subject = attempt.subject,
                reason,
                "failed to record denied device enrollment"
            );
        }
    }

    /// Issues a renewed certificate to an authenticated device, failing closed if
    /// the renewal cannot be audited.
    async fn renew_certificate(
        &self,
        device_id: &str,
        csr_der: &[u8],
        info: RequestInfo,
    ) -> Result<DeviceCertificateResponse, Status> {
        let issuer = self.device_certificate_issuer.as_ref().ok_or_else(|| {
            Status::failed_precondition("device certificate issuance is disabled")
        })?;
        let issued = issuer.issue(device_id, csr_der).map_err(invalid_csr)?;
        let renewed = AuditEvent::new(
            EventKind::DeviceCertificateRenewed,
            Actor::device(device_id),
            Outcome::Succeeded,
        )
        .target(DEVICES, Some(device_id))
        .request(info);
        if let Err(error) = self.audit.record(renewed).await {
            error!(
                error = format!("{error:#}"),
                device_id, "refusing certificate renewal that could not be audited"
            );
            return Err(audit_unavailable());
        }
        info!(device_id, "renewed device certificate");
        Ok(DeviceCertificateResponse {
            client_certificate_pem: issued.chain_pem,
            expires_at_unix_seconds: issued.expires_at_unix_seconds,
        })
    }
}

fn grpc_request_info<T>(request: &Request<T>, path: &'static str) -> RequestInfo {
    RequestInfo::new(
        &axum::http::Method::POST,
        &axum::http::Uri::from_static(path),
        request.metadata().as_ref(),
        request.remote_addr(),
    )
}

fn audit_unavailable() -> Status {
    Status::unavailable("audit_unavailable")
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
            store_config_status(database, device_id, &status).await?;
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

/// Stores a device's configuration status. A per-program report the
/// controller rejects must not break the stream (the agent would reconnect,
/// receive the configuration again and send the same report), so the
/// device-wide status is stored and the last accepted program rows stay.
/// Database errors are returned as they are.
async fn store_config_status(
    database: &Database,
    device_id: &str,
    status: &ConfigStatus,
) -> anyhow::Result<()> {
    if let Err(error) = crate::database::validate_program_status(status) {
        warn!(
            device_id,
            error = %format!("{error:#}"),
            "rejected per-program configuration status; storing the device status and keeping the last accepted program status"
        );
        return database.update_device_config_state(device_id, status).await;
    }
    database.update_config_status(device_id, status).await
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

    use super::{
        COMPLETE_ENROLLMENT, FleetAgentService, RENEW_DEVICE_CERTIFICATE,
        device_id_from_certificate, subscription_is_configured_for_client,
    };
    use crate::{
        audit::{AuditLog, RequestInfo},
        daemon_config::DaemonConfigStore,
        database::Database,
        device_ca::DeviceCertificateIssuer,
        oidc::{CompletedEnrollment, OidcPrincipal},
    };

    async fn audit_database(name: &str) -> Database {
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-{name}-{}-{}.db",
            std::process::id(),
            rand::random::<u64>()
        ));
        Database::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("connect database")
    }

    fn certificate_issuer() -> DeviceCertificateIssuer {
        use rcgen::{BasicConstraints, IsCa, KeyUsagePurpose};
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let ca = ca_params.self_signed(&ca_key).unwrap();
        DeviceCertificateIssuer::from_pem(ca.pem(), &ca_key.serialize_pem()).unwrap()
    }

    fn certificate_request() -> Vec<u8> {
        CertificateParams::new(vec!["ignored.example".to_owned()])
            .unwrap()
            .serialize_request(&KeyPair::generate().unwrap())
            .unwrap()
            .der()
            .to_vec()
    }

    fn signed_in_user() -> CompletedEnrollment {
        CompletedEnrollment {
            hostname: "dev-mac".into(),
            issuer: "https://id.example".into(),
            subject: "user-1".into(),
            idp_claims: [("email".to_owned(), "dev@example.com".into())].into(),
        }
    }

    fn grpc_request(path: &'static str) -> RequestInfo {
        RequestInfo::new(
            &axum::http::Method::POST,
            &axum::http::Uri::from_static(path),
            &axum::http::HeaderMap::new(),
            None,
        )
    }

    fn audited_service(
        database: &Database,
    ) -> (
        FleetAgentService,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ) {
        let (audit, lines) = AuditLog::in_memory(database.clone());
        let service = FleetAgentService::new(
            None,
            database.clone(),
            DaemonConfigStore::new(None),
            None,
            Some(certificate_issuer()),
        )
        .with_audit(audit);
        (service, lines)
    }

    fn streamed(lines: &std::sync::Mutex<Vec<String>>) -> Vec<serde_json::Value> {
        lines
            .lock()
            .unwrap()
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn enrollment_is_audited_with_the_signed_in_user_and_new_device() {
        let database = audit_database("audit-enroll").await;
        let (service, lines) = audited_service(&database);
        let device_id = service
            .enroll_device(
                &signed_in_user(),
                &certificate_request(),
                grpc_request(COMPLETE_ENROLLMENT),
            )
            .await
            .unwrap()
            .into_inner()
            .device_id;
        let events = streamed(&lines);
        let [attempted, completed] = &events[..] else {
            panic!("expected attempted and completed events: {events:?}");
        };
        assert_eq!(attempted["event"], "action.attempted");
        assert_eq!(attempted["outcome"], "authorized");
        assert_eq!(completed["event"], "action.completed");
        assert_eq!(completed["outcome"], "succeeded");
        assert_eq!(attempted["request"]["id"], completed["request"]["id"]);
        for event in [attempted, completed] {
            assert_eq!(event["action"], "devices.enroll");
            assert_eq!(event["actor"]["kind"], "human");
            assert_eq!(event["actor"]["issuer"], "https://id.example");
            assert_eq!(event["actor"]["subject"], "user-1");
            assert_eq!(event["actor"]["email"], "dev@example.com");
            assert_eq!(
                event["target"],
                serde_json::json!({ "class": "devices", "id": device_id, "name": "dev-mac" })
            );
            assert_eq!(event["request"]["path"], COMPLETE_ENROLLMENT);
        }
        assert!(database.get_device(&device_id).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn enrollment_fails_closed_when_it_cannot_be_audited() {
        let database = audit_database("audit-enroll-closed").await;
        let service = FleetAgentService::new(
            None,
            database.clone(),
            DaemonConfigStore::new(None),
            None,
            Some(certificate_issuer()),
        )
        .with_audit(AuditLog::broken_stream(database.clone()));
        let status = service
            .enroll_device(
                &signed_in_user(),
                &certificate_request(),
                grpc_request(COMPLETE_ENROLLMENT),
            )
            .await
            .expect_err("an unaudited enrollment must be refused");
        assert_eq!(status.code(), Code::Unavailable);
        assert!(database.list_devices().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_rejected_certificate_request_is_audited_as_a_denied_enrollment() {
        let database = audit_database("audit-enroll-denied").await;
        let (service, lines) = audited_service(&database);
        let status = service
            .enroll_device(
                &signed_in_user(),
                b"not a certificate request",
                grpc_request(COMPLETE_ENROLLMENT),
            )
            .await
            .expect_err("an invalid certificate request must be refused");
        assert_eq!(status.code(), Code::InvalidArgument);
        let events = streamed(&lines);
        let [denied] = &events[..] else {
            panic!("expected one event: {events:?}");
        };
        assert_eq!(denied["event"], "device.enrollment_denied");
        assert_eq!(denied["outcome"], "denied");
        assert_eq!(denied["reason"], "invalid_certificate_request");
        assert_eq!(denied["actor"]["subject"], "user-1");
        assert_eq!(
            denied["target"],
            serde_json::json!({ "class": "devices", "id": null, "name": "dev-mac" })
        );
        assert!(database.list_devices().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn certificate_renewal_is_audited_with_the_device_as_actor() {
        let database = audit_database("audit-renew").await;
        let (service, lines) = audited_service(&database);
        let device_id = "7ca03414-bb20-4c80-98ef-7b0538b988ba";
        service
            .renew_certificate(
                device_id,
                &certificate_request(),
                grpc_request(RENEW_DEVICE_CERTIFICATE),
            )
            .await
            .unwrap();
        let events = streamed(&lines);
        let [renewed] = &events[..] else {
            panic!("expected one event: {events:?}");
        };
        assert_eq!(renewed["event"], "device.certificate_renewed");
        assert_eq!(renewed["actor"]["kind"], "device");
        assert_eq!(renewed["actor"]["subject"], device_id);
        assert_eq!(renewed["target"]["id"], device_id);
        assert_eq!(renewed["request"]["path"], RENEW_DEVICE_CERTIFICATE);

        let unaudited = FleetAgentService::new(
            None,
            database.clone(),
            DaemonConfigStore::new(None),
            None,
            Some(certificate_issuer()),
        )
        .with_audit(AuditLog::broken_stream(database.clone()));
        let status = unaudited
            .renew_certificate(
                device_id,
                &certificate_request(),
                grpc_request(RENEW_DEVICE_CERTIFICATE),
            )
            .await
            .expect_err("an unaudited renewal must be refused");
        assert_eq!(status.code(), Code::Unavailable);
    }

    #[test]
    fn subscription_billing_requires_server_configured_program_auth() {
        let config = agentdesktop_core::config::parse_daemon(
            "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: oidc\n    issuer: https://issuer.example.com\n    clientId: desktop\nprograms:\n  claudeCode:\n    auth: subscription\n",
        )
        .unwrap();
        assert!(subscription_is_configured_for_client(
            &config.programs,
            "claude-code"
        ));
        assert!(!subscription_is_configured_for_client(
            &config.programs,
            "claude-desktop"
        ));
        assert!(!subscription_is_configured_for_client(
            &config.programs,
            "codex"
        ));
        let codex = agentdesktop_core::config::parse_daemon(
            "llmGateway:\n  url: https://gateway.example.com\n  proxyUrl: https://gateway.example.com/codex\n  authentication:\n    type: controllerJwt\n    audience: gateway\n    allowedClientIds: [codex]\nprograms:\n  codex:\n    auth: subscription\n",
        )
        .unwrap();
        assert!(subscription_is_configured_for_client(
            &codex.programs,
            "codex"
        ));
        assert!(!subscription_is_configured_for_client(
            &codex.programs,
            "claude-code"
        ));
    }

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

    #[tokio::test]
    async fn a_rejected_program_report_keeps_the_device_status() {
        use agentdesktop_proto::fleet::{ConfigState, ConfigStatus, ProgramState, ProgramStatus};
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-rejected-report-{}-{}.db",
            std::process::id(),
            rand::random::<u64>()
        ));
        let database = Database::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .unwrap();
        database
            .enroll_device("device", "host", "issuer", "subject", None)
            .await
            .unwrap();
        let accepted = ConfigStatus {
            revision: 2,
            state: ConfigState::Applied.into(),
            error: String::new(),
            programs: vec![ProgramStatus {
                program: "claude-code".to_owned(),
                state: ProgramState::Applied.into(),
                detail: String::new(),
            }],
            programs_reported: true,
        };
        super::store_config_status(&database, "device", &accepted)
            .await
            .unwrap();
        let status = ConfigStatus {
            revision: 3,
            state: ConfigState::Failed.into(),
            error: "refusing to change VS Code settings".to_owned(),
            programs: vec![ProgramStatus {
                program: "Not Valid".to_owned(),
                state: ProgramState::Conflict.into(),
                detail: String::new(),
            }],
            programs_reported: true,
        };
        super::store_config_status(&database, "device", &status)
            .await
            .expect("a rejected program report does not fail the stream");
        let device = database.get_device("device").await.unwrap().unwrap();
        assert_eq!(device.device.config_revision, Some(3));
        assert_eq!(device.device.config_state, Some(2));
        assert_eq!(
            device
                .programs
                .iter()
                .map(|program| program.program.as_str())
                .collect::<Vec<_>>(),
            vec!["claude-code"],
            "the last accepted program rows stay"
        );
        assert_eq!(device.programs_reported, Some(true));
        drop(database);
        let _ = std::fs::remove_file(path);
    }
}
