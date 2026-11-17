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
    net::TcpStream,
    sync::mpsc,
    time,
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Code, Request, Response, Status, Streaming,
    service::Routes,
    transport::{Identity, ServerTlsConfig},
};

use super::{Connections, FLEET_LIMITS, FleetLimits, RPC_CLASSES, rpc_class};

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
    service: impl FleetAgent,
) -> (SocketAddr, Connections) {
    // As the controller does at startup, so TLS works whichever rustls
    // backends the test build enables.
    crate::install_rustls_provider();
    // The controller's own listener, so these tests exercise what it runs.
    let listener = limits
        .listener(SocketAddr::from(([127, 0, 0, 1], 0)), tls)
        .unwrap();
    let address = listener.local_address();
    let connections = listener.connections().clone();
    tokio::spawn(listener.serve(service, Routes::default()));
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
    stream.write_all(PREFACE).await.unwrap();

    bounded(until_closed(stream)).await;
}

/// The error code of the GOAWAY the server sends, or `None` if it closes the
/// connection without one.
async fn goaway_code(mut stream: TcpStream) -> Option<u32> {
    loop {
        let mut header = [0_u8; 9];
        stream.read_exact(&mut header).await.ok()?;
        let length = u32::from_be_bytes([0, header[0], header[1], header[2]]) as usize;
        let mut payload = vec![0_u8; length];
        stream.read_exact(&mut payload).await.ok()?;
        // GOAWAY: last stream ID, then the error code.
        if header[3] == 0x7 {
            return Some(u32::from_be_bytes(payload[4..8].try_into().unwrap()));
        }
    }
}

