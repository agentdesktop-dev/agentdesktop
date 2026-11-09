//! Bounds on the device-facing fleet API.
//!
//! The fleet listener has to be reachable by every enrolled device, and client
//! certificates are optional on it because enrollment runs before a device has
//! one. These limits keep a client from holding resources on it indefinitely.

use std::{
    collections::HashMap,
    convert::Infallible,
    io,
    net::{IpAddr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use agentdesktop_proto::fleet::fleet_agent_server::{FleetAgent, FleetAgentServer, SERVICE_NAME};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, oneshot},
    time::Instant,
};
use tonic::{
    Status,
    codegen::{Service, http},
    server::NamedService,
    transport::{
        Server, ServerTlsConfig,
        server::{TcpConnectInfo, TcpIncoming, TlsConnectInfo},
    },
};
use tower_layer::{Identity, Stack};

mod connections;

pub use connections::{Connections, LimitedConnection, LimitedIncoming, TrackRequests};

/// Limits applied to the fleet gRPC listener.
#[derive(Clone, Copy, Debug)]
pub struct FleetLimits {
    /// Time a client has to finish the TLS handshake after connecting. Tonic sets
    /// no limit by default, so a client could hold a connection open forever by
    /// never sending its ClientHello.
    pub tls_handshake: Duration,
    /// Time a request handler has to produce its response. A streaming RPC's
    /// response is produced once the stream opens, so this does not end
    /// established streams such as `Connect`.
    pub request: Duration,
    /// How often an HTTP/2 PING checks a connection, and how long an unanswered
    /// PING waits before the connection is closed.
    pub keepalive_interval: Duration,
    pub keepalive_timeout: Duration,
    /// TCP keepalive probe interval, for peers that vanish without closing.
    pub tcp_keepalive: Duration,
    /// Open connections allowed in total and from one client address. A
    /// connection over either cap is closed as soon as it is accepted. The
    /// per-address cap is high because many devices can share an address behind
    /// NAT, or appear to when a load balancer rewrites client addresses.
    pub max_connections: usize,
    pub connections_per_address: usize,
    /// Time a new connection has to make its first request, and time a
    /// connection may stay without any request in progress after its last one
    /// finished. A connection with a request in progress (including an open
    /// `Connect` stream) is never idle. At the total cap, the longest-idle
    /// connection is closed to admit a new one.
    pub first_request: Duration,
    pub idle_connection: Duration,
    /// Concurrent requests one connection may have in progress, and concurrent
    /// HTTP/2 streams it may open. Further requests wait, so one connection
    /// cannot occupy an unbounded share of the server.
    pub requests_per_connection: usize,
    pub streams_per_connection: u32,
    /// Largest encoded message an enrolled device's calls may send or receive.
    pub max_message_bytes: usize,
    /// Largest request from a caller without a device certificate: enrollment,
    /// which runs before a device has one, and any other call made without one
    /// (rejected by its handler, but only after the request has been decoded).
    /// Enrollment requests are small (a hostname and a CSR).
    pub unauthenticated_message_bytes: usize,
    /// Enrollment requests (`BeginEnrollment`, `CompleteEnrollment`) running at
    /// once across all clients. Enrollment calls the identity provider, so this
    /// also bounds the load a flood can put on it.
    pub enrollment_in_flight: usize,
    /// Per-client-address enrollment budget: up to `enrollment_burst` requests at
    /// once, refilled at one request per `enrollment_refill`. Generous, because
    /// many devices can share one address behind NAT.
    pub enrollment_burst: u32,
    pub enrollment_refill: Duration,
    /// Messages a device may send on one `Connect` stream: up to
    /// `stream_message_burst` at once, refilled at one per
    /// `stream_message_refill`. Devices send a heartbeat every 30 seconds,
    /// inventory periodically and one telemetry event per tool event, so this is
    /// far above normal use; a stream over budget is closed.
    pub stream_message_burst: u32,
    pub stream_message_refill: Duration,
}

/// Production limits.
pub const FLEET_LIMITS: FleetLimits = FleetLimits {
    tls_handshake: Duration::from_secs(10),
    request: Duration::from_secs(30),
    keepalive_interval: Duration::from_secs(30),
    keepalive_timeout: Duration::from_secs(20),
    tcp_keepalive: Duration::from_secs(60),
    max_connections: 50_000,
    connections_per_address: 4_096,
    first_request: Duration::from_secs(20),
    idle_connection: Duration::from_secs(120),
    requests_per_connection: 32,
    streams_per_connection: 64,
    max_message_bytes: 4 * 1024 * 1024,
    unauthenticated_message_bytes: 64 * 1024,
    enrollment_in_flight: 64,
    enrollment_burst: 60,
    enrollment_refill: Duration::from_secs(1),
    stream_message_burst: 600,
    stream_message_refill: Duration::from_millis(50),
};

