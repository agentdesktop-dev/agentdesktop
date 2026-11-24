use std::{ops::ControlFlow, pin::Pin, time::Duration};

use agentdesktop_proto::fleet::{
    AgentMessage, ConfigStatus, ControllerMessage, DaemonConfig, DeviceCertificateResponse,
    DeviceUsageRequest, DeviceUsageResponse, EnrollDeviceRequest, EnrollResponse,
    LlmGatewayCredentialRequest, LlmGatewayCredentialResponse, LoginConfiguration,
    LoginConfigurationRequest, RenewDeviceCertificateRequest, agent_message, controller_message,
    enroll_device_request::Credential, fleet_agent_server::FleetAgent,
};
use futures_core::Stream;
use sha2::{Digest, Sha256};
use tokio::{
    sync::{mpsc, watch},
    time,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, error, info, warn};
use uuid::Uuid;
use x509_parser::{extensions::GeneralName, parse_x509_certificate};

use agentdesktop_core::config::{LlmGatewayAuthentication, ProgramsConfig};
use agentdesktop_usage::UsageError;

use crate::{
    daemon_config::DaemonConfigStore,
    database::{Database, DevicePrincipal},
    device_ca::DeviceCertificateIssuer,
    enrollment_token::EnrollmentTokens,
    fleet_limits::{
        DeviceStream, DeviceStreams, EnrollmentLimiter, FleetLimits, StreamEvent, ThrottledWarning,
    },
    gateway_jwt::GatewayJwtIssuer,
    oidc::{CompletedEnrollment, EnrollmentCredential, OidcPrincipal, OidcProvider},
};

#[derive(Clone)]
pub struct FleetAgentService {
    oidc: Option<OidcProvider>,
    enrollment_tokens: Option<EnrollmentTokens>,
    database: Database,
    daemon_config: DaemonConfigStore,
    gateway_jwt_issuer: Option<GatewayJwtIssuer>,
    device_certificate_issuer: Option<DeviceCertificateIssuer>,
    usage_enabled: bool,
    enrollment: EnrollmentLimiter,
    device_streams: DeviceStreams,
}

impl FleetAgentService {
    /// `limits` sets the enrollment and `Connect` limits; pass the ones from
    /// the controller configuration (`FleetLimits::from_config`), the same the
    /// fleet listener uses.
    pub fn new(
        oidc: Option<OidcProvider>,
        database: Database,
        daemon_config: DaemonConfigStore,
        gateway_jwt_issuer: Option<GatewayJwtIssuer>,
        device_certificate_issuer: Option<DeviceCertificateIssuer>,
        limits: &FleetLimits,
    ) -> Self {
        Self {
            oidc,
            enrollment_tokens: None,
            database,
            daemon_config,
            gateway_jwt_issuer,
            device_certificate_issuer,
            usage_enabled: false,
            enrollment: EnrollmentLimiter::new(limits),
            device_streams: DeviceStreams::new(limits),
        }
    }

    /// Accepts operator-signed enrollment tokens in place of a user sign-in.
    pub fn with_enrollment_tokens(mut self, tokens: Option<EnrollmentTokens>) -> Self {
        self.enrollment_tokens = tokens;
        self
    }

    /// Tells devices that the controller collects gateway usage.
    pub fn with_usage_collection(mut self, enabled: bool) -> Self {
        self.usage_enabled = enabled;
        self
    }

    /// The registry of open device streams, shared with this service.
    pub(crate) fn device_streams(&self) -> DeviceStreams {
        self.device_streams.clone()
    }
}

fn subscription_is_configured_for_client(programs: &ProgramsConfig, client_id: &str) -> bool {
    programs.subscription_auth(client_id).is_some()
}

#[tonic::async_trait]
impl FleetAgent for FleetAgentService {
    type ConnectStream = Pin<Box<dyn Stream<Item = Result<ControllerMessage, Status>> + Send>>;

