//! Exercise the real session loop against isolated OAuth and gRPC services.
//! Linux uses a temporary file-backed secret store, not a user's Keychain.
use super::*;
use agentdesktop_proto::fleet::{
    fleet_agent_server::{FleetAgent, FleetAgentServer},
    *,
};
use axum::{Form, Json, Router, http::StatusCode, routing::post};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::task::JoinHandle;

type Reply<T> = Result<tonic::Response<T>, tonic::Status>;

#[derive(Debug)]
enum Event {
    Connected { token: String, device_id: String },
    EnrollmentRequested,
    TokenDeviceRejected,
    TokenEnrollmentRequested,
}

#[derive(Clone)]
struct Controller {
    events: mpsc::UnboundedSender<Event>,
    reject_fresh: bool,
    expire_first_fresh: bool,
}

#[tonic::async_trait]
impl FleetAgent for Controller {
    async fn get_login_configuration(
        &self,
        _: Request<LoginConfigurationRequest>,
    ) -> Reply<LoginConfiguration> {
        let _ = self.events.send(Event::EnrollmentRequested);
        Err(tonic::Status::unavailable(
            "interactive enrollment is not part of this test",
        ))
    }
    async fn enroll_device(&self, request: Request<EnrollDeviceRequest>) -> Reply<EnrollResponse> {
        if let Some(enroll_device_request::Credential::EnrollmentToken(_)) =
            request.get_ref().credential
        {
            let _ = self.events.send(Event::TokenEnrollmentRequested);
            return Err(tonic::Status::unauthenticated("enrollment token rejected"));
        }
        Err(tonic::Status::unimplemented("not used"))
    }
    async fn renew_device_certificate(
        &self,
        _: Request<RenewDeviceCertificateRequest>,
    ) -> Reply<DeviceCertificateResponse> {
        Err(tonic::Status::unimplemented("not used"))
    }
    async fn get_llm_gateway_credential(
        &self,
        _: Request<LlmGatewayCredentialRequest>,
    ) -> Reply<LlmGatewayCredentialResponse> {
        Err(tonic::Status::unimplemented("not used"))
    }
    async fn get_device_usage(&self, _: Request<DeviceUsageRequest>) -> Reply<DeviceUsageResponse> {
        Err(tonic::Status::unimplemented("not used"))
    }
    type ConnectStream = ReceiverStream<Result<ControllerMessage, tonic::Status>>;
    async fn connect(
        &self,
        request: Request<tonic::Streaming<AgentMessage>>,
    ) -> Reply<Self::ConnectStream> {
        // A token-enrolled device sends no access token; this controller no
        // longer accepts its token.
        let Some(token) = request.metadata().get("authorization") else {
            let _ = self.events.send(Event::TokenDeviceRejected);
            return Err(tonic::Status::unauthenticated(
                "device enrollment token is no longer accepted",
            ));
        };
        let token = token.to_str().unwrap().to_owned();
        if self.reject_fresh && token.starts_with("Bearer fresh-") {
            return Err(tonic::Status::unauthenticated("device was revoked"));
        }
        let rejected =
            token == "Bearer stale" || (self.expire_first_fresh && token == "Bearer fresh-1");
        let mut inbound = request.into_inner();
        let (sender, receiver) = mpsc::channel(4);
        let events = self.events.clone();
        tokio::spawn(async move {
            let hello = inbound.message().await.unwrap().unwrap();
            let Some(agent_message::Message::Hello(hello)) = hello.message else {
                panic!("expected hello")
            };
            if rejected {
                sender
                    .send(Err(tonic::Status::unauthenticated(
                        "OIDC access token rejected",
                    )))
                    .await
                    .unwrap();
            } else {
                let _ = events.send(Event::Connected {
                    token,
                    device_id: hello.device_id,
                });
                while inbound
                    .message()
                    .await
                    .is_ok_and(|message| message.is_some())
                {}
            }
            drop(sender);
        });
        Ok(tonic::Response::new(ReceiverStream::new(receiver)))
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    events: mpsc::UnboundedReceiver<Event>,
    refreshes: Arc<AtomicUsize>,
    tasks: Vec<JoinHandle<()>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Fixture {
    async fn start(
        statuses: Vec<StatusCode>,
        reject_fresh: bool,
        expire_first_fresh: bool,
    ) -> Self {
        Self::launch(statuses, reject_fresh, expire_first_fresh, false).await
    }

    /// A device enrolled with an enrollment token, which the controller no
    /// longer accepts.
    async fn start_token_enrolled() -> Self {
        Self::launch(vec![], false, false, true).await
    }

    async fn launch(
        statuses: Vec<StatusCode>,
        reject_fresh: bool,
        expire_first_fresh: bool,
        token_enrolled: bool,
    ) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let calls = refreshes.clone();
        let token_app = Router::new().route("/token", post(move |Form(form): Form<std::collections::HashMap<String, String>>| {
            let calls = calls.clone(); let statuses = statuses.clone();
            async move {
                assert_eq!(form.get("grant_type").unwrap(), "refresh_token");
                assert_eq!(form.get("client_id").unwrap(), "test-client");
                let attempt = calls.fetch_add(1, Ordering::SeqCst);
                let status = statuses.get(attempt).copied().unwrap_or(StatusCode::OK);
                if status != StatusCode::OK {
                    return (status, Json(serde_json::json!({"error":"invalid_grant"})));
                }
                (status, Json(serde_json::json!({
                    "access_token":format!("fresh-{}", attempt+1), "refresh_token":format!("rotated-{}", attempt+1),
                    "token_type":"Bearer", "expires_in":3600
                })))
            }
        }));
        let oauth = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let token_endpoint = format!("http://{}/token", oauth.local_addr().unwrap());
        let oauth_task = tokio::spawn(async move {
            axum::serve(oauth, token_app).await.unwrap();
        });
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
        let path = directory.path();
        std::fs::write(path.join("ca.pem"), cert.cert.pem()).unwrap();
        let identity = Identity {
            device_id: "existing-device".to_owned(),
            client_certificate_pem: cert.cert.pem(),
            client_private_key_pem: cert.signing_key.serialize_pem(),
            client_certificate_expires_at_unix_seconds: unix_time_seconds() + 30 * 86400,
            oauth: Some(crate::identity::OAuthCredentials {
                access_token: "stale".to_owned(),
                refresh_token: "refresh-stale".to_owned(),
                // A server rejection must force refresh even before our local deadline.
                expires_at_unix_seconds: unix_time_seconds() + 3600,
            }),
            oauth_token_endpoint: token_endpoint,
            oauth_client_id: "test-client".to_owned(),
        };
        let identity = if token_enrolled {
            Identity {
                oauth: None,
                oauth_token_endpoint: String::new(),
                oauth_client_id: String::new(),
                ..identity
            }
        } else {
            identity
        };
        identity::save(&path.join("identity.json"), &identity).unwrap();
        let enrollment_token_path = token_enrolled.then(|| path.join("enrollment-token"));
        if let Some(token_path) = &enrollment_token_path {
            std::fs::write(token_path, "signed-token").unwrap();
        }
        let (events_sender, events) = mpsc::unbounded_channel();
        let controller = Controller {
            events: events_sender,
            reject_fresh,
            expire_first_fresh,
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server_task = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(FleetAgentServer::new(controller))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
                .await
                .unwrap();
        });
        let controller = ControllerConnectionConfig {
            address: format!("http://{address}"),
            ca_certificate_path: path.join("ca.pem"),
            heartbeat_interval: Duration::from_secs(30),
            device_authorization: false,
            enrollment_token_path,
        };
        let state_dir = path.to_owned();
        let session_task = tokio::spawn(async move {
            let (inventory_sender, mut discovered) = watch::channel(Arc::new(AgentDiscovery {
                agents: vec![],
                model_runtimes: vec![],
            }));
            let (telemetry_sender, mut telemetry) = mpsc::channel(1);
            let (logout_sender, mut logout) = mpsc::channel(1);
            let (current, _current_receiver) = watch::channel(None);
            let mut channels = ConfigChannels {
                current,
                tick_statuses: None,
            };
            let reconciler = super::tests::test_reconciler(&state_dir);
            let _keep_channels_open = (inventory_sender, telemetry_sender, logout_sender);
            run_session(
                &controller,
                &mut discovered,
                &state_dir,
                None,
                false,
                &reconciler,
                &EnrollmentState::new(true),
                &ControllerConnectionState::new(),
                &mut telemetry,
                &mut logout,
                &mut channels,
            )
            .await
            .unwrap();
        });
        Self {
            directory,
            events,
            refreshes,
            tasks: vec![session_task, server_task, oauth_task],
        }
    }
    async fn next_event(&mut self) -> Event {
        time::timeout(Duration::from_secs(10), self.events.recv())
            .await
            .expect("session did not recover")
            .unwrap()
    }
    fn stored(&self) -> Identity {
        identity::load(&self.directory.path().join("identity.json"))
            .unwrap()
            .unwrap()
    }
}