impl FleetLimits {
    /// Adds the handshake timeout to the fleet listener's TLS settings.
    pub fn tls(&self, config: ServerTlsConfig) -> ServerTlsConfig {
        config.timeout(self.tls_handshake)
    }

    /// The registry that admits fleet connections and tracks their activity.
    /// Pass the same one to `server` and `incoming`.
    pub fn connections(&self) -> Connections {
        Connections::new(self)
    }

    /// A server builder with the fleet listener's limits applied.
    pub fn server(&self, connections: &Connections) -> Server<Stack<TrackRequests, Identity>> {
        Server::builder()
            .timeout(self.request)
            .http2_keepalive_interval(Some(self.keepalive_interval))
            .http2_keepalive_timeout(Some(self.keepalive_timeout))
            .concurrency_limit_per_connection(self.requests_per_connection)
            .max_concurrent_streams(self.streams_per_connection)
            .layer(connections.track_requests())
    }

    /// Binds the fleet listener with TCP keepalive, admitting connections
    /// through `connections`. Serve with `Server::serve_with_incoming`.
    pub fn incoming(
        &self,
        address: SocketAddr,
        connections: &Connections,
    ) -> io::Result<LimitedIncoming<TcpIncoming>> {
        let incoming = TcpIncoming::bind(address)?
            .with_nodelay(Some(true))
            .with_keepalive(Some(self.tcp_keepalive));
        Ok(connections.incoming(incoming))
    }

    /// The fleet gRPC service with message size limits applied.
    pub fn fleet_service<T: FleetAgent>(&self, service: T) -> FleetService<T> {
        let service = Arc::new(service);
        FleetService {
            unauthenticated: FleetAgentServer::from_arc(service.clone())
                .max_decoding_message_size(self.unauthenticated_message_bytes)
                .max_encoding_message_size(self.max_message_bytes),
            devices: FleetAgentServer::from_arc(service)
                .max_decoding_message_size(self.max_message_bytes)
                .max_encoding_message_size(self.max_message_bytes),
        }
    }

    pub fn enrollment(&self) -> EnrollmentLimiter {
        EnrollmentLimiter::new(self)
    }

    /// A fresh message budget for one `Connect` stream.
    pub fn stream_messages(&self) -> MessageBudget {
        MessageBudget {
            bucket: Bucket {
                tokens: f64::from(self.stream_message_burst),
                updated: Instant::now(),
            },
            burst: f64::from(self.stream_message_burst),
            refill: self.stream_message_refill,
        }
    }
}

/// The fleet gRPC service, with a smaller request limit for enrollment and for
/// callers without a device certificate. Tonic sets message limits per service,
/// so those requests go to their own instance of the generated server.
pub struct FleetService<T> {
    unauthenticated: FleetAgentServer<T>,
    devices: FleetAgentServer<T>,
}

impl<T> Clone for FleetService<T> {
    fn clone(&self) -> Self {
        Self {
            unauthenticated: self.unauthenticated.clone(),
            devices: self.devices.clone(),
        }
    }
}

fn is_enrollment(path: &str) -> bool {
    path.strip_prefix('/')
        .and_then(|path| path.strip_prefix(SERVICE_NAME))
        .is_some_and(|method| matches!(method, "/BeginEnrollment" | "/CompleteEnrollment"))
}

/// Whether the TLS client presented a certificate signed by the device CA
/// (the handshake rejects any other).
fn has_device_certificate<B>(request: &http::Request<B>) -> bool {
    request
        .extensions()
        .get::<TlsConnectInfo<TcpConnectInfo>>()
        .and_then(TlsConnectInfo::peer_certs)
        .is_some_and(|certificates| !certificates.is_empty())
}

impl<T, B> Service<http::Request<B>> for FleetService<T>
where
    FleetAgentServer<T>: Service<http::Request<B>, Error = Infallible>,
{
    type Response = <FleetAgentServer<T> as Service<http::Request<B>>>::Response;
    type Error = Infallible;
    type Future = <FleetAgentServer<T> as Service<http::Request<B>>>::Future;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.unauthenticated.poll_ready(context) {
            Poll::Ready(Ok(())) => self.devices.poll_ready(context),
            other => other,
        }
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        if is_enrollment(request.uri().path()) || !has_device_certificate(&request) {
            self.unauthenticated.call(request)
        } else {
            self.devices.call(request)
        }
    }
}

impl<T> NamedService for FleetService<T> {
    const NAME: &'static str = SERVICE_NAME;
}

/// Admission control for the unauthenticated enrollment RPCs.
#[derive(Clone)]
pub struct EnrollmentLimiter {
    in_flight: Arc<Semaphore>,
    peers: Arc<Mutex<HashMap<IpAddr, Bucket>>>,
    burst: f64,
    refill: Duration,
}