#[tokio::test]
async fn a_client_that_provokes_too_many_stream_resets_is_disconnected() {
    let limits = FleetLimits {
        error_resets_per_connection: 1,
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

    // The preface and an empty SETTINGS frame, then two PRIORITY frames that
    // make streams 1 and 3 depend on themselves. Each is a stream error, so the
    // server resets the stream.
    let mut frames = PREFACE.to_vec();
    for stream_id in [1_u32, 3] {
        frames.extend_from_slice(&[0, 0, 5, 0x2, 0]);
        frames.extend_from_slice(&stream_id.to_be_bytes());
        frames.extend_from_slice(&stream_id.to_be_bytes());
        frames.push(0);
    }
    stream.write_all(&frames).await.unwrap();

    // ENHANCE_YOUR_CALM
    assert_eq!(bounded(goaway_code(stream)).await, Some(0xb));
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
async fn streams_beyond_the_per_connection_limit_wait() {
    let limits = FleetLimits {
        concurrent_streams_per_connection: 1,
        ..FLEET_LIMITS
    };

    let elapsed = two_calls_on_one_connection(limits).await;

    assert!(
        elapsed >= Duration::from_millis(950),
        "ran concurrently: {elapsed:?}"
    );
}

fn small_messages() -> FleetLimits {
    let mut limits = FleetLimits {
        unauthenticated_message_bytes: 128,
        ..FLEET_LIMITS
    };
    limits.configured.max_message_bytes = 1024;
    limits
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

/// The device that `Pki`'s device certificate identifies.
const DEVICE_ID: &str = "7ca03414-bb20-4c80-98ef-7b0538b988ba";

/// A device CA with a server certificate for `localhost` and a certificate for
/// the device `DEVICE_ID`, which the controller issues.
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
        device: leaf(&format!("{DEVICE_ID}.device.agentdesktop.invalid")),
    }
}

/// Serves with the production TLS shape: device CA, client certificate optional.
async fn serve_mtls(limits: FleetLimits, pki: &Pki) -> (SocketAddr, Connections) {
    serve_mtls_with(
        limits,
        pki,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await
}

/// Like `serve_mtls`, serving `service`.
async fn serve_mtls_with(
    limits: FleetLimits,
    pki: &Pki,
    service: impl FleetAgent,
) -> (SocketAddr, Connections) {
    let tls = ServerTlsConfig::new()
        .identity(pki.server.clone())
        .client_ca_root(pki.ca.clone())
        .client_auth_optional(true);
    serve_tracked(limits, Some(tls), service).await
}

/// A TLS endpoint for `address`, presenting the device certificate when `device`.
fn tls_endpoint(address: SocketAddr, pki: &Pki, device: bool) -> tonic::transport::Endpoint {
    let mut tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(pki.ca.clone())
        .domain_name("localhost");
    if device {
        tls = tls.identity(pki.device.clone());
    }
    tonic::transport::Endpoint::from_shared(format!("https://{address}"))
        .unwrap()
        .tls_config(tls)
        .unwrap()
}

async fn tls_client(
    address: SocketAddr,
    pki: &Pki,
    device: bool,
) -> FleetAgentClient<tonic::transport::Channel> {
    FleetAgentClient::new(tls_endpoint(address, pki, device).connect().await.unwrap())
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

#[tokio::test]
async fn at_the_per_address_cap_the_addresses_longest_idle_connection_makes_room() {
    let mut limits = FLEET_LIMITS;
    limits.configured.connections_per_address = 2;
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
async fn at_the_per_address_cap_a_new_connection_is_closed_when_every_connection_is_busy() {
    let pki = pki();
    let mut limits = FLEET_LIMITS;
    limits.configured.connections_per_address = 1;
    let (address, _) = serve_mtls_with(
        limits,
        &pki,
        Slow {
            delay: Duration::from_millis(500),
        },
    )
    .await;
    let (_outbound, receiver) = mpsc::channel::<AgentMessage>(1);
    let mut stream = bounded(
        tls_client(address, &pki, true)
            .await
            .connect(ReceiverStream::new(receiver)),
    )
    .await
    .expect("open the stream")
    .into_inner();

    let newcomer = TcpStream::connect(address).await.unwrap();
    bounded(until_closed(newcomer)).await;

    // The busy connection was kept.
    assert!(bounded(stream.message()).await.unwrap().is_some());
}

#[test]
fn at_its_cap_an_address_evicts_only_its_own_idle_connections() {
    use tokio::sync::oneshot::error::TryRecvError;

    let mut limits = FLEET_LIMITS;
    limits.configured.max_connections = 2;
    limits.configured.connections_per_address = 1;
    let connections = Connections::new(&limits);
    let first_address = |port| SocketAddr::from(([192, 0, 2, 1], port));
    let (_first, mut first_evicted) = connections
        .register(first_address(1))
        .expect("first connection");
    let other = SocketAddr::from(([192, 0, 2, 2], 1));
    let (_other, mut other_evicted) = connections
        .register(other)
        .expect("a connection from another address");

    // At the total cap and at the first address's cap: the first address's
    // own connection makes room, though the other one has been idle as long.
    assert!(connections.register(first_address(2)).is_some());
    assert_eq!(first_evicted.try_recv(), Ok(()));
    assert_eq!(other_evicted.try_recv(), Err(TryRecvError::Empty));

    // A busy connection is never evicted, so its address is refused.
    let _busy = connections
        .start_request(other, true)
        .expect("busy request");
    assert!(
        connections
            .register(SocketAddr::from(([192, 0, 2, 2], 2)))
            .is_none()
    );
    assert_eq!(other_evicted.try_recv(), Err(TryRecvError::Empty));
}

#[test]
fn a_connection_made_idle_by_its_last_request_is_evictable_at_its_address_cap() {
    use tokio::sync::oneshot::error::TryRecvError;

    let mut limits = FLEET_LIMITS;
    limits.configured.connections_per_address = 1;
    let connections = Connections::new(&limits);
    let peer = |port| SocketAddr::from(([192, 0, 2, 1], port));
    let (_first, mut first_evicted) = connections.register(peer(1)).expect("first connection");
    let busy = connections
        .start_request(peer(1), true)
        .expect("busy request");
    assert!(connections.register(peer(2)).is_none());

    // The request's end queues the connection as idle again.
    drop(busy);
    assert_eq!(first_evicted.try_recv(), Err(TryRecvError::Empty));
    assert!(connections.register(peer(3)).is_some());
    assert_eq!(first_evicted.try_recv(), Ok(()));
}

#[test]
fn a_request_without_a_device_certificate_does_not_keep_its_connection_from_eviction() {
    let mut limits = FLEET_LIMITS;
    limits.configured.max_connections = 1;
    let connections = Connections::new(&limits);
    let (_anonymous, mut evicted) = connections
        .register(SocketAddr::from(([192, 0, 2, 1], 1)))
        .expect("first connection");
    let _request = connections
        .start_request(SocketAddr::from(([192, 0, 2, 1], 1)), false)
        .expect("anonymous request");

    assert!(
        connections
            .register(SocketAddr::from(([192, 0, 2, 2], 1)))
            .is_some()
    );
    assert_eq!(evicted.try_recv(), Ok(()));
}

#[tokio::test]
async fn at_the_total_cap_the_longest_idle_connection_makes_room() {
    let mut limits = FLEET_LIMITS;
    limits.configured.max_connections = 2;
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
    let pki = pki();
    let mut limits = FLEET_LIMITS;
    limits.configured.max_connections = 1;
    let (address, _) = serve_mtls_with(
        limits,
        &pki,
        Slow {
            delay: Duration::from_millis(500),
        },
    )
    .await;
    let (_outbound, receiver) = mpsc::channel::<AgentMessage>(1);
    let mut stream = bounded(
        tls_client(address, &pki, true)
            .await
            .connect(ReceiverStream::new(receiver)),
    )
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
    stream.write_all(PREFACE).await.unwrap();

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
    let pki = pki();
    let mut limits = FLEET_LIMITS;
    // Far past the test's bound, so only the idle close can end the connection.
    limits.first_request = Duration::from_secs(60);
    limits.idle_connection = Duration::from_millis(200);
    let (address, connections) = serve_mtls(limits, &pki).await;
    // A device that grants no HTTP/2 flow-control window, so the server can
    // never send it a response body. It still answers keepalive PINGs.
    let channel = tls_endpoint(address, &pki, true)
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
    let pki = pki();
    let mut limits = FLEET_LIMITS;
    // Far past the test's bound, so only the idle close can end the connection.
    limits.first_request = Duration::from_secs(60);
    limits.idle_connection = Duration::from_millis(200);
    let (address, connections) = serve_mtls(limits, &pki).await;
    // A device that grants no flow-control window: the session's messages can
    // never be sent, so hyper keeps the response body after the session ends.
    let channel = tls_endpoint(address, &pki, true)
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
    let pki = pki();
    let mut limits = FLEET_LIMITS;
    limits.idle_connection = Duration::from_millis(200);
    // Messages arrive further apart than the idle limit.
    let (address, _) = serve_mtls_with(
        limits,
        &pki,
        Slow {
            delay: Duration::from_millis(500),
        },
    )
    .await;
    let (_outbound, receiver) = mpsc::channel::<AgentMessage>(1);
    let mut stream = bounded(
        tls_client(address, &pki, true)
            .await
            .connect(ReceiverStream::new(receiver)),
    )
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

/// A HEADERS frame opening a gRPC request for the fleet `method` on
/// `stream_id`. With `end_stream` the request ends with an empty body; without
/// it, the server waits for a body that this client never sends.
fn grpc_request_headers(stream_id: u32, method: &str, end_stream: bool) -> Vec<u8> {
    use agentdesktop_proto::fleet::fleet_agent_server::SERVICE_NAME;

    let path = format!("/{SERVICE_NAME}/{method}");
    assert!(path.len() < 127, "the path needs a one-byte HPACK length");
    // HPACK: `:method: POST` and `:scheme: http` from the static table, then
    // `:path`, `content-type` and `te` as literals without indexing.
    let mut block = vec![0x83, 0x86, 0x04, path.len() as u8];
    block.extend_from_slice(path.as_bytes());
    block.extend_from_slice(&[0x0f, 0x10, 16]);
    block.extend_from_slice(b"application/grpc");
    block.extend_from_slice(&[0x00, 2]);
    block.extend_from_slice(b"te");
    block.push(8);
    block.extend_from_slice(b"trailers");

    let mut frame = (block.len() as u32).to_be_bytes()[1..].to_vec();
    // Type HEADERS; flags END_HEADERS, plus END_STREAM.
    frame.extend_from_slice(&[0x1, if end_stream { 0x5 } else { 0x4 }]);
    frame.extend_from_slice(&stream_id.to_be_bytes());
    frame.extend_from_slice(&block);
    frame
}

/// Reads frames from `stream` until a HEADERS frame arrives on `stream_id`.
async fn until_response_headers(stream: &mut TcpStream, stream_id: u32) {
    loop {
        let mut header = [0_u8; 9];
        stream.read_exact(&mut header).await.unwrap();
        let length = u32::from_be_bytes([0, header[0], header[1], header[2]]);
        let mut payload = vec![0_u8; length as usize];
        stream.read_exact(&mut payload).await.unwrap();
        let frame_stream =
            u32::from_be_bytes([header[5], header[6], header[7], header[8]]) & 0x7fff_ffff;
        if header[3] == 0x1 && frame_stream == stream_id {
            return;
        }
    }
}

#[tokio::test]
async fn a_request_without_a_body_does_not_keep_its_connection_from_eviction() {
    let mut limits = FLEET_LIMITS;
    limits.configured.max_connections = 1;
    // Far past the test's bound, so only eviction can close the connection.
    limits.first_request = Duration::from_secs(60);
    let address = serve(
        limits,
        None,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;
    let mut anonymous = TcpStream::connect(address).await.unwrap();
    anonymous.write_all(PREFACE).await.unwrap();
    // A request whose body never comes: tonic waits for it until the request
    // timeout, before any handler runs.
    anonymous
        .write_all(&grpc_request_headers(1, "BeginEnrollment", false))
        .await
        .unwrap();
    // A request that ends at once. Its response shows the server has started
    // both.
    anonymous
        .write_all(&grpc_request_headers(3, "BeginEnrollment", true))
        .await
        .unwrap();
    bounded(until_response_headers(&mut anonymous, 3)).await;

    let newcomer = TcpStream::connect(address).await.unwrap();

    bounded(until_closed(anonymous)).await;
    assert!(stays_open(newcomer).await, "the new connection was refused");
}

/// The HTTP/2 client preface and an empty SETTINGS frame.
const PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00";

/// An HTTP/2 PING frame: length 8, type 6, no flags, stream 0, then its payload.
const PING: &[u8] = &[0, 0, 8, 6, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];

/// Batches of PINGs sent before a flood gives up on the server stalling.
const FLOOD_BATCHES: usize = 1000;

/// Sends the HTTP/2 preface, then PINGs without reading the server's replies,
/// until a write stalls or fails. The unread replies fill the server's send
/// buffer, and HTTP/2 stops reading until it can write them.
async fn flood_pings(stream: &mut TcpStream) {
    stream.write_all(PREFACE).await.unwrap();
    let pings = PING.repeat(4096);
    for _ in 0..FLOOD_BATCHES {
        match time::timeout(Duration::from_millis(500), stream.write_all(&pings)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) | Err(_) => return,
        }
    }
    panic!("the server read every PING without stalling or closing the connection");
}

/// Connects with a small receive buffer, so the server's unread replies fill
/// its send buffer quickly.
async fn connect_small_window(address: SocketAddr) -> TcpStream {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    socket.connect(address).await.unwrap()
}

/// Waits until at most `count` connections are open.
async fn until_open_at_most(connections: &Connections, count: usize) {
    while connections.open_count() > count {
        time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_connection_that_floods_pings_without_reading_is_closed() {
    let mut limits = FLEET_LIMITS;
    // Long enough that the server has stopped reading when it passes.
    limits.first_request = Duration::from_secs(2);
    let (address, connections) = serve_tracked(
        limits,
        None,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;
    let mut stream = connect_small_window(address).await;

    flood_pings(&mut stream).await;

    bounded(until_open_at_most(&connections, 0)).await;
}

#[tokio::test]
async fn an_evicted_connection_that_floods_pings_is_closed() {
    let mut limits = FLEET_LIMITS;
    limits.configured.max_connections = 1;
    // Far past the test's bound, so only eviction can end the flooding connection.
    limits.first_request = Duration::from_secs(60);
    let (address, connections) = serve_tracked(
        limits,
        None,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;
    let mut flooder = connect_small_window(address).await;
    flood_pings(&mut flooder).await;

    // At the cap, the new connection evicts the flooder, which has made no
    // request. The server answers its preface once it is admitted.
    let mut newcomer = TcpStream::connect(address).await.unwrap();
    newcomer.write_all(PREFACE).await.unwrap();
    let mut settings = [0_u8; 9];
    bounded(newcomer.read_exact(&mut settings))
        .await
        .expect("the new connection was refused");

    bounded(until_open_at_most(&connections, 1)).await;
    assert!(stays_open(newcomer).await, "the new connection was closed");
}

/// The RPC names in `fleet.proto`'s `FleetAgent` service.
fn fleet_rpcs(proto: &str) -> std::collections::BTreeSet<&str> {
    let (_, service) = proto
        .split_once("service FleetAgent {")
        .expect("fleet.proto declares service FleetAgent");
    let (service, _) = service
        .split_once('}')
        .expect("service FleetAgent is closed");
    service
        .lines()
        .filter_map(|line| line.trim().strip_prefix("rpc "))
        .map(|rpc| rpc.split_once('(').map_or(rpc, |(name, _)| name).trim())
        .collect()
}

#[test]
fn every_fleet_rpc_has_a_limit_class() {
    let rpcs = fleet_rpcs(include_str!("../../../proto/proto/fleet.proto"));
    let classified: std::collections::BTreeSet<&str> =
        RPC_CLASSES.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        classified.len(),
        RPC_CLASSES.len(),
        "RPC_CLASSES lists an RPC twice"
    );

    let unclassified: Vec<_> = rpcs.difference(&classified).collect();
    let not_in_proto: Vec<_> = classified.difference(&rpcs).collect();
    assert!(
        unclassified.is_empty() && not_in_proto.is_empty(),
        "every FleetAgent RPC needs a class in RPC_CLASSES (fleet_limits.rs), which sets its \
         message limit and how long it keeps its connection busy; an Enrollment RPC's handler \
         must also take an EnrollmentLimiter permit. Not classified: {unclassified:?}. \
         Classified but not in fleet.proto: {not_in_proto:?}"
    );
}

#[test]
fn fleet_paths_are_classified_by_the_table() {
    use agentdesktop_proto::fleet::fleet_agent_server::SERVICE_NAME;

    for (name, class) in RPC_CLASSES {
        assert_eq!(
            rpc_class(&format!("/{SERVICE_NAME}/{name}")),
            Some(*class),
            "{name}"
        );
    }
    assert_eq!(rpc_class(&format!("/{SERVICE_NAME}/Unknown")), None);
    assert_eq!(rpc_class("/other.Service/BeginEnrollment"), None);
    assert!(super::is_enrollment(&format!(
        "/{SERVICE_NAME}/CompleteEnrollment"
    )));
    assert!(super::is_connect(&format!("/{SERVICE_NAME}/Connect")));
    assert!(!super::is_enrollment(&format!("/{SERVICE_NAME}/Connect")));
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
        let mut limits = FLEET_LIMITS;
        limits.configured.enrollment_in_flight = in_flight;
        limits.configured.enrollment_burst = burst;
        limits.configured.enrollment_refill = Duration::from_secs(10);
        limits
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
        limits.configured.device_message_burst = 3;
        limits.configured.device_message_refill = Duration::from_secs(1);
        let stream = limits.device_streams().open("device-a");
        let start = Instant::now();
        let second = Duration::from_secs(1);

        assert_eq!(stream.spend_message(start), None);
        assert_eq!(stream.spend_message(start), None);
        assert_eq!(
            stream.spend_message(start),
            Some(start + second),
            "the burst is spent"
        );
        assert_eq!(
            stream.spend_message(start + second),
            Some(start + 2 * second),
            "one message per refill"
        );
    }

    #[test]
    fn a_message_read_before_the_budget_refills_delays_the_next_one() {
        let mut limits = FLEET_LIMITS;
        limits.configured.device_message_burst = 1;
        limits.configured.device_message_refill = Duration::from_secs(1);
        let stream = limits.device_streams().open("device-a");
        let start = Instant::now();

        assert_eq!(
            stream.spend_message(start),
            Some(start + Duration::from_secs(1))
        );

        assert_eq!(
            stream.spend_message(start),
            Some(start + Duration::from_secs(2))
        );
    }

    #[test]
    fn an_absurdly_long_refill_caps_the_wait() {
        let mut limits = FLEET_LIMITS;
        limits.configured.device_message_burst = 1;
        limits.configured.device_message_refill = Duration::MAX;
        let stream = limits.device_streams().open("device-a");
        let start = Instant::now();

        assert_eq!(
            stream.spend_message(start),
            Some(start + super::super::LONGEST_WAIT)
        );
    }

    #[test]
    fn reconnecting_does_not_reset_the_message_budget() {
        let mut limits = FLEET_LIMITS;
        limits.configured.device_message_burst = 2;
        limits.configured.device_message_refill = Duration::from_secs(3600);
        let streams = limits.device_streams();
        let now = Instant::now();
        let first = streams.open("device-a");
        assert_eq!(first.spend_message(now), None);
        assert!(first.spend_message(now).is_some(), "the burst is spent");
        drop(first);

        let second = streams.open("device-a");

        assert!(
            second.spend_message(now).is_some(),
            "a new stream got a fresh budget"
        );
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

/// The controller's own fleet service behind the fleet limits, with its
/// identity provider on a local port.
mod fleet_service {
    use std::{
        net::SocketAddr,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use agentdesktop_proto::fleet::{
        AgentMessage, BeginEnrollmentRequest, CompleteEnrollmentRequest, DaemonConfig,
        fleet_agent_server::FleetAgent,
    };
    use axum::{
        http::{StatusCode, Uri},
        response::{IntoResponse, Response},
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use serde_json::json;
    use tokio::{sync::mpsc, time};
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::{Code, Request, Status};

    use super::{
        super::{Connections, FLEET_LIMITS, FleetLimits},
        DEVICE_ID, Pki, bounded, pki, serve_mtls_with, tls_client, until_open_at_most,
    };
    use crate::{
        daemon_config::DaemonConfigStore, database::Database, oidc::OidcProvider,
        service::FleetAgentService,
    };

    /// The user the identity provider signs in, who enrolled `DEVICE_ID`.
    const SUBJECT: &str = "user";

    /// The `authorization` header a device sends. The identity provider
    /// accepts any access token.
    const AUTHORIZATION: &str = "Bearer access-token";

    /// One enrollment slot, which `CompleteEnrollment` waits for only briefly.
    const ONE_ENROLLMENT_SLOT: FleetLimits = {
        let mut limits = FLEET_LIMITS;
        limits.configured.enrollment_in_flight = 1;
        limits.enrollment_completion_wait = Duration::from_millis(50);
        limits
    };

    /// An identity provider on a local port. It answers discovery, its key set
    /// and UserInfo (for `SUBJECT`) until `stall` is called; from then on it
    /// holds every request without answering.
    struct IdentityProvider {
        issuer: String,
        stalled: Arc<AtomicBool>,
        /// The path of each request held since `stall`.
        held: mpsc::UnboundedReceiver<String>,
    }

    impl IdentityProvider {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let issuer = format!("http://{}", listener.local_addr().unwrap());
            let stalled = Arc::new(AtomicBool::new(false));
            let (hold, held) = mpsc::unbounded_channel();
            let endpoints = Endpoints {
                issuer: issuer.clone(),
                stalled: stalled.clone(),
                hold,
            };
            let app = axum::Router::new().fallback(move |uri: Uri| endpoints.clone().answer(uri));
            tokio::spawn(async move { axum::serve(listener, app).await });
            Self {
                issuer,
                stalled,
                held,
            }
        }

        /// The controller's client of this provider, built as at startup.
        async fn client(&self) -> OidcProvider {
            OidcProvider::discover(
                self.issuer.clone(),
                "fleet".to_owned(),
                "http://127.0.0.1/callback".to_owned(),
            )
            .await
            .unwrap()
        }

        /// From now on, holds every request without answering.
        fn stall(&self) {
            self.stalled.store(true, Ordering::SeqCst);
        }

        /// Waits until a request is held, and returns its path.
        async fn held_request(&mut self) -> String {
            bounded(self.held.recv())
                .await
                .expect("the identity provider stopped")
        }
    }

    /// The identity provider's server.
    #[derive(Clone)]
    struct Endpoints {
        issuer: String,
        stalled: Arc<AtomicBool>,
        hold: mpsc::UnboundedSender<String>,
    }

    impl Endpoints {
        async fn answer(self, uri: Uri) -> Response {
            if self.stalled.load(Ordering::SeqCst) {
                let _ = self.hold.send(uri.path().to_owned());
                return std::future::pending().await;
            }
            let issuer = &self.issuer;
            let body = match uri.path() {
                "/.well-known/openid-configuration" => json!({
                    "issuer": issuer,
                    "authorization_endpoint": format!("{issuer}/authorize"),
                    "token_endpoint": format!("{issuer}/token"),
                    "userinfo_endpoint": format!("{issuer}/userinfo"),
                    "jwks_uri": format!("{issuer}/keys"),
                    "device_authorization_endpoint": format!("{issuer}/device"),
                }),
                "/keys" => json!({ "keys": [{ "kty": "oct", "kid": "current", "k": "a2V5" }] }),
                "/userinfo" => json!({ "sub": SUBJECT }),
                _ => return StatusCode::NOT_FOUND.into_response(),
            };
            axum::Json(body).into_response()
        }
    }

    /// The controller's fleet service, signing users in with an
    /// `IdentityProvider`, with `DEVICE_ID` enrolled and a daemon configuration
    /// to send it.
    struct Controller {
        limits: FleetLimits,
        service: FleetAgentService,
        idp: IdentityProvider,
        database: PathBuf,
    }

    impl Controller {
        async fn start(limits: FleetLimits) -> Self {
            let idp = IdentityProvider::start().await;
            let database = std::env::temp_dir().join(format!(
                "agentdesktop-fleet-service-{}-{}.db",
                std::process::id(),
                rand::random::<u64>()
            ));
            let devices = Database::connect(&format!("sqlite://{}?mode=rwc", database.display()))
                .await
                .unwrap();
            devices
                .enroll_device(DEVICE_ID, "host", &idp.issuer, SUBJECT, None)
                .await
                .unwrap();
            let config = DaemonConfig {
                revision: 1,
                yaml: b"{}".to_vec(),
                sha256: Vec::new(),
            };
            let service = FleetAgentService::new(
                Some(idp.client().await),
                devices,
                DaemonConfigStore::new(Some(config)),
                None,
                None,
                &limits,
            );
            Self {
                limits,
                service,
                idp,
                database,
            }
        }

        /// Serves the fleet service on the fleet listener, over TLS with
        /// `pki`'s device CA.
        async fn serve(&self, pki: &Pki) -> (SocketAddr, Connections) {
            serve_mtls_with(self.limits, pki, self.service.clone()).await
        }

        fn enrollment(&self) -> Enrollment {
            Enrollment(self.service.clone())
        }
    }

    impl Drop for Controller {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.database);
        }
    }

    /// Calls a fleet service's enrollment handlers.
    #[derive(Clone)]
    struct Enrollment(FleetAgentService);

    impl Enrollment {
        /// Begins a browser enrollment, which needs nothing from the identity
        /// provider, and returns its ID.
        async fn begin(self) -> Result<String, Status> {
            let request = BeginEnrollmentRequest {
                hostname: "host".to_owned(),
                code_challenge: "c".repeat(43),
                device_authorization: false,
            };
            let response = self.0.begin_enrollment(Request::new(request)).await?;
            Ok(response.into_inner().enrollment_id)
        }

        /// Begins an enrollment with the device grant, which asks the identity
        /// provider for a device code.
        async fn begin_with_device_grant(self) -> Result<String, Status> {
            let request = BeginEnrollmentRequest {
                hostname: "host".to_owned(),
                code_challenge: String::new(),
                device_authorization: true,
            };
            let response = self.0.begin_enrollment(Request::new(request)).await?;
            Ok(response.into_inner().enrollment_id)
        }

        /// Completes `enrollment_id` with an ID token signed by a key the
        /// controller has not seen, so it asks the identity provider for its
        /// keys again before anything else.
        async fn complete(self, enrollment_id: String) -> Result<(), Status> {
            let header = json!({ "alg": "RS256", "kid": "rotated" }).to_string();
            let mut request = Request::new(CompleteEnrollmentRequest {
                enrollment_id,
                id_token: format!("{}.e30.c2ln", URL_SAFE_NO_PAD.encode(header)),
                ..Default::default()
            });
            request
                .metadata_mut()
                .insert("authorization", AUTHORIZATION.parse().unwrap());
            self.0.complete_enrollment(request).await.map(drop)
        }

        /// Asserts that every slot is taken: neither handler is admitted.
        async fn assert_full(self) {
            let begun = self.clone().begin().await.expect_err("an enrollment began");
            assert_eq!(begun.code(), Code::ResourceExhausted, "{begun}");
            let completed = self
                .complete(String::new())
                .await
                .expect_err("an enrollment completed");
            assert_eq!(completed.code(), Code::ResourceExhausted, "{completed}");
        }
    }

    /// A device's `Connect` session keeps its connection busy for as long as
    /// it lasts, through quiet spells longer than the idle limit, and frees it
    /// when it ends.
    #[tokio::test]
    async fn a_device_session_keeps_its_connection_busy_until_it_ends() {
        let mut limits = FLEET_LIMITS;
        limits.idle_connection = Duration::from_millis(200);
        let controller = Controller::start(limits).await;
        let pki = pki();
        let (address, connections) = controller.serve(&pki).await;

        let (outbound, receiver) = mpsc::channel::<AgentMessage>(1);
        let mut request = Request::new(ReceiverStream::new(receiver));
        request
            .metadata_mut()
            .insert("authorization", AUTHORIZATION.parse().unwrap());
        let mut session = bounded(tls_client(address, &pki, true).await.connect(request))
            .await
            .expect("open the session")
            .into_inner();
        let config = bounded(session.message())
            .await
            .expect("receive the configuration");
        assert!(
            config.is_some(),
            "the session ended before its configuration"
        );

        // Nothing is sent either way for several idle limits.
        let quiet = time::timeout(5 * limits.idle_connection, session.message()).await;
        assert!(quiet.is_err(), "the quiet session ended: {quiet:?}");
        assert_eq!(connections.open_count(), 1);

        // The device ends the session, so its connection goes idle and closes.
        drop(outbound);
        bounded(until_open_at_most(&connections, 0)).await;
    }

    /// `BeginEnrollment` holds its in-flight slot while it waits on the
    /// identity provider, and frees it when it ends.
    #[tokio::test]
    async fn a_beginning_enrollment_holds_its_slot_while_it_runs() {
        let mut controller = Controller::start(ONE_ENROLLMENT_SLOT).await;
        controller.idp.stall();
        let running = tokio::spawn(controller.enrollment().begin_with_device_grant());
        assert_eq!(controller.idp.held_request().await, "/device");

        controller.enrollment().assert_full().await;

        running.abort();
        let _ = running.await;
        controller
            .enrollment()
            .begin()
            .await
            .expect("the slot was not freed");
    }

    /// `CompleteEnrollment` holds its in-flight slot while it waits on the
    /// identity provider, and frees it when it ends.
    #[tokio::test]
    async fn a_completing_enrollment_holds_its_slot_while_it_runs() {
        let mut controller = Controller::start(ONE_ENROLLMENT_SLOT).await;
        let enrollment_id = controller.enrollment().begin().await.unwrap();
        controller.idp.stall();
        let running = tokio::spawn(controller.enrollment().complete(enrollment_id));
        assert_eq!(controller.idp.held_request().await, "/keys");

        controller.enrollment().assert_full().await;

        running.abort();
        let _ = running.await;
        controller
            .enrollment()
            .begin()
            .await
            .expect("the slot was not freed");
    }
}
