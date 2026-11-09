use std::{net::SocketAddr, pin::Pin, time::Duration};

use agentdesktop_proto::fleet::{
    AgentMessage, BeginEnrollmentRequest, BeginEnrollmentResponse, CompleteEnrollmentRequest,
    ControllerMessage, DeviceCertificateResponse, DeviceUsageRequest, DeviceUsageResponse,
    EnrollResponse, LlmGatewayCredentialRequest, LlmGatewayCredentialResponse,
    RenewDeviceCertificateRequest,
    fleet_agent_client::FleetAgentClient,
    fleet_agent_server::{FleetAgent, FleetAgentServer},
};
use futures_core::Stream;
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Code, Request, Response, Status, Streaming,
    transport::{Identity, ServerTlsConfig, server::TcpIncoming},
};

use super::{FLEET_LIMITS, FleetLimits};

/// Limits small enough for tests to observe; anything a test does not exercise
/// stays at the production value.
fn limits() -> FleetLimits {
    let mut limits = FLEET_LIMITS;
    limits.tls_handshake = Duration::from_millis(200);
    limits.request = Duration::from_millis(200);
    limits
}

/// Fails the test, instead of hanging it, when the server never ends the wait.
async fn bounded<T>(call: impl std::future::Future<Output = T>) -> T {
    time::timeout(Duration::from_secs(5), call)
        .await
        .expect("the server did not enforce the limit")
}

/// A fleet service whose calls take `delay`; `Connect` sends a message after each
/// `delay`, twice.
#[derive(Clone)]
struct Slow {
    delay: Duration,
}

type MessageStream = Pin<Box<dyn Stream<Item = Result<ControllerMessage, Status>> + Send>>;

#[tonic::async_trait]
impl FleetAgent for Slow {
    type ConnectStream = MessageStream;

    async fn begin_enrollment(
        &self,
        _: Request<BeginEnrollmentRequest>,
    ) -> Result<Response<BeginEnrollmentResponse>, Status> {
        time::sleep(self.delay).await;
        Ok(Response::new(BeginEnrollmentResponse::default()))
    }

    async fn complete_enrollment(
        &self,
        _: Request<CompleteEnrollmentRequest>,
    ) -> Result<Response<EnrollResponse>, Status> {
        Err(Status::unimplemented("test"))
    }

    async fn renew_device_certificate(
        &self,
        _: Request<RenewDeviceCertificateRequest>,
    ) -> Result<Response<DeviceCertificateResponse>, Status> {
        Err(Status::unimplemented("test"))
    }

    async fn get_llm_gateway_credential(
        &self,
        _: Request<LlmGatewayCredentialRequest>,
    ) -> Result<Response<LlmGatewayCredentialResponse>, Status> {
        Err(Status::unimplemented("test"))
    }

    async fn get_device_usage(
        &self,
        _: Request<DeviceUsageRequest>,
    ) -> Result<Response<DeviceUsageResponse>, Status> {
        Err(Status::unimplemented("test"))
    }

    async fn connect(
        &self,
        _: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let (sender, receiver) = mpsc::channel(2);
        let delay = self.delay;
        tokio::spawn(async move {
            for _ in 0..2 {
                time::sleep(delay).await;
                if sender.send(Ok(ControllerMessage::default())).await.is_err() {
                    return;
                }
            }
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

/// Serves `service` with `limits`, over TLS when `identity` is given.
async fn serve(limits: FleetLimits, identity: Option<Identity>, service: Slow) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let mut server = limits.server();
    if let Some(identity) = identity {
        server = server
            .tls_config(limits.tls(ServerTlsConfig::new().identity(identity)))
            .unwrap();
    }
    let router = server.add_service(FleetAgentServer::new(service));
    tokio::spawn(router.serve_with_incoming(TcpIncoming::from(listener)));
    address
}

async fn client(address: SocketAddr) -> FleetAgentClient<tonic::transport::Channel> {
    // The generated `connect` RPC shadows the client's `connect` constructor.
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    FleetAgentClient::new(channel)
}

/// Reads and discards everything the server sends until it closes the connection.
async fn until_closed(mut stream: TcpStream) {
    let mut buffer = [0_u8; 1024];
    while matches!(stream.read(&mut buffer).await, Ok(read) if read > 0) {}
}

#[tokio::test]
async fn a_client_that_never_finishes_the_tls_handshake_is_disconnected() {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let identity = Identity::from_pem(certified.cert.pem(), certified.signing_key.serialize_pem());
    let address = serve(
        limits(),
        Some(identity),
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;

    // Connect and never send a ClientHello.
    let stream = TcpStream::connect(address).await.unwrap();

    bounded(until_closed(stream)).await;
}

#[tokio::test]
async fn a_request_that_runs_too_long_is_ended() {
    let address = serve(
        limits(),
        None,
        Slow {
            delay: Duration::from_secs(30),
        },
    )
    .await;

    let error = bounded(
        client(address)
            .await
            .begin_enrollment(BeginEnrollmentRequest::default()),
    )
    .await
    .expect_err("a request outlived the request timeout");

    assert_eq!(error.code(), Code::Cancelled, "{error}");
}

#[tokio::test]
async fn the_request_timeout_does_not_end_an_open_stream() {
    // Each message arrives after more than the request timeout.
    let delay = limits().request * 2;
    let address = serve(limits(), None, Slow { delay }).await;
    let (_outbound, receiver) = mpsc::channel::<AgentMessage>(1);

    let mut stream = bounded(client(address).await.connect(ReceiverStream::new(receiver)))
        .await
        .expect("open the stream")
        .into_inner();

    for message in 0..2 {
        let received = bounded(stream.message())
            .await
            .unwrap_or_else(|error| panic!("stream ended at message {message}: {error}"));
        assert!(received.is_some(), "stream closed at message {message}");
    }
}