/// Peer buckets kept before idle (full) ones are dropped.
const MAX_TRACKED_PEERS: usize = 10_000;

#[derive(Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl EnrollmentLimiter {
    fn new(limits: &FleetLimits) -> Self {
        Self {
            in_flight: Arc::new(Semaphore::new(limits.enrollment_in_flight)),
            peers: Arc::default(),
            burst: f64::from(limits.enrollment_burst),
            refill: limits.enrollment_refill,
        }
    }

    /// Admits one enrollment request from `peer`, or rejects it with
    /// `RESOURCE_EXHAUSTED`. Hold the returned permit until the request finishes.
    pub fn admit(&self, peer: Option<SocketAddr>) -> Result<OwnedSemaphorePermit, Status> {
        if let Some(peer) = peer
            && !self.take(peer.ip(), Instant::now())
        {
            tracing::warn!(peer = %peer.ip(), "enrollment rate limit exceeded");
            return Err(Status::resource_exhausted(
                "too many enrollment requests from this address; retry later",
            ));
        }
        self.in_flight.clone().try_acquire_owned().map_err(|_| {
            tracing::warn!("enrollment concurrency limit reached");
            Status::resource_exhausted("too many enrollment requests in progress; retry later")
        })
    }

    fn take(&self, peer: IpAddr, now: Instant) -> bool {
        let mut peers = self.peers.lock().expect("enrollment limiter lock poisoned");
        if peers.len() >= MAX_TRACKED_PEERS && !peers.contains_key(&peer) {
            let burst = self.burst;
            let refill = self.refill;
            peers.retain(|_, bucket| refilled(*bucket, now, refill, burst) < burst);
        }
        let bucket = peers.entry(peer).or_insert(Bucket {
            tokens: self.burst,
            updated: now,
        });
        take_token(bucket, now, self.refill, self.burst)
    }
}

/// Refills `bucket` for the time since its last use, then takes one token.
fn take_token(bucket: &mut Bucket, now: Instant, refill: Duration, burst: f64) -> bool {
    bucket.tokens = refilled(*bucket, now, refill, burst);
    bucket.updated = now;
    if bucket.tokens < 1.0 {
        return false;
    }
    bucket.tokens -= 1.0;
    true
}

/// Messages a device may send on one `Connect` stream.
pub struct MessageBudget {
    bucket: Bucket,
    burst: f64,
    refill: Duration,
}

impl MessageBudget {
    /// Takes one message from the budget, or returns false when it is spent.
    pub fn take(&mut self, now: Instant) -> bool {
        take_token(&mut self.bucket, now, self.refill, self.burst)
    }
}

/// Open `Connect` streams by device. A device has at most one: opening a new
/// stream tells the device's previous stream to close. A device that reconnects
/// (after sleep, for example) is never blocked by a stream the controller has
/// not yet noticed is dead.
#[derive(Clone, Default)]
pub struct DeviceStreams {
    open: Arc<Mutex<HashMap<String, Registration>>>,
    next: Arc<AtomicU64>,
}

/// A device's current stream: its generation and the signal that replaces it.
type Registration = (u64, oneshot::Sender<()>);

/// A registered `Connect` stream. `superseded` resolves when the same device
/// opens a newer stream; dropping this unregisters it.
pub struct DeviceStream {
    streams: DeviceStreams,
    device_id: String,
    generation: u64,
    pub superseded: oneshot::Receiver<()>,
}

impl DeviceStreams {
    pub fn open(&self, device_id: &str) -> DeviceStream {
        let generation = self.next.fetch_add(1, Ordering::Relaxed);
        let (replace, superseded) = oneshot::channel();
        let previous = self
            .open
            .lock()
            .expect("device stream registry lock poisoned")
            .insert(device_id.to_owned(), (generation, replace));
        if let Some((_, previous)) = previous {
            let _ = previous.send(());
        }
        DeviceStream {
            streams: self.clone(),
            device_id: device_id.to_owned(),
            generation,
            superseded,
        }
    }
}

impl Drop for DeviceStream {
    fn drop(&mut self) {
        let mut open = self
            .streams
            .open
            .lock()
            .expect("device stream registry lock poisoned");
        // A newer stream for the same device keeps its registration.
        if open
            .get(&self.device_id)
            .is_some_and(|(generation, _)| *generation == self.generation)
        {
            open.remove(&self.device_id);
        }
    }
}

fn refilled(bucket: Bucket, now: Instant, refill: Duration, burst: f64) -> f64 {
    let elapsed = now.saturating_duration_since(bucket.updated);
    (bucket.tokens + elapsed.as_secs_f64() / refill.as_secs_f64()).min(burst)
}

#[cfg(test)]
mod tests;
