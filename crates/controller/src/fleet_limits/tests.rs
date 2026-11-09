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

use super::{FLEET_LIMITS, FleetLimits};

/// Limits small enough for tests to observe; anything a test does not exercise
/// stays at the production value.
fn limits() -> FleetLimits {
    let mut limits = FLEET_LIMITS;
    limits.tls_handshake = Duration::from_millis(200);
    limits.request = Duration::from_millis(200);
    limits.keepalive_interval = Duration::from_millis(100);
    limits.keepalive_timeout = Duration::from_millis(100);
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
    let router = server.add_service(limits.fleet_service(service));
    tokio::spawn(router.serve_with_incoming(limits.limit_connections(TcpIncoming::from(listener))));
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

#[tokio::test]
async fn a_connection_that_stops_answering_pings_is_closed() {
    let address = serve(
        limits(),
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

fn one_at_a_time() -> FleetLimits {
    let mut limits = limits();
    limits.request = Duration::from_secs(5);
    limits
}

#[tokio::test]
async fn requests_beyond_the_per_connection_limit_wait() {
    let mut limits = one_at_a_time();
    limits.requests_per_connection = 1;

    let elapsed = two_calls_on_one_connection(limits).await;

    assert!(
        elapsed >= Duration::from_millis(950),
        "ran concurrently: {elapsed:?}"
    );
}

#[tokio::test]
async fn streams_beyond_the_per_connection_limit_wait() {
    let mut limits = one_at_a_time();
    limits.streams_per_connection = 1;

    let elapsed = two_calls_on_one_connection(limits).await;

    assert!(
        elapsed >= Duration::from_millis(950),
        "ran concurrently: {elapsed:?}"
    );
}

fn small_messages() -> FleetLimits {
    let mut limits = limits();
    limits.max_message_bytes = 1024;
    limits.enrollment_message_bytes = 128;
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

#[tokio::test]
async fn device_calls_keep_the_larger_message_limit() {
    let address = serve(
        small_messages(),
        None,
        Slow {
            delay: Duration::ZERO,
        },
    )
    .await;
    let mut client = client(address).await;

    // Over the enrollment limit but within the device limit: reaches the handler.
    let within = bounded(
        client.renew_device_certificate(RenewDeviceCertificateRequest {
            certificate_signing_request_der: vec![0; 512],
        }),
    )
    .await
    .expect_err("the test service rejects every renewal");
    assert_eq!(within.code(), Code::Unimplemented, "{within}");

    let over = bounded(
        client.renew_device_certificate(RenewDeviceCertificateRequest {
            certificate_signing_request_der: vec![0; 4096],
        }),
    )
    .await
    .expect_err("an oversized device request was accepted");
    assert_eq!(over.code(), Code::OutOfRange, "{over}");
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
    let mut limits = limits();
    limits.connections_per_address = 2;

    connections_over_cap(limits).await;
}

#[tokio::test]
async fn connections_over_the_total_cap_are_closed() {
    let mut limits = limits();
    limits.max_connections = 2;

    connections_over_cap(limits).await;
}

mod enrollment {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    use tokio::time::Instant;

    use super::super::{FLEET_LIMITS, FleetLimits, MAX_TRACKED_PEERS};
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
    async fn requests_without_a_peer_address_still_count_against_the_cap() {
        let limiter = limits(1, 60).enrollment();

        let _first = limiter.admit(None).expect("first request");
        assert!(limiter.admit(None).is_err());
    }

    #[test]
    fn idle_peers_are_dropped_when_the_table_is_full() {
        let limiter = limits(64, 1).enrollment();
        let start = Instant::now();
        for index in 0..MAX_TRACKED_PEERS {
            let [a, b, c, d] = u32::try_from(index).unwrap().to_be_bytes();
            assert!(limiter.take(IpAddr::V4(Ipv4Addr::new(10 | a, b, c, d)), start));
        }

        // Once every bucket has refilled, a new peer evicts the idle ones.
        assert!(limiter.take(peer(1), start + Duration::from_secs(10)));
        assert_eq!(limiter.peers.lock().unwrap().len(), 1);
    }
}
