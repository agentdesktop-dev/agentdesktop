use std::{net::SocketAddr, pin::Pin, time::Duration};

use agentdesktop_proto::fleet::{
    AgentMessage, BeginEnrollmentRequest, BeginEnrollmentResponse, CompleteEnrollmentRequest,
    ControllerMessage, DeviceCertificateResponse, DeviceUsageRequest, DeviceUsageResponse,
    EnrollResponse, LlmGatewayCredentialRequest, LlmGatewayCredentialResponse,
    RenewDeviceCertificateRequest, fleet_agent_client::FleetAgentClient,
    fleet_agent_server::FleetAgent,
};
use futures_core::Stream;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Code, Request, Response, Status, Streaming,
    transport::{Identity, ServerTlsConfig, server::TcpIncoming},
};

use super::{Connections, FLEET_LIMITS, FleetLimits};

/// Fails the test, instead of hanging it, when the server never ends the wait.
async fn bounded<T>(call: impl std::future::Future<Output = T>) -> T {
    time::timeout(Duration::from_secs(5), call)
        .await
        .expect("the server did not enforce the limit")
}

/// A fleet service whose calls take `delay`; `Connect` sends a message after each
/// `delay`, twice, then ends its session, holding the connection's session
/// release as the controller's `Connect` does.
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
        request: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::ConnectStream>, Status> {
        let session = super::SessionBusy::of(&request).map(|busy| busy.release_on_drop());
        let (sender, receiver) = mpsc::channel(2);
        let delay = self.delay;
        tokio::spawn(async move {
            let _session = session;
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
    let tls = identity.map(|identity| ServerTlsConfig::new().identity(identity));
    serve_tracked(limits, tls, service).await.0
}

/// Like `serve`, with any TLS configuration, also returning the connection registry.
async fn serve_tracked(
    limits: FleetLimits,
    tls: Option<ServerTlsConfig>,
    service: Slow,
) -> (SocketAddr, Connections) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let connections = limits.connections();
    // HTTP/1 is accepted as the controller does (`main.rs`), so connections go
    // through hyper-util's protocol detection, as they do in production.
    let mut server = limits.server(&connections).accept_http1(true);
    if let Some(tls) = tls {
        server = server.tls_config(limits.tls(tls)).unwrap();
    }
    let router = server.add_service(limits.fleet_service(service));
    tokio::spawn(router.serve_with_incoming(connections.incoming(TcpIncoming::from(listener))));
    (address, connections)
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
    let limits = FleetLimits {
        tls_handshake: Duration::from_millis(200),
        ..FLEET_LIMITS
    };
    let address = serve(
        limits,
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
    let limits = FleetLimits {
        request: Duration::from_millis(200),
        ..FLEET_LIMITS
    };
    let address = serve(
        limits,
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
    let limits = FleetLimits {
        request: Duration::from_millis(200),
        ..FLEET_LIMITS
    };
    // Each message arrives after more than the request timeout.
    let delay = limits.request * 2;
    let address = serve(limits, None, Slow { delay }).await;
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

#[tokio::test]
async fn a_connection_that_stops_answering_pings_is_closed() {
    let limits = FleetLimits {
        keepalive_interval: Duration::from_millis(100),
        keepalive_timeout: Duration::from_millis(100),
        ..FLEET_LIMITS
    };
    let address = serve(
        limits,
        None,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;
    let mut stream = TcpStream::connect(address).await.unwrap();

    // Open an HTTP/2 connection (preface and an empty SETTINGS frame), then go
    // silent: never acknowledge the server's SETTINGS or PINGs.
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00")
        .await
        .unwrap();

    bounded(until_closed(stream)).await;
}

/// How long two concurrent half-second calls take on one connection.
async fn two_calls_on_one_connection(limits: FleetLimits) -> Duration {
    let delay = Duration::from_millis(500);
    let address = serve(limits, None, Slow { delay }).await;
    let mut client = client(address).await;
    // One quick call first, so the HTTP/2 SETTINGS exchange (which carries the
    // stream limit) has completed before the measured calls.
    let _ = client.get_device_usage(DeviceUsageRequest::default()).await;
    let (mut first, mut second) = (client.clone(), client);
    let started = time::Instant::now();
    let (a, b) = bounded(async {
        tokio::join!(
            first.begin_enrollment(BeginEnrollmentRequest::default()),
            second.begin_enrollment(BeginEnrollmentRequest::default()),
        )
    })
    .await;
    a.unwrap();
    b.unwrap();
    started.elapsed()
}

#[tokio::test]
async fn requests_beyond_the_per_connection_limit_wait() {
    let limits = FleetLimits {
        requests_per_connection: 1,
        ..FLEET_LIMITS
    };

    let elapsed = two_calls_on_one_connection(limits).await;

    assert!(
        elapsed >= Duration::from_millis(950),
        "ran concurrently: {elapsed:?}"
    );
}

#[tokio::test]
async fn streams_beyond_the_per_connection_limit_wait() {
    let limits = FleetLimits {
        streams_per_connection: 1,
        ..FLEET_LIMITS
    };

    let elapsed = two_calls_on_one_connection(limits).await;

    assert!(
        elapsed >= Duration::from_millis(950),
        "ran concurrently: {elapsed:?}"
    );
}

fn small_messages() -> FleetLimits {
    FleetLimits {
        max_message_bytes: 1024,
        unauthenticated_message_bytes: 128,
        ..FLEET_LIMITS
    }
}

#[tokio::test]
async fn an_oversized_enrollment_request_is_rejected() {
    let address = serve(
        small_messages(),
        None,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;

    let error = bounded(
        client(address)
            .await
            .begin_enrollment(BeginEnrollmentRequest {
                hostname: "h".repeat(512),
                ..Default::default()
            }),
    )
    .await
    .expect_err("an oversized enrollment request was accepted");

    assert_eq!(error.code(), Code::OutOfRange, "{error}");
}

/// A device CA with a server certificate for `localhost` and a device certificate.
struct Pki {
    ca: tonic::transport::Certificate,
    server: Identity,
    device: Identity,
}

fn pki() -> Pki {
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, IsCa, KeyPair, KeyUsagePurpose,
    };

    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca = CertifiedIssuer::self_signed(ca_params, KeyPair::generate().unwrap()).unwrap();
    let leaf = |name: &str| {
        let key = KeyPair::generate().unwrap();
        let certificate = CertificateParams::new(vec![name.to_owned()])
            .unwrap()
            .signed_by(&key, &ca)
            .unwrap();
        Identity::from_pem(certificate.pem(), key.serialize_pem())
    };
    Pki {
        ca: tonic::transport::Certificate::from_pem(ca.pem()),
        server: leaf("localhost"),
        device: leaf("device"),
    }
}

/// Serves with the production TLS shape: device CA, client certificate optional.
async fn serve_mtls(limits: FleetLimits, pki: &Pki) -> (SocketAddr, Connections) {
    let tls = ServerTlsConfig::new()
        .identity(pki.server.clone())
        .client_ca_root(pki.ca.clone())
        .client_auth_optional(true);
    serve_tracked(
        limits,
        Some(tls),
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await
}

async fn tls_client(
    address: SocketAddr,
    pki: &Pki,
    device: bool,
) -> FleetAgentClient<tonic::transport::Channel> {
    let mut tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(pki.ca.clone())
        .domain_name("localhost");
    if device {
        tls = tls.identity(pki.device.clone());
    }
    let channel = tonic::transport::Endpoint::from_shared(format!("https://{address}"))
        .unwrap()
        .tls_config(tls)
        .unwrap()
        .connect()
        .await
        .unwrap();
    FleetAgentClient::new(channel)
}

fn renewal(bytes: usize) -> RenewDeviceCertificateRequest {
    RenewDeviceCertificateRequest {
        certificate_signing_request_der: vec![0; bytes],
    }
}

#[tokio::test]
async fn device_calls_keep_the_larger_message_limit() {
    let pki = pki();
    let (address, _) = serve_mtls(small_messages(), &pki).await;
    let mut client = tls_client(address, &pki, true).await;

    // Over the unauthenticated limit but within the device limit: reaches the handler.
    let within = bounded(client.renew_device_certificate(renewal(512)))
        .await
        .expect_err("the test service rejects every renewal");
    assert_eq!(within.code(), Code::Unimplemented, "{within}");

    let over = bounded(client.renew_device_certificate(renewal(4096)))
        .await
        .expect_err("an oversized device request was accepted");
    assert_eq!(over.code(), Code::OutOfRange, "{over}");
}

#[tokio::test]
async fn callers_without_a_device_certificate_get_the_small_limit_on_every_call() {
    let pki = pki();
    let (address, _) = serve_mtls(small_messages(), &pki).await;
    let mut client = tls_client(address, &pki, false).await;

    let error = bounded(client.renew_device_certificate(renewal(512)))
        .await
        .expect_err("a large request without a certificate reached the handler");

    assert_eq!(error.code(), Code::OutOfRange, "{error}");
}

#[tokio::test]
async fn enrollment_gets_the_small_limit_even_with_a_device_certificate() {
    let pki = pki();
    let (address, _) = serve_mtls(small_messages(), &pki).await;
    let mut client = tls_client(address, &pki, true).await;

    // Over the unauthenticated limit but within the device limit.
    let error = bounded(client.begin_enrollment(BeginEnrollmentRequest {
        hostname: "h".repeat(512),
        ..Default::default()
    }))
    .await
    .expect_err("a large enrollment request reached the handler");

    assert_eq!(error.code(), Code::OutOfRange, "{error}");
}

#[tokio::test]
async fn a_tls_connection_that_never_makes_a_request_is_closed() {
    use std::io::Read;

    use rustls::pki_types::{CertificateDer, pem::PemObject};

    let pki = pki();
    let mut limits = FLEET_LIMITS;
    limits.first_request = Duration::from_millis(300);
    let (address, _) = serve_mtls(limits, &pki).await;
    let ca = pki.ca.clone().into_inner();

    // Finish the TLS handshake, then send nothing: not even the HTTP/2
    // preface, so the server is still detecting the protocol.
    let closed = tokio::task::spawn_blocking(move || {
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(&ca).expect("parse device CA"))
            .expect("trust device CA");
        let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
        let mut tls = rustls::ClientConnection::new(
            std::sync::Arc::new(config),
            "localhost".try_into().expect("server name"),
        )
        .expect("TLS client");
        let mut socket = std::net::TcpStream::connect(address).expect("connect");
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        while tls.is_handshaking() {
            tls.complete_io(&mut socket).expect("TLS handshake");
        }

        let mut stream = rustls::Stream::new(&mut tls, &mut socket);
        let mut buffer = [0_u8; 1024];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return false;
                }
                Err(_) => return true,
            }
        }
    })
    .await
    .unwrap();

    assert!(closed, "the server kept a silent TLS connection open");
}

#[tokio::test]
async fn a_tls_connection_idle_after_its_last_request_is_closed() {
    let pki = pki();
    let mut limits = FLEET_LIMITS;
    // Far past the test's bound, so only the idle close can end the connection.
    limits.first_request = Duration::from_secs(60);
    limits.idle_connection = Duration::from_millis(200);
    let (address, connections) = serve_mtls(limits, &pki).await;
    let mut client = tls_client(address, &pki, true).await;
    bounded(client.begin_enrollment(BeginEnrollmentRequest::default()))
        .await
        .unwrap();
    assert_eq!(connections.open_count(), 1);

    bounded(async {
        while connections.open_count() > 0 {
            time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
}

/// Whether the server keeps `stream` open for a while. An accepted connection
/// receives the server's HTTP/2 SETTINGS and then waits for the client; a
/// rejected one is closed (end of stream) right away.
async fn stays_open(mut stream: TcpStream) -> bool {
    let mut buffer = [0_u8; 1024];
    let closed = time::timeout(Duration::from_millis(300), async {
        loop {
            match stream.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await;
    closed.is_err()
}

async fn connections_over_cap(limits: FleetLimits) {
    let address = serve(
        limits,
        None,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;
    let first = TcpStream::connect(address).await.unwrap();
    let second = TcpStream::connect(address).await.unwrap();

    let third = TcpStream::connect(address).await.unwrap();
    bounded(until_closed(third)).await;

    // Closing a connection frees its slot.
    drop(first);
    time::sleep(Duration::from_millis(100)).await;
    let replacement = TcpStream::connect(address).await.unwrap();
    assert!(stays_open(replacement).await, "a freed slot was not reused");
    drop(second);
}

#[tokio::test]
async fn connections_over_the_per_address_cap_are_closed() {
    let mut limits = FLEET_LIMITS;
    limits.connections_per_address = 2;

    connections_over_cap(limits).await;
}

#[test]
fn an_address_over_its_cap_does_not_evict_other_connections() {
    use tokio::sync::oneshot::error::TryRecvError;

    let mut limits = FLEET_LIMITS;
    limits.max_connections = 2;
    limits.connections_per_address = 1;
    let connections = limits.connections();
    let first_address = |port| SocketAddr::from(([192, 0, 2, 1], port));
    let (_first, mut first_evicted) = connections
        .register(first_address(1))
        .expect("first connection");
    let (_other, mut other_evicted) = connections
        .register(SocketAddr::from(([192, 0, 2, 2], 1)))
        .expect("a connection from another address");

    // At the total cap, and over the first address's cap.
    assert!(connections.register(first_address(2)).is_none());

    assert_eq!(first_evicted.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(other_evicted.try_recv(), Err(TryRecvError::Empty));
}

#[tokio::test]
async fn at_the_total_cap_the_longest_idle_connection_makes_room() {
    let mut limits = FLEET_LIMITS;
    limits.max_connections = 2;
    let address = serve(
        limits,
        None,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;
    let oldest = TcpStream::connect(address).await.unwrap();
    time::sleep(Duration::from_millis(50)).await;
    let newer = TcpStream::connect(address).await.unwrap();
    time::sleep(Duration::from_millis(50)).await;

    let newest = TcpStream::connect(address).await.unwrap();

    bounded(until_closed(oldest)).await;
    assert!(stays_open(newest).await, "the new connection was refused");
    drop(newer);
}

#[tokio::test]
async fn at_the_total_cap_a_new_connection_is_closed_when_every_connection_is_busy() {
    let mut limits = FLEET_LIMITS;
    limits.max_connections = 1;
    let address = serve(
        limits,
        None,
        Slow {
            delay: Duration::from_millis(500),
        },
    )
    .await;
    let (_outbound, receiver) = mpsc::channel::<AgentMessage>(1);
    let mut stream = bounded(client(address).await.connect(ReceiverStream::new(receiver)))
        .await
        .expect("open the stream")
        .into_inner();

    let newcomer = TcpStream::connect(address).await.unwrap();
    bounded(until_closed(newcomer)).await;

    // The busy connection was kept.
    assert!(bounded(stream.message()).await.unwrap().is_some());
}

#[tokio::test]
async fn a_connection_that_never_sends_anything_is_closed() {
    let mut limits = FLEET_LIMITS;
    limits.first_request = Duration::from_millis(200);
    let address = serve(
        limits,
        None,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;

    let stream = TcpStream::connect(address).await.unwrap();

    bounded(until_closed(stream)).await;
}

#[tokio::test]
async fn a_connection_that_never_makes_a_request_is_closed() {
    let mut limits = FLEET_LIMITS;
    limits.first_request = Duration::from_millis(200);
    let address = serve(
        limits,
        None,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;
    let mut stream = TcpStream::connect(address).await.unwrap();

    // A valid HTTP/2 preface and SETTINGS frame, then no request.
    stream
        .write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00")
        .await
        .unwrap();

    bounded(until_closed(stream)).await;
}

#[tokio::test]
async fn a_connection_idle_after_its_last_request_is_closed() {
    let mut limits = FLEET_LIMITS;
    // Far past the test's bound, so only the idle close can end the connection.
    limits.first_request = Duration::from_secs(60);
    limits.idle_connection = Duration::from_millis(200);
    let (address, connections) = serve_tracked(
        limits,
        None,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;
    let mut client = client(address).await;
    bounded(client.begin_enrollment(BeginEnrollmentRequest::default()))
        .await
        .unwrap();
    assert_eq!(connections.open_count(), 1);

    bounded(async {
        while connections.open_count() > 0 {
            time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
}

#[tokio::test]
async fn a_response_the_client_never_accepts_does_not_keep_its_connection_busy() {
    let mut limits = FLEET_LIMITS;
    // Far past the test's bound, so only the idle close can end the connection.
    limits.first_request = Duration::from_secs(60);
    limits.idle_connection = Duration::from_millis(200);
    let (address, connections) = serve_tracked(
        limits,
        None,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;
    // A client that grants no HTTP/2 flow-control window, so the server can
    // never send it a response body. It still answers keepalive PINGs.
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .initial_stream_window_size(0)
        .connect()
        .await
        .unwrap();
    let mut stalled = FleetAgentClient::new(channel);
    let call = tokio::spawn(async move {
        stalled
            .begin_enrollment(BeginEnrollmentRequest::default())
            .await
    });
    bounded(async {
        while connections.open_count() == 0 {
            time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    bounded(async {
        while connections.open_count() > 0 {
            time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let response = bounded(call).await.unwrap();
    assert!(response.is_err(), "the stalled response was delivered");
}

#[tokio::test]
async fn a_stalled_connect_response_frees_its_connection_when_the_session_ends() {
    let mut limits = FLEET_LIMITS;
    // Far past the test's bound, so only the idle close can end the connection.
    limits.first_request = Duration::from_secs(60);
    limits.idle_connection = Duration::from_millis(200);
    let (address, connections) = serve_tracked(
        limits,
        None,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;
    // A client that grants no flow-control window: the session's messages can
    // never be sent, so hyper keeps the response body after the session ends.
    let channel = tonic::transport::Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .initial_stream_window_size(0)
        .connect()
        .await
        .unwrap();
    let (_outbound, receiver) = mpsc::channel::<AgentMessage>(1);
    let _stream = bounded(FleetAgentClient::new(channel).connect(ReceiverStream::new(receiver)))
        .await
        .expect("open the stream");
    assert_eq!(connections.open_count(), 1);

    bounded(async {
        while connections.open_count() > 0 {
            time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
}

#[tokio::test]
async fn an_open_stream_keeps_its_connection_from_going_idle() {
    let mut limits = FLEET_LIMITS;
    limits.idle_connection = Duration::from_millis(200);
    // Messages arrive further apart than the idle limit.
    let address = serve(
        limits,
        None,
        Slow {
            delay: Duration::from_millis(500),
        },
    )
    .await;
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

#[test]
fn the_configuration_defaults_match_the_default_limits() {
    use agentdesktop_core::config::ControllerFleetLimitsConfig;

    assert_eq!(
        FLEET_LIMITS.configured(&ControllerFleetLimitsConfig::default()),
        FLEET_LIMITS
    );
}

#[test]
fn configured_limits_replace_every_deployment_limit() {
    use agentdesktop_core::config::ControllerFleetLimitsConfig;

    // The literal names every setting, each with a value that is not its
    // default, so a setting `configured` does not copy fails here.
    let config = ControllerFleetLimitsConfig {
        max_connections: 8,
        connections_per_address: 7,
        max_message_bytes: 6,
        enrollment_in_flight: 5,
        enrollment_burst: 4,
        enrollment_refill: Duration::from_millis(3),
        device_message_burst: 2,
        device_message_refill: Duration::from_millis(1),
    };

    assert_eq!(
        FLEET_LIMITS.configured(&config),
        FleetLimits {
            max_connections: 8,
            connections_per_address: 7,
            max_message_bytes: 6,
            enrollment_in_flight: 5,
            enrollment_burst: 4,
            enrollment_refill: Duration::from_millis(3),
            device_message_burst: 2,
            device_message_refill: Duration::from_millis(1),
            ..FLEET_LIMITS
        }
    );
}

#[test]
fn a_repeated_warning_is_logged_once_per_interval_with_its_count() {
    let warning = super::ThrottledWarning::new();
    let start = tokio::time::Instant::now();

    assert_eq!(warning.event(start), Some(1));
    assert_eq!(warning.event(start + Duration::from_secs(1)), None);
    assert_eq!(warning.event(start + Duration::from_secs(9)), None);
    assert_eq!(warning.event(start + Duration::from_secs(10)), Some(3));
    assert_eq!(warning.event(start + Duration::from_secs(11)), None);
}

mod enrollment {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    use tokio::time::Instant;

    use super::super::{FLEET_LIMITS, FleetLimits, PEERS_PER_GENERATION, address_key};
    use super::Duration;

    fn limits(in_flight: usize, burst: u32) -> FleetLimits {
        FleetLimits {
            enrollment_in_flight: in_flight,
            enrollment_burst: burst,
            enrollment_refill: Duration::from_secs(10),
            ..FLEET_LIMITS
        }
    }

    fn peer(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, last))
    }

    #[test]
    fn a_peer_gets_its_burst_then_one_request_per_refill() {
        let limiter = limits(64, 3).enrollment();
        let start = Instant::now();

        for _ in 0..3 {
            assert!(limiter.take(peer(1), start));
        }
        assert!(!limiter.take(peer(1), start), "burst exceeded");
        assert!(
            !limiter.take(peer(1), start + Duration::from_secs(9)),
            "refilled early"
        );
        assert!(limiter.take(peer(1), start + Duration::from_secs(10)));
        assert!(!limiter.take(peer(1), start + Duration::from_secs(10)));
    }

    #[test]
    fn peers_have_separate_budgets() {
        let limiter = limits(64, 1).enrollment();
        let now = Instant::now();

        assert!(limiter.take(peer(1), now));
        assert!(!limiter.take(peer(1), now));
        assert!(limiter.take(peer(2), now));
    }

    #[test]
    fn a_full_bucket_is_never_overfilled() {
        let limiter = limits(64, 2).enrollment();
        let start = Instant::now();

        assert!(limiter.take(peer(1), start));
        // A long idle period refills only up to the burst.
        let later = start + Duration::from_secs(3600);
        assert!(limiter.take(peer(1), later));
        assert!(limiter.take(peer(1), later));
        assert!(!limiter.take(peer(1), later));
    }

    #[tokio::test]
    async fn requests_in_progress_are_capped_until_one_finishes() {
        let limiter = limits(2, 60).enrollment();
        let address = |last| Some(SocketAddr::new(peer(last), 4000));

        let first = limiter.admit(address(1)).expect("first request");
        let _second = limiter.admit(address(2)).expect("second request");
        let error = limiter
            .admit(address(3))
            .expect_err("third request admitted");
        assert_eq!(error.code(), tonic::Code::ResourceExhausted);

        drop(first);
        let _third = limiter
            .admit(address(3))
            .expect("admitted after one finished");
    }

    #[tokio::test]
    async fn a_rate_limited_peer_is_rejected_with_resource_exhausted() {
        let limiter = limits(64, 1).enrollment();
        let address = Some(SocketAddr::new(peer(1), 4000));

        let _first = limiter.admit(address).expect("first request");
        let error = limiter.admit(address).expect_err("second request admitted");

        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
    }

    #[tokio::test]
    async fn completing_an_enrollment_does_not_spend_the_address_budget() {
        let limiter = limits(64, 1).enrollment();
        let address = Some(SocketAddr::new(peer(1), 4000));
        let _begin = limiter.admit(address).expect("begin");

        let _complete = limiter.admit_completion().await.expect("complete");
    }

    fn one_slot(completion_wait: Duration) -> FleetLimits {
        FleetLimits {
            enrollment_completion_wait: completion_wait,
            ..limits(1, 60)
        }
    }

    #[tokio::test]
    async fn completing_an_enrollment_waits_for_a_slot() {
        let limiter = one_slot(Duration::from_secs(5)).enrollment();
        let begin = limiter.admit(None).expect("begin");
        let waiting = tokio::spawn({
            let limiter = limiter.clone();
            async move { limiter.admit_completion().await.map(drop) }
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!waiting.is_finished(), "completion did not wait");

        drop(begin);

        waiting
            .await
            .unwrap()
            .expect("completion admitted once a slot freed");
    }

    #[tokio::test]
    async fn completing_an_enrollment_gives_up_after_the_wait() {
        let limiter = one_slot(Duration::from_millis(50)).enrollment();
        let _begin = limiter.admit(None).expect("begin");

        let error = limiter.admit_completion().await.expect_err("admitted");

        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
    }

    #[tokio::test]
    async fn requests_without_a_peer_address_still_count_against_the_cap() {
        let limiter = limits(1, 60).enrollment();

        let _first = limiter.admit(None).expect("first request");
        assert!(limiter.admit(None).is_err());
    }

    fn numbered(index: usize) -> IpAddr {
        let [a, b, c, d] = u32::try_from(index).unwrap().to_be_bytes();
        IpAddr::V4(Ipv4Addr::new(10 | a, b, c, d))
    }

    #[test]
    fn the_peer_table_stays_bounded_while_addresses_keep_changing() {
        let limiter = limits(64, 1).enrollment();
        let now = Instant::now();

        for index in 0..5 * PEERS_PER_GENERATION {
            assert!(limiter.take(numbered(index), now));
        }

        assert!(limiter.peers.lock().unwrap().len() <= 2 * PEERS_PER_GENERATION);
    }

    #[test]
    fn a_peer_seen_recently_keeps_its_spent_budget_when_the_table_turns_over() {
        let limiter = limits(64, 1).enrollment();
        let now = Instant::now();
        assert!(limiter.take(peer(1), now));

        // Fill a generation so peer 1 moves to the previous one.
        for index in 0..PEERS_PER_GENERATION {
            assert!(limiter.take(numbered(index), now));
        }

        assert!(!limiter.take(peer(1), now), "the spent budget was reset");
    }

    fn v6(address: &str) -> IpAddr {
        address.parse::<Ipv6Addr>().unwrap().into()
    }

    #[test]
    fn addresses_in_one_ipv6_64_share_a_budget() {
        let limiter = limits(64, 1).enrollment();
        let now = Instant::now();

        assert!(limiter.take(v6("2001:db8:1:2::1"), now));
        assert!(!limiter.take(v6("2001:db8:1:2:ffff::9"), now));
        assert!(limiter.take(v6("2001:db8:1:3::1"), now), "another /64");
    }

    #[test]
    fn ipv4_mapped_addresses_count_as_ipv4() {
        assert_eq!(address_key(v6("::ffff:192.0.2.1")), peer(1));
        assert_eq!(address_key(peer(1)), peer(1));
        assert_eq!(address_key(v6("2001:db8::1")), v6("2001:db8::"));
    }
}

mod device_streams {
    use tokio::{sync::oneshot::error::TryRecvError, time::Instant};

    use super::super::{DeviceStreams, FLEET_LIMITS};
    use super::Duration;

    fn streams() -> DeviceStreams {
        FLEET_LIMITS.device_streams()
    }

    #[test]
    fn a_newer_stream_from_the_same_device_replaces_the_older_one() {
        let streams = streams();
        let mut older = streams.open("device-a");
        let mut other_device = streams.open("device-b");

        let mut newer = streams.open("device-a");

        assert_eq!(older.superseded.try_recv(), Ok(()));
        assert_eq!(other_device.superseded.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(newer.superseded.try_recv(), Err(TryRecvError::Empty));
    }

    #[test]
    fn a_replaced_stream_closing_keeps_the_newer_registration() {
        let streams = streams();
        let older = streams.open("device-a");
        let mut newer = streams.open("device-a");

        drop(older);
        let _newest = streams.open("device-a");

        // The newer stream was still registered, so the newest one replaced it.
        assert_eq!(newer.superseded.try_recv(), Ok(()));
    }

    #[test]
    fn a_closed_stream_is_unregistered() {
        let streams = streams();
        drop(streams.open("device-a"));

        assert!(
            streams.devices.lock().unwrap()["device-a"]
                .current
                .is_none(),
            "the closed stream is still registered"
        );
    }

    #[test]
    fn a_device_gets_its_message_burst_then_the_refill_rate() {
        let mut limits = FLEET_LIMITS;
        limits.device_message_burst = 3;
        limits.device_message_refill = Duration::from_secs(1);
        let stream = limits.device_streams().open("device-a");
        let start = Instant::now();

        for _ in 0..3 {
            assert!(stream.take_message(start));
        }
        assert!(!stream.take_message(start), "burst exceeded");
        assert!(stream.take_message(start + Duration::from_secs(1)));
        assert!(!stream.take_message(start + Duration::from_secs(1)));
    }

    #[test]
    fn reconnecting_does_not_reset_the_message_budget() {
        let mut limits = FLEET_LIMITS;
        limits.device_message_burst = 2;
        limits.device_message_refill = Duration::from_secs(3600);
        let streams = limits.device_streams();
        let now = Instant::now();
        let first = streams.open("device-a");
        assert!(first.take_message(now));
        assert!(first.take_message(now));
        drop(first);

        let second = streams.open("device-a");

        assert!(!second.take_message(now), "a new stream got a fresh budget");
    }

    #[test]
    fn a_device_that_opens_streams_too_often_is_refused() {
        let mut limits = FLEET_LIMITS;
        limits.stream_open_burst = 2;
        let streams = limits.device_streams();

        streams.admit_open("device-a").expect("first open");
        streams.admit_open("device-a").expect("second open");
        let error = streams
            .admit_open("device-a")
            .expect_err("third open admitted");

        assert_eq!(error.code(), tonic::Code::ResourceExhausted);
        streams
            .admit_open("device-b")
            .expect("another device is unaffected");
    }
}