#[tokio::test]
async fn rejected_access_token_refreshes_without_discarding_enrollment() {
    let mut fixture = Fixture::start(vec![], false, false).await;
    let event = fixture.next_event().await;
    assert!(
        matches!(event, Event::Connected { ref token, ref device_id } if token == "Bearer fresh-1" && device_id == "existing-device"),
        "{event:?}"
    );
    assert_eq!(fixture.refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.stored().oauth.unwrap().refresh_token, "rotated-1");
    assert_eq!(fixture.stored().device_id, "existing-device");
}

#[tokio::test]
async fn transient_refresh_failure_preserves_enrollment_until_recovery() {
    let mut fixture = Fixture::start(vec![StatusCode::SERVICE_UNAVAILABLE], false, false).await;
    let event = fixture.next_event().await;
    assert!(
        matches!(event, Event::Connected { ref token, ref device_id } if token == "Bearer fresh-2" && device_id == "existing-device"),
        "{event:?}"
    );
    assert_eq!(fixture.refreshes.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.stored().oauth.unwrap().refresh_token, "rotated-2");
}

#[tokio::test]
async fn rejected_refresh_token_still_requires_enrollment() {
    let mut fixture = Fixture::start(vec![StatusCode::BAD_REQUEST], false, false).await;
    assert!(matches!(
        fixture.next_event().await,
        Event::EnrollmentRequested
    ));
    assert_eq!(fixture.refreshes.load(Ordering::SeqCst), 1);
    assert!(!fixture.directory.path().join("identity.json").exists());
}

