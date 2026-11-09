//! Bounds on the device-facing fleet API.
//!
//! The fleet listener has to be reachable by every enrolled device, and client
//! certificates are optional on it because enrollment runs before a device has
//! one. These limits keep a client from holding resources on it indefinitely.

use std::{
    collections::HashMap,
    convert::Infallible,
    io,
    net::{IpAddr, Ipv6Addr, SocketAddr},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use agentdesktop_core::config::ControllerFleetLimitsConfig;
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
#[derive(Clone, Copy, Debug, PartialEq)]
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
    /// How long a connection may be silent before TCP keepalive probes start,
    /// for peers that vanish without closing.
    pub tcp_keepalive: Duration,
    /// Open connections allowed in total and from one client address (an IPv4
    /// address or an IPv6 /64). A connection over either cap is closed as soon
    /// as it is accepted. The
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
    /// Per-client-address `BeginEnrollment` budget: up to `enrollment_burst`
    /// requests at once, refilled at one request per `enrollment_refill`.
    /// Generous, because many devices can share one address behind NAT.
    pub enrollment_burst: u32,
    pub enrollment_refill: Duration,
    /// How long `CompleteEnrollment` waits for an in-flight slot before it is
    /// refused. It waits rather than failing at once because the user has
    /// already signed in, and a refusal makes them start over.
    pub enrollment_completion_wait: Duration,
    /// Messages a device may send on its `Connect` streams: up to
    /// `device_message_burst` at once, refilled at one per
    /// `device_message_refill`. The budget belongs to the device, so
    /// reconnecting does not reset it. Devices send a heartbeat every 30
    /// seconds, inventory periodically and one telemetry event per tool event,
    /// so this is far above normal use; a stream over budget is closed.
    pub device_message_burst: u32,
    pub device_message_refill: Duration,
    /// How often a device may open a `Connect` stream: up to
    /// `stream_open_burst` at once, refilled at one per `stream_open_refill`.
    /// Each open checks the access token with the identity provider.
    pub stream_open_burst: u32,
    pub stream_open_refill: Duration,
}

/// Default limits. The controller configuration can change the
/// deployment-dependent ones (see `configured`).
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
    enrollment_completion_wait: Duration::from_secs(10),
    device_message_burst: 600,
    device_message_refill: Duration::from_millis(50),
    stream_open_burst: 20,
    stream_open_refill: Duration::from_secs(60),
};

impl FleetLimits {
    /// These limits, with the deployment-dependent ones from the controller
    /// configuration.
    pub fn configured(mut self, config: &ControllerFleetLimitsConfig) -> Self {
        self.max_connections = config.max_connections;
        self.connections_per_address = config.connections_per_address;
        self.max_message_bytes = config.max_message_bytes;
        self.enrollment_in_flight = config.enrollment_in_flight;
        self.enrollment_burst = config.enrollment_burst;
        self.enrollment_refill = config.enrollment_refill;
        self.device_message_burst = config.device_message_burst;
        self.device_message_refill = config.device_message_refill;
        self
    }

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

    /// Per-device `Connect` state: one stream per device, and its budgets.
    pub fn device_streams(&self) -> DeviceStreams {
        DeviceStreams::new(self)
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

/// The address a per-address limit counts against: an IPv4 address, or the /64
/// network of an IPv6 one, since one IPv6 host usually controls a whole /64.
pub(crate) fn address_key(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V4(_) => address,
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & (u128::MAX << 64))),
        },
    }
}

/// Admission control for the unauthenticated enrollment RPCs.
#[derive(Clone)]
pub struct EnrollmentLimiter {
    in_flight: Arc<Semaphore>,
    peers: Arc<Mutex<PeerBuckets>>,
    burst: f64,
    refill: Duration,
    completion_wait: Duration,
}

/// Enrollment buckets by address, in two generations. When the current
/// generation fills up it becomes the previous one, and the old previous one is
/// dropped; a peer found in the previous generation moves back to the current
/// one. Active peers keep their buckets, and the table never holds more than
/// two generations.
#[derive(Default)]
struct PeerBuckets {
    current: HashMap<IpAddr, Bucket>,
    previous: HashMap<IpAddr, Bucket>,
}

const PEERS_PER_GENERATION: usize = 10_000;