    async fn get_login_configuration(
        &self,
        request: Request<LoginConfigurationRequest>,
    ) -> Result<Response<LoginConfiguration>, Status> {
        let _admitted = self.enrollment.admit(request.remote_addr())?;
        let oidc = self
            .oidc
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("OIDC enrollment is disabled"))?;
        Ok(Response::new(oidc.login_configuration()))
    }

    async fn enroll_device(
        &self,
        request: Request<EnrollDeviceRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        let _admitted = self
            .enrollment
            .admit_enrollment(request.remote_addr())
            .await?;
        let (metadata, _, request) = request.into_parts();
        let csr_der = request.certificate_signing_request_der;
        // Parsing verifies the CSR signature, which proves the caller holds the key
        // that the ID token's nonce must be bound to.
        let csr = rcgen::CertificateSigningRequestParams::from_der(&csr_der.as_slice().into())
            .map_err(|error| invalid_csr(error.into()))?;
        let completed = match request.credential {
            Some(Credential::IdToken(id_token)) => {
                let oidc = self
                    .oidc
                    .as_ref()
                    .ok_or_else(|| Status::failed_precondition("OIDC enrollment is disabled"))?;
                let device_key_nonce = agentdesktop_proto::enrollment_nonce(
                    &rcgen::PublicKeyData::subject_public_key_info(&csr.public_key),
                );
                oidc.authenticate_enrollment(
                    request.hostname,
                    &id_token,
                    bearer_credential(&metadata)?,
                    &device_key_nonce,
                )
                .await
                .map_err(invalid_enrollment)?
            }
            Some(Credential::EnrollmentToken(token)) => {
                self.authenticate_enrollment_token(request.hostname, &token)?
            }
            None => return Err(Status::invalid_argument("missing enrollment credential")),
        };
        self.issue_enrolled_device(&completed, &csr_der).await
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
        // The credential carries the identity of the device's user.
        let user = principal
            .user
            .ok_or_else(|| Status::failed_precondition("sign in to use the LLM gateway"))?;
        let (credential, expires_at_unix_seconds) = issuer
            .issue(
                &user.subject,
                &device_id,
                &client_id,
                audience,
                Some(&user.idp_claims),
                subscription,
            )
            .map_err(internal)?;
        info!(
            device_id,
            expires_at_unix_seconds,
            enrolled_by_issuer = user.issuer,
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
        let device_id = peer_certificate_device_id(&request)?;
        // Rate-limit opens per device before checking the access token, which
        // calls the identity provider.
        let permit = self.device_streams.admit_open(&device_id)?;
        let access = match self
            .authenticate_device_identity(&device_id, request.metadata())
            .await?
        {
            Some(oidc) => Some((
                oidc.clone(),
                bearer_credential(request.metadata())?.to_owned(),
            )),
            None => None,
        };
        let (responses, receiver) = mpsc::channel(8);
        let session = ConnectSession {
            device_id,
            daemon_updates: self.daemon_config.subscribe(),
            stream: permit.open(request),
            responses,
            database: self.database.clone(),
            access,
        };
        tokio::spawn(session.run());
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

/// How often an open `Connect` stream checks its access token again.
const ACCESS_TOKEN_RECHECK: Duration = Duration::from_secs(5 * 60);

/// A device's `Connect` session, run by its own task. The task owns the
/// response stream: it ends when the device's request stream ends, the device
/// goes away, or the session closes the stream for cause. Configuration is
/// sent from the same task, so no other task keeps the response open.
struct ConnectSession {
    device_id: String,
    stream: DeviceStream,
    responses: mpsc::Sender<Result<ControllerMessage, Status>>,
    daemon_updates: watch::Receiver<Option<DaemonConfig>>,
    database: Database,
    /// The identity provider and the access token the stream opened with.
    /// `None` for a device enrolled with a token.
    access: Option<(OidcProvider, String)>,
}

impl ConnectSession {
    async fn run(mut self) {
        // Marked seen, so a change made since the subscription is sent once.
        let current = self.daemon_updates.borrow_and_update().clone();
        if let Some(config) = current
            && self.send_config(config).is_break()
        {
            return;
        }
        let enrolled = match self.database.device_principal(&self.device_id).await {
            Ok(principal) => principal,
            Err(error) => {
                error!(%error, device_id = %self.device_id, "failed to load stream identity");
                return;
            }
        };
        let mut recheck = time::interval(ACCESS_TOKEN_RECHECK);
        recheck.tick().await;
        loop {
            let next = tokio::select! {
                _ = self.responses.closed() => ControlFlow::Break(()),
                changed = self.daemon_updates.changed() => match changed {
                    Ok(()) => self.forward_config(),
                    Err(_) => ControlFlow::Break(()),
                },
                _ = recheck.tick() => self.recheck_access_token(&enrolled).await,
                // Waits while the device is over its message budget; the other
                // branches keep running.
                event = self.stream.next() => self.receive(event).await,
            };
            if next.is_break() {
                break;
            }
        }
    }

    /// Sends the device the configuration that just changed.
    fn forward_config(&mut self) -> ControlFlow<()> {
        let config = self.daemon_updates.borrow_and_update().clone();
        match config {
            Some(config) => self.send_config(config),
            None => ControlFlow::Continue(()),
        }
    }

    /// Sends `config` to the device without waiting for room: a device that
    /// stopped reading would hold the session forever.
    fn send_config(&self, config: DaemonConfig) -> ControlFlow<()> {
        let message = ControllerMessage {
            message: Some(controller_message::Message::DaemonConfig(config)),
        };
        match self.responses.try_send(Ok(message)) {
            Ok(()) => ControlFlow::Continue(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!(device_id = %self.device_id, "closing stream after the device stopped reading it");
                ControlFlow::Break(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => ControlFlow::Break(()),
        }
    }

    /// Checks the stream's access token again, and closes the stream unless it
    /// still identifies the user who enrolled the device. The stream stays open
    /// while the identity provider is unavailable.
    async fn recheck_access_token(&self, enrolled: &DevicePrincipal) -> ControlFlow<()> {
        let Some((oidc, access_token)) = &self.access else {
            return ControlFlow::Continue(());
        };
        match oidc.authenticate_access_token(access_token).await {
            Ok(principal) if oidc_principals_match(&principal, enrolled) => {
                ControlFlow::Continue(())
            }
            Ok(_) => {
                warn!(device_id = %self.device_id, "closing stream after OIDC principal changed");
                self.close(Status::permission_denied("OIDC principal changed"))
            }
            Err(error) => {
                let status = access_token_status(&error);
                if status.code() == tonic::Code::Unavailable {
                    // Every open stream rechecks, so an outage would otherwise
                    // log a line per stream.
                    if let Some(retained) = RECHECK_UNAVAILABLE.event(time::Instant::now()) {
                        warn!(latest_error = %error, latest_device_id = %self.device_id, retained_since_last_warning = retained, "OIDC unavailable during stream revalidation; retaining streams");
                    }
                    return ControlFlow::Continue(());
                }
                warn!(%error, device_id = %self.device_id, "closing stream after OIDC access token revalidation failed");
                self.close(status)
            }
        }
    }

    async fn receive(&self, event: StreamEvent<AgentMessage>) -> ControlFlow<()> {
        match event {
            StreamEvent::Message(message) => self.store(message).await,
            StreamEvent::Superseded => {
                info!(device_id = %self.device_id, "closing stream replaced by a newer stream from the same device");
                self.close(Status::aborted(
                    "replaced by a newer stream from this device",
                ))
            }
            StreamEvent::Ended => ControlFlow::Break(()),
            StreamEvent::Failed(error) => {
                warn!(%error, device_id = %self.device_id, "device stream failed");
                ControlFlow::Break(())
            }
        }
    }

    /// Stores a message from the device. A hello must name the device that
    /// the certificate identifies.
    async fn store(&self, message: AgentMessage) -> ControlFlow<()> {
        if let Some(agent_message::Message::Hello(hello)) = &message.message
            && hello.device_id != self.device_id
        {
            warn!(
                authenticated_device_id = self.device_id.as_str(),
                claimed_device_id = %hello.device_id,
                "device certificate and hello identity do not match"
            );
            return self.close(Status::permission_denied(
                "hello does not match the device certificate",
            ));
        }
        if let Err(error) = handle_agent_message(&self.database, &self.device_id, message).await {
            error!(%error, device_id = %self.device_id, "failed to store device state");
            return ControlFlow::Break(());
        }
        ControlFlow::Continue(())
    }

    /// Ends the stream with `status`, dropped if the device is not reading.
    fn close(&self, status: Status) -> ControlFlow<()> {
        let _ = self.responses.try_send(Err(status));
        ControlFlow::Break(())
    }
}

impl FleetAgentService {
    async fn authenticate_device<T>(&self, request: &Request<T>) -> Result<String, Status> {
        let device_id = peer_certificate_device_id(request)?;
        self.authenticate_device_identity(&device_id, request.metadata())
            .await?;
        Ok(device_id)
    }

    /// Authenticates `device_id`. A device enrolled with a token needs only
    /// its certificate. Any other device also needs the access token of the
    /// user who enrolled it; the identity provider that checked it is returned.
    async fn authenticate_device_identity(
        &self,
        device_id: &str,
        metadata: &tonic::metadata::MetadataMap,
    ) -> Result<Option<&OidcProvider>, Status> {
        let enrolled = self.enrolled_principal(device_id).await?;
        if let Some(token) = &enrolled.enrollment_token {
            let accepted = self
                .enrollment_tokens
                .as_ref()
                .is_some_and(|tokens| tokens.accepts(token));
            if !accepted {
                warn!(device_id, "device enrollment token is no longer accepted");
                return Err(Status::unauthenticated(
                    "device enrollment token is no longer accepted",
                ));
            }
            return Ok(None);
        }
        let oidc = self.oidc.as_ref().ok_or_else(|| {
            Status::failed_precondition("OIDC access-token validation is disabled")
        })?;
        let access_principal = oidc
            .authenticate_access_token(bearer_credential(metadata)?)
            .await
            .map_err(invalid_access_token)?;
        authenticate_enrolled_user(&enrolled, &access_principal)?;
        Ok(Some(oidc))
    }

    fn authenticate_enrollment_token(
        &self,
        hostname: String,
        token: &str,
    ) -> Result<CompletedEnrollment, Status> {
        let tokens = self
            .enrollment_tokens
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("enrollment tokens are disabled"))?;
        crate::oidc::validate_hostname(&hostname)
            .map_err(|_| Status::invalid_argument("invalid enrollment hostname"))?;
        let token = tokens.verify(token).map_err(|error| {
            warn!(error = %format!("{error:#}"), "enrollment token rejected");
            Status::unauthenticated("enrollment token rejected")
        })?;
        Ok(CompletedEnrollment {
            hostname,
            credential: EnrollmentCredential::Token(token),
        })
    }

    async fn enrolled_principal(&self, device_id: &str) -> Result<DevicePrincipal, Status> {
        self.database
            .device_principal(device_id)
            .await
            .map_err(|error| {
                warn!(%error, device_id, "certificate references an unknown device");
                Status::unauthenticated("unrecognized device certificate")
            })
    }

    async fn issue_enrolled_device(
        &self,
        completed: &CompletedEnrollment,
        csr_der: &[u8],
    ) -> Result<Response<EnrollResponse>, Status> {
        let device_id = match &completed.credential {
            EnrollmentCredential::User(user) => {
                enrollment_device_id(&user.issuer, &user.subject, csr_der)
            }
            EnrollmentCredential::Token(token) => enrollment_device_id(
                &format!("enrollment-token:{}", token.issuer),
                &token.id,
                csr_der,
            ),
        };
        if let Some(response) = self
            .database
            .enrollment_receipt(&device_id)
            .await
            .map_err(internal)?
        {
            return Ok(Response::new(response));
        }
        let issued_certificate = self
            .device_certificate_issuer
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("device certificate issuance is disabled"))?
            .issue(&device_id, csr_der)
            .map_err(invalid_csr)?;
        let response = self
            .database
            .finish_enrollment(
                completed,
                &EnrollResponse {
                    device_id: device_id.clone(),
                    client_certificate_pem: issued_certificate.chain_pem,
                    client_certificate_expires_at_unix_seconds: issued_certificate
                        .expires_at_unix_seconds,
                },
            )
            .await
            .map_err(internal)?;
        match &completed.credential {
            EnrollmentCredential::User(user) => info!(
                %device_id,
                hostname = %completed.hostname,
                enrolled_by_issuer = %user.issuer,
                enrolled_by_subject = %user.subject,
                "enrolled device"
            ),
            EnrollmentCredential::Token(token) => info!(
                %device_id,
                hostname = %completed.hostname,
                enrollment_token_issuer = %token.issuer,
                enrollment_token_id = %token.id,
                "enrolled device"
            ),
        }

        Ok(Response::new(response))
    }
}

fn enrollment_device_id(issuer: &str, subject: &str, csr_der: &[u8]) -> String {
    let mut hash = Sha256::new();
    for part in [issuer.as_bytes(), subject.as_bytes(), csr_der] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    let digest = hash.finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(bytes).to_string()
}

fn authenticate_enrolled_user(
    enrolled: &DevicePrincipal,
    access_principal: &OidcPrincipal,
) -> Result<(), Status> {
    if !oidc_principals_match(access_principal, enrolled) {
        return Err(Status::permission_denied(
            "access token principal does not match enrolled device user",
        ));
    }
    Ok(())
}

fn oidc_principals_match(authenticated: &OidcPrincipal, enrolled: &DevicePrincipal) -> bool {
    enrolled.user.as_ref().is_some_and(|user| {
        authenticated.issuer == user.issuer && authenticated.subject == user.subject
    })
}

fn peer_certificate_device_id<T>(request: &Request<T>) -> Result<String, Status> {
    let certificates = request
        .peer_certs()
        .ok_or_else(|| Status::unauthenticated("missing device certificate"))?;
    let certificate = certificates
        .first()
        .ok_or_else(|| Status::unauthenticated("missing device certificate"))?;
    device_id_from_certificate(certificate.as_ref())
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
    if identity_provider_unavailable(&err) {
        // The agent retries this with the same sign-in instead of starting over.
        return Status::unavailable("OIDC identity provider is unavailable");
    }
    Status::unauthenticated("OIDC enrollment failed")
}

fn invalid_csr(err: anyhow::Error) -> Status {
    warn!(error = %err, "device certificate request failed");
    Status::invalid_argument("invalid certificate signing request")
}

/// Stream rechecks that kept a stream open because the identity provider was
/// unavailable.
static RECHECK_UNAVAILABLE: ThrottledWarning = ThrottledWarning::new();

fn invalid_access_token(err: anyhow::Error) -> Status {
    warn!(error = %err, "OIDC access token rejected");
    access_token_status(&err)
}

/// The status for an access token that could not be checked: `UNAVAILABLE`
/// when the identity provider could not be used, `UNAUTHENTICATED`
/// otherwise.
fn access_token_status(err: &anyhow::Error) -> Status {
    if identity_provider_unavailable(err) {
        return Status::unavailable("OIDC access-token validation is unavailable");
    }
    Status::unauthenticated("OIDC access token rejected")
}

/// Whether `err` means the identity provider could not be used, rather than that
/// it rejected the credential: any failed request other than 401 or 403.
fn identity_provider_unavailable(err: &anyhow::Error) -> bool {
    if err
        .chain()
        .any(|cause| cause.is::<crate::oidc::IdentityProviderUnavailable>())
    {
        return true;
    }
    err.chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
        .is_some_and(|error| {
            !matches!(
                error.status(),
                Some(reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN)
            )
        })
}

#[cfg(test)]
mod tests {
    use rcgen::{CertificateParams, KeyPair};
    use tonic::Code;

    use super::{
        FleetAgentService, authenticate_enrolled_user, device_id_from_certificate,
        subscription_is_configured_for_client,
    };
    use crate::{daemon_config::DaemonConfigStore, database::Database, oidc::OidcPrincipal};

    #[tokio::test]
    async fn enrollment_rpc_is_user_bound_idempotent_and_durable() {
        use crate::oidc::tests::{authenticated_provider, id_token, user_claims};
        use agentdesktop_proto::fleet::{
            EnrollDeviceRequest, LoginConfigurationRequest, enroll_device_request::Credential,
            fleet_agent_server::FleetAgent,
        };
        use rcgen::{BasicConstraints, IsCa, KeyUsagePurpose};

        let path = std::env::temp_dir().join(format!(
            "agentdesktop-enrollment-{}-{}.db",
            std::process::id(),
            rand::random::<u64>(),
        ));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let database = Database::connect(&url).await.unwrap();
        let (oidc, signing_key, idp) = authenticated_provider().await;
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let issuer =
            crate::device_ca::DeviceCertificateIssuer::from_pem(ca.pem(), &ca_key.serialize_pem())
                .unwrap();
        let service = FleetAgentService::new(
            Some(oidc.clone()),
            database.clone(),
            DaemonConfigStore::new(None),
            None,
            Some(issuer.clone()),
            &crate::fleet_limits::FleetLimits::from_config(&Default::default())
                .expect("default limits"),
        );
        for _ in 0..1100 {
            service
                .get_login_configuration(tonic::Request::new(LoginConfigurationRequest {}))
                .await
                .unwrap();
        }
        let counts = || async {
            let (devices,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM devices")
                .fetch_one(database.pool())
                .await
                .unwrap();
            let (receipts,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM enrollment_receipts")
                .fetch_one(database.pool())
                .await
                .unwrap();
            (devices, receipts)
        };
        assert_eq!(
            counts().await,
            (0, 0),
            "configuration reads must allocate no enrollment state"
        );
        let device_key = KeyPair::generate().unwrap();
        let csr = CertificateParams::new(Vec::<String>::new())
            .unwrap()
            .serialize_request(&device_key)
            .unwrap();
        let key_nonce = |key: &KeyPair| {
            agentdesktop_proto::enrollment_nonce(&rcgen::PublicKeyData::subject_public_key_info(
                key,
            ))
        };
        let body = EnrollDeviceRequest {
            hostname: "workstation".to_owned(),
            certificate_signing_request_der: csr.der().as_ref().to_vec(),
            credential: Some(Credential::IdToken(id_token(
                &signing_key,
                &user_claims(&key_nonce(&device_key)),
            ))),
        };
        let request_with = |token: &str, body: EnrollDeviceRequest| {
            let mut request = tonic::Request::new(body);
            request
                .metadata_mut()
                .insert("authorization", format!("Bearer {token}").parse().unwrap());
            request
        };
        let request = |body: EnrollDeviceRequest| request_with("access-token", body);
        assert_eq!(
            service
                .enroll_device(tonic::Request::new(body.clone()))
                .await
                .unwrap_err()
                .code(),
            Code::Unauthenticated,
        );
        let mut invalid = body.clone();
        invalid.credential = Some(Credential::IdToken("invalid".to_owned()));
        assert_eq!(
            service
                .enroll_device(request(invalid))
                .await
                .unwrap_err()
                .code(),
            Code::Unauthenticated
        );
        let mut invalid = body.clone();
        invalid.certificate_signing_request_der = vec![1, 2, 3];
        assert_eq!(
            service
                .enroll_device(request(invalid))
                .await
                .unwrap_err()
                .code(),
            Code::InvalidArgument
        );
        let mut other_key_sign_in = body.clone();
        other_key_sign_in.credential = Some(Credential::IdToken(id_token(
            &signing_key,
            &user_claims("other-key-nonce"),
        )));
        assert_eq!(
            service
                .enroll_device(request(other_key_sign_in))
                .await
                .unwrap_err()
                .code(),
            Code::Unauthenticated,
            "a sign-in bound to another key must not enroll this one"
        );
        assert_eq!(
            service
                .enroll_device(request_with("flaky", body.clone()))
                .await
                .unwrap_err()
                .code(),
            Code::Unavailable,
            "an IdP failure must be retryable instead of sending the user back to sign-in"
        );
        assert_eq!(counts().await, (0, 0));
        let response = service
            .enroll_device(request_with("flaky", body.clone()))
            .await
            .unwrap()
            .into_inner();
        let principal = database
            .device_principal(&response.device_id)
            .await
            .unwrap();
        let user = principal.user.unwrap();
        assert_eq!(user.issuer, "https://idp.example");
        assert_eq!(user.subject, "user-1");
        assert_eq!(counts().await, (1, 1));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let service = service.clone();
            let request = request(body.clone());
            tasks.spawn(async move { service.enroll_device(request).await.unwrap().into_inner() });
        }
        while let Some(result) = tasks.join_next().await {
            assert_eq!(result.unwrap(), response);
        }
        let reopened = Database::connect(&url).await.unwrap();
        let restarted = FleetAgentService::new(
            Some(oidc),
            reopened,
            DaemonConfigStore::new(None),
            None,
            Some(issuer),
            &crate::fleet_limits::FleetLimits::from_config(&Default::default())
                .expect("default limits"),
        );
        assert_eq!(
            restarted
                .enroll_device(request(body.clone()))
                .await
                .unwrap()
                .into_inner(),
            response,
            "retry after controller restart must recover the original certificate",
        );
        let mut other_body = body;
        let other_key = KeyPair::generate().unwrap();
        let other_csr = CertificateParams::new(Vec::<String>::new())
            .unwrap()
            .serialize_request(&other_key)
            .unwrap();
        other_body.certificate_signing_request_der = other_csr.der().as_ref().to_vec();
        other_body.credential = Some(Credential::IdToken(id_token(
            &signing_key,
            &user_claims(&key_nonce(&other_key)),
        )));
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let service = service.clone();
            let request = request(other_body.clone());
            tasks.spawn(async move { service.enroll_device(request).await.unwrap().into_inner() });
        }
        let first = tasks.join_next().await.unwrap().unwrap();
        assert_ne!(first.device_id, response.device_id);
        while let Some(result) = tasks.join_next().await {
            assert_eq!(
                result.unwrap(),
                first,
                "first-time concurrent enrollment must also deduplicate"
            );
        }
        assert_eq!(counts().await, (2, 2));
        assert!(database.delete_device(&response.device_id).await.unwrap());
        assert!(
            database
                .enrollment_receipt(&response.device_id)
                .await
                .unwrap()
                .is_none(),
            "deleting a device must also remove its retry receipt",
        );
        assert_eq!(counts().await, (1, 1));
        idp.abort();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn a_token_enrolls_a_device_that_then_needs_only_its_certificate() {
        use crate::enrollment_token::tests::Issuer;
        use agentdesktop_proto::fleet::{
            EnrollDeviceRequest, enroll_device_request::Credential, fleet_agent_server::FleetAgent,
        };
        use rcgen::{BasicConstraints, IsCa, KeyUsagePurpose};

        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("controller.db").display()
        );
        let database = Database::connect(&url).await.unwrap();
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let certificates =
            crate::device_ca::DeviceCertificateIssuer::from_pem(ca.pem(), &ca_key.serialize_pem())
                .unwrap();
        let tokens = Issuer::new();
        let service = |revoked_ids: &[&str]| {
            FleetAgentService::new(
                None,
                database.clone(),
                DaemonConfigStore::new(None),
                None,
                Some(certificates.clone()),
                &crate::fleet_limits::FleetLimits::from_config(&Default::default())
                    .expect("default limits"),
            )
            .with_enrollment_tokens(Some(tokens.tokens(revoked_ids)))
        };
        let enroll = |service: FleetAgentService, token: String| async move {
            let csr = CertificateParams::new(Vec::<String>::new())
                .unwrap()
                .serialize_request(&KeyPair::generate().unwrap())
                .unwrap();
            let request = EnrollDeviceRequest {
                hostname: "workstation".to_owned(),
                certificate_signing_request_der: csr.der().as_ref().to_vec(),
                credential: Some(Credential::EnrollmentToken(token)),
            };
            let response = service
                .enroll_device(tonic::Request::new(request.clone()))
                .await?
                .into_inner();
            let retried = service.enroll_device(tonic::Request::new(request)).await?;
            assert_eq!(retried.into_inner(), response, "a retry must be idempotent");
            Ok::<_, tonic::Status>(response.device_id)
        };
        let no_access_token = tonic::metadata::MetadataMap::new();

        let first = enroll(service(&[]), tokens.token("batch-1")).await.unwrap();
        let second = enroll(service(&[]), tokens.token("batch-1")).await.unwrap();
        assert_ne!(first, second, "one token enrolls many devices");
        service(&[])
            .authenticate_device_identity(&first, &no_access_token)
            .await
            .expect("the certificate alone authenticates the device");
        let principal = database.device_principal(&first).await.unwrap();
        assert_eq!(principal.user, None);
        let token = principal.enrollment_token.unwrap();
        assert_eq!(
            (token.issuer.as_str(), token.id.as_str()),
            ("jamf", "batch-1")
        );

        let revoked = service(&["batch-1"]);
        for device_id in [&first, &second] {
            let status = revoked
                .authenticate_device_identity(device_id, &no_access_token)
                .await
                .err()
                .expect("a device enrolled with a revoked token must be rejected");
            assert_eq!(status.code(), Code::Unauthenticated);
        }
        let status = enroll(revoked, tokens.token("batch-1")).await.unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        let without_tokens = service(&[]).with_enrollment_tokens(None);
        let status = without_tokens
            .authenticate_device_identity(&first, &no_access_token)
            .await
            .err()
            .expect("token-enrolled devices need enrollment tokens to stay enabled");
        assert_eq!(status.code(), Code::Unauthenticated);
        let status = enroll(without_tokens, tokens.token("batch-1"))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::FailedPrecondition);
    }

    #[test]
    fn jwks_refresh_cooldown_is_retryable() {
        let error = anyhow::Error::new(crate::oidc::IdentityProviderUnavailable(
            "OIDC JWKS refresh is rate limited",
        ))
        .context("validate enrollment");
        assert_eq!(super::invalid_enrollment(error).code(), Code::Unavailable);
        assert_eq!(
            super::invalid_enrollment(anyhow::anyhow!("OIDC nonce mismatch")).code(),
            Code::Unauthenticated
        );
    }

    #[test]
    fn enrollment_retry_identity_is_bound_to_principal_and_device_key() {
        let id = super::enrollment_device_id("issuer", "user", b"csr");
        assert_eq!(id, super::enrollment_device_id("issuer", "user", b"csr"));
        assert_ne!(id, super::enrollment_device_id("other", "user", b"csr"));
        assert_ne!(id, super::enrollment_device_id("issuer", "other", b"csr"));
        assert_ne!(id, super::enrollment_device_id("issuer", "user", b"other"));
        assert_ne!(
            super::enrollment_device_id("ab", "c", b"csr"),
            super::enrollment_device_id("a", "bc", b"csr"),
        );
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
    async fn enrollment_is_refused_before_any_work_when_the_limiter_is_full() {
        use agentdesktop_proto::fleet::{
            EnrollDeviceRequest, LoginConfigurationRequest, fleet_agent_server::FleetAgent,
        };
        use tonic::{Code, Request};

        use crate::fleet_limits::FleetLimits;

        let path = std::env::temp_dir().join(format!(
            "agentdesktop-enrollment-limit-{}-{}.db",
            std::process::id(),
            rand::random::<u64>()
        ));
        let database = Database::connect(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("connect database");
        let mut full = FleetLimits {
            enrollment_completion_wait: std::time::Duration::from_millis(10),
            ..FleetLimits::from_config(&Default::default()).expect("default limits")
        };
        full.configured.enrollment_in_flight = 0;
        let service = FleetAgentService::new(
            None,
            database,
            DaemonConfigStore::new(None),
            None,
            None,
            &full,
        );

        // Without the limiter these fail later, with "OIDC enrollment is disabled".
        let configuration = service
            .get_login_configuration(Request::new(LoginConfigurationRequest {}))
            .await
            .expect_err("login configuration admitted");
        assert_eq!(
            configuration.code(),
            Code::ResourceExhausted,
            "{configuration}"
        );
        let enrollment = service
            .enroll_device(Request::new(EnrollDeviceRequest::default()))
            .await
            .expect_err("enrollment admitted");
        assert_eq!(enrollment.code(), Code::ResourceExhausted, "{enrollment}");
        let _ = std::fs::remove_file(path);
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
        let service = FleetAgentService::new(
            None,
            database,
            DaemonConfigStore::new(None),
            None,
            None,
            &crate::fleet_limits::FleetLimits::from_config(&Default::default())
                .expect("default limits"),
        );
        let principal = |issuer: &str, subject: &str| OidcPrincipal {
            issuer: issuer.to_owned(),
            subject: subject.to_owned(),
        };
        let enrolled = service.enrolled_principal(device_id).await.unwrap();
        authenticate_enrolled_user(&enrolled, &principal("issuer", "subject"))
            .expect("the enrolled principal is accepted");

        for mismatched in [
            principal("issuer", "other-subject"),
            principal("other-issuer", "subject"),
            principal("other-issuer", "other-subject"),
        ] {
            let status = authenticate_enrolled_user(&enrolled, &mismatched)
                .expect_err("mismatched OIDC principal must fail");
            assert_eq!(status.code(), Code::PermissionDenied);
        }

        let status = service
            .enrolled_principal("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .await
            .err()
            .expect("unknown device certificate must fail");
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
        let service = FleetAgentService::new(
            None,
            database,
            DaemonConfigStore::new(None),
            None,
            None,
            &crate::fleet_limits::FleetLimits::from_config(&Default::default())
                .expect("default limits"),
        );

        let enrolled = service.enrolled_principal(device_id).await.unwrap();
        let status = authenticate_enrolled_user(
            &enrolled,
            &OidcPrincipal {
                issuer: String::new(),
                subject: "subject".to_owned(),
            },
        )
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