#[tokio::test]
async fn rejection_after_refresh_still_requires_enrollment() {
    let mut fixture = Fixture::start(vec![], true, false).await;
    assert!(matches!(
        fixture.next_event().await,
        Event::EnrollmentRequested
    ));
    assert_eq!(
        fixture.refreshes.load(Ordering::SeqCst),
        1,
        "must not refresh forever when a device is revoked"
    );
    assert!(!fixture.directory.path().join("identity.json").exists());
}

#[tokio::test]
async fn accepted_stream_resets_recovery_for_the_next_token_expiry() {
    let mut fixture = Fixture::start(vec![], false, true).await;
    let event = fixture.next_event().await;
    assert!(
        matches!(event, Event::Connected { ref token, ref device_id } if token == "Bearer fresh-2" && device_id == "existing-device"),
        "{event:?}"
    );
    assert_eq!(fixture.refreshes.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.stored().oauth.unwrap().refresh_token, "rotated-2");
}

#[tokio::test]
async fn rejected_token_device_reenrolls_without_reconnecting() {
    let mut fixture = Fixture::start_token_enrolled().await;
    assert!(matches!(
        fixture.next_event().await,
        Event::TokenDeviceRejected
    ));
    let event = fixture.next_event().await;
    assert!(
        matches!(event, Event::TokenEnrollmentRequested),
        "a rejected token device must enroll again without reconnecting first: {event:?}"
    );
    assert_eq!(fixture.refreshes.load(Ordering::SeqCst), 0);
    assert!(!fixture.directory.path().join("identity.json").exists());
}