impl PeerBuckets {
    fn bucket(&mut self, peer: IpAddr, fresh: Bucket) -> &mut Bucket {
        if !self.current.contains_key(&peer) {
            let bucket = self.previous.remove(&peer).unwrap_or(fresh);
            if self.current.len() >= PEERS_PER_GENERATION {
                self.previous = std::mem::take(&mut self.current);
            }
            self.current.insert(peer, bucket);
        }
        self.current
            .get_mut(&peer)
            .expect("bucket was just inserted")
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.current.len() + self.previous.len()
    }
}

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
            completion_wait: limits.enrollment_completion_wait,
        }
    }

    /// Admits one `BeginEnrollment` from `peer`, or rejects it with
    /// `RESOURCE_EXHAUSTED`. Hold the returned permit until the request finishes.
    pub fn admit(&self, peer: Option<SocketAddr>) -> Result<OwnedSemaphorePermit, Status> {
        if let Some(peer) = peer
            && !self.take(peer.ip(), Instant::now())
        {
            if let Some(events) = ENROLLMENT_RATE.event(Instant::now()) {
                tracing::warn!(
                    peer = %address_key(peer.ip()),
                    events,
                    "refused enrollment requests over the per-address rate limit"
                );
            }
            return Err(Status::resource_exhausted(
                "too many enrollment requests from this address; retry later",
            ));
        }
        self.in_flight
            .clone()
            .try_acquire_owned()
            .map_err(|_| in_flight_refusal())
    }

    /// Admits one `CompleteEnrollment`, waiting up to `enrollment_completion_wait`
    /// for an in-flight slot. It does not spend the address's budget: completing
    /// needs an enrollment that `BeginEnrollment` started (and paid for), and each
    /// enrollment completes at most once, so an unknown one fails at a lookup.
    pub async fn admit_completion(&self) -> Result<OwnedSemaphorePermit, Status> {
        match tokio::time::timeout(self.completion_wait, self.in_flight.clone().acquire_owned())
            .await
        {
            Ok(Ok(permit)) => Ok(permit),
            _ => Err(in_flight_refusal()),
        }
    }

    fn take(&self, peer: IpAddr, now: Instant) -> bool {
        let mut peers = self.peers.lock().expect("enrollment limiter lock poisoned");
        let bucket = peers.bucket(
            address_key(peer),
            Bucket {
                tokens: self.burst,
                updated: now,
            },
        );
        take_token(bucket, now, self.refill, self.burst)
    }
}

fn in_flight_refusal() -> Status {
    if let Some(events) = ENROLLMENT_IN_FLIGHT.event(Instant::now()) {
        tracing::warn!(
            events,
            "refused enrollment requests with every in-flight slot taken"
        );
    }
    Status::resource_exhausted("too many enrollment requests in progress; retry later")
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

/// `Connect` state by device. A device has at most one open stream: opening a
/// new stream tells the device's previous one to close, so a device that
/// reconnects (after sleep, for example) is never blocked by a stream the
/// controller has not yet noticed is dead. The device's message budget and its
/// budget for opening streams live here too, so reconnecting does not reset them.
#[derive(Clone)]
pub struct DeviceStreams {
    devices: Arc<Mutex<HashMap<String, DeviceState>>>,
    next: Arc<AtomicU64>,
    limits: FleetLimits,
}

struct DeviceState {
    /// The open stream's generation and the signal that replaces it.
    current: Option<(u64, oneshot::Sender<()>)>,
    messages: Bucket,
    opens: Bucket,
}

/// Device entries kept before idle ones (no open stream) are dropped, or twice
/// `max_connections` if that is more: each open stream holds a connection, so
/// dropping idle entries always frees at least half the table. Only devices
/// with a certificate from the device CA get an entry.
const MAX_TRACKED_DEVICES: usize = 100_000;

/// A registered `Connect` stream. `superseded` resolves when the same device
/// opens a newer stream; dropping this unregisters it.
pub struct DeviceStream {
    streams: DeviceStreams,
    device_id: String,
    generation: u64,
    pub superseded: oneshot::Receiver<()>,
}

impl DeviceStreams {
    pub fn new(limits: &FleetLimits) -> Self {
        Self {
            devices: Arc::default(),
            next: Arc::default(),
            limits: *limits,
        }
    }

    fn with_device<T>(&self, device_id: &str, f: impl FnOnce(&mut DeviceState) -> T) -> T {
        let mut devices = self
            .devices
            .lock()
            .expect("device stream registry lock poisoned");
        let max_devices = MAX_TRACKED_DEVICES.max(self.limits.max_connections.saturating_mul(2));
        if devices.len() >= max_devices && !devices.contains_key(device_id) {
            devices.retain(|_, device| device.current.is_some());
        }
        let now = Instant::now();
        let device = devices
            .entry(device_id.to_owned())
            .or_insert_with(|| DeviceState {
                current: None,
                messages: Bucket {
                    tokens: f64::from(self.limits.device_message_burst),
                    updated: now,
                },
                opens: Bucket {
                    tokens: f64::from(self.limits.stream_open_burst),
                    updated: now,
                },
            });
        f(device)
    }

    /// Admits one more `Connect` from `device_id`, or rejects it with
    /// `RESOURCE_EXHAUSTED` when the device opens streams too often. Call this
    /// before any per-stream work, such as checking the access token.
    pub fn admit_open(&self, device_id: &str) -> Result<(), Status> {
        let limits = self.limits;
        let admitted = self.with_device(device_id, |device| {
            take_token(
                &mut device.opens,
                Instant::now(),
                limits.stream_open_refill,
                f64::from(limits.stream_open_burst),
            )
        });
        if admitted {
            Ok(())
        } else {
            if let Some(events) = STREAM_OPENS.event(Instant::now()) {
                tracing::warn!(
                    device_id,
                    events,
                    "refused Connect streams from devices that open them too often"
                );
            }
            Err(Status::resource_exhausted(
                "this device opened too many streams; retry later",
            ))
        }
    }

    pub fn open(&self, device_id: &str) -> DeviceStream {
        let generation = self.next.fetch_add(1, Ordering::Relaxed);
        let (replace, superseded) = oneshot::channel();
        let previous = self.with_device(device_id, |device| {
            device.current.replace((generation, replace))
        });
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

impl DeviceStream {
    /// Takes one message from the device's budget, or returns false when it is spent.
    pub fn take_message(&self, now: Instant) -> bool {
        let limits = self.streams.limits;
        self.streams.with_device(&self.device_id, |device| {
            take_token(
                &mut device.messages,
                now,
                limits.device_message_refill,
                f64::from(limits.device_message_burst),
            )
        })
    }
}

impl Drop for DeviceStream {
    fn drop(&mut self) {
        let mut devices = self
            .streams
            .devices
            .lock()
            .expect("device stream registry lock poisoned");
        // A newer stream for the same device keeps its registration.
        if let Some(device) = devices.get_mut(&self.device_id)
            && device
                .current
                .as_ref()
                .is_some_and(|(generation, _)| *generation == self.generation)
        {
            device.current = None;
        }
    }
}

fn refilled(bucket: Bucket, now: Instant, refill: Duration, burst: f64) -> f64 {
    let elapsed = now.saturating_duration_since(bucket.updated);
    (bucket.tokens + elapsed.as_secs_f64() / refill.as_secs_f64()).min(burst)
}

/// A warning that fires once per refused request or connection. A flood would
/// log a line per event, so this logs at most one per `WARNING_INTERVAL` and
/// says how many events it stands for.
pub(crate) struct ThrottledWarning {
    state: Mutex<WarningState>,
}

struct WarningState {
    last: Option<Instant>,
    events: u64,
}

const WARNING_INTERVAL: Duration = Duration::from_secs(10);

impl ThrottledWarning {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(WarningState {
                last: None,
                events: 0,
            }),
        }
    }

    /// Counts one event. Returns the number of events since the last warning,
    /// including this one, when it is time to log another.
    pub(crate) fn event(&self, now: Instant) -> Option<u64> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.events += 1;
        if state
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < WARNING_INTERVAL)
        {
            return None;
        }
        state.last = Some(now);
        Some(std::mem::take(&mut state.events))
    }
}

static ENROLLMENT_RATE: ThrottledWarning = ThrottledWarning::new();
static ENROLLMENT_IN_FLIGHT: ThrottledWarning = ThrottledWarning::new();
static STREAM_OPENS: ThrottledWarning = ThrottledWarning::new();

#[cfg(test)]
mod tests;
