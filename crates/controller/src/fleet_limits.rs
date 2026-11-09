//! Bounds on the device-facing fleet API.
//!
//! The fleet listener has to be reachable by every enrolled device, and client
//! certificates are optional on it because enrollment runs before a device has
//! one. These limits keep a client from holding resources on it indefinitely.

use std::{
    collections::HashMap,
    convert::Infallible,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use agentdesktop_proto::fleet::fleet_agent_server::{FleetAgent, FleetAgentServer, SERVICE_NAME};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore},
    time::Instant,
};
use tonic::{
    Status,
    codegen::{Service, http},
    server::NamedService,
    transport::{Server, ServerTlsConfig},
};

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
    /// Concurrent requests one connection may have in progress, and concurrent
    /// HTTP/2 streams it may open. Further requests wait, so one connection
    /// cannot occupy an unbounded share of the server.
    pub requests_per_connection: usize,
    pub streams_per_connection: u32,
    /// Largest encoded message an enrolled device's calls may send or receive.
    pub max_message_bytes: usize,
    /// Largest enrollment request. Enrollment runs before a device is
    /// authenticated, and its requests are small (a hostname and a CSR).
    pub enrollment_message_bytes: usize,
    /// Enrollment requests (`BeginEnrollment`, `CompleteEnrollment`) running at
    /// once across all clients. Enrollment calls the identity provider, so this
    /// also bounds the load a flood can put on it.
    pub enrollment_in_flight: usize,
    /// Per-client-address enrollment budget: up to `enrollment_burst` requests at
    /// once, refilled at one request per `enrollment_refill`. Generous, because
    /// many devices can share one address behind NAT.
    pub enrollment_burst: u32,
    pub enrollment_refill: Duration,
}

/// Production limits.
pub const FLEET_LIMITS: FleetLimits = FleetLimits {
    tls_handshake: Duration::from_secs(10),
    request: Duration::from_secs(30),
    keepalive_interval: Duration::from_secs(30),
    keepalive_timeout: Duration::from_secs(20),
    tcp_keepalive: Duration::from_secs(60),
    requests_per_connection: 32,
    streams_per_connection: 64,
    max_message_bytes: 4 * 1024 * 1024,
    enrollment_message_bytes: 64 * 1024,
    enrollment_in_flight: 64,
    enrollment_burst: 60,
    enrollment_refill: Duration::from_secs(1),
};

impl FleetLimits {
    /// Adds the handshake timeout to the fleet listener's TLS settings.
    pub fn tls(&self, config: ServerTlsConfig) -> ServerTlsConfig {
        config.timeout(self.tls_handshake)
    }

    /// A server builder with the fleet listener's limits applied.
    pub fn server(&self) -> Server {
        Server::builder()
            .timeout(self.request)
            .http2_keepalive_interval(Some(self.keepalive_interval))
            .http2_keepalive_timeout(Some(self.keepalive_timeout))
            .tcp_keepalive(Some(self.tcp_keepalive))
            .concurrency_limit_per_connection(self.requests_per_connection)
            .max_concurrent_streams(self.streams_per_connection)
    }

    /// The fleet gRPC service with message size limits applied.
    pub fn fleet_service<T: FleetAgent>(&self, service: T) -> FleetService<T> {
        let service = Arc::new(service);
        FleetService {
            enrollment: FleetAgentServer::from_arc(service.clone())
                .max_decoding_message_size(self.enrollment_message_bytes)
                .max_encoding_message_size(self.max_message_bytes),
            devices: FleetAgentServer::from_arc(service)
                .max_decoding_message_size(self.max_message_bytes)
                .max_encoding_message_size(self.max_message_bytes),
        }
    }

    pub fn enrollment(&self) -> EnrollmentLimiter {
        EnrollmentLimiter::new(self)
    }
}

/// The fleet gRPC service, with a smaller request limit for the enrollment RPCs.
/// Tonic sets message limits per service, so enrollment requests go to their own
/// instance of the generated server.
pub struct FleetService<T> {
    enrollment: FleetAgentServer<T>,
    devices: FleetAgentServer<T>,
}

impl<T> Clone for FleetService<T> {
    fn clone(&self) -> Self {
        Self {
            enrollment: self.enrollment.clone(),
            devices: self.devices.clone(),
        }
    }
}

fn is_enrollment(path: &str) -> bool {
    path.strip_prefix('/')
        .and_then(|path| path.strip_prefix(SERVICE_NAME))
        .is_some_and(|method| matches!(method, "/BeginEnrollment" | "/CompleteEnrollment"))
}

impl<T, B> Service<http::Request<B>> for FleetService<T>
where
    FleetAgentServer<T>: Service<http::Request<B>, Error = Infallible>,
{
    type Response = <FleetAgentServer<T> as Service<http::Request<B>>>::Response;
    type Error = Infallible;
    type Future = <FleetAgentServer<T> as Service<http::Request<B>>>::Future;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        match self.enrollment.poll_ready(context) {
            Poll::Ready(Ok(())) => self.devices.poll_ready(context),
            other => other,
        }
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        if is_enrollment(request.uri().path()) {
            self.enrollment.call(request)
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
        bucket.tokens = refilled(*bucket, now, self.refill, self.burst);
        bucket.updated = now;
        if bucket.tokens < 1.0 {
            return false;
        }
        bucket.tokens -= 1.0;
        true
    }
}

fn refilled(bucket: Bucket, now: Instant, refill: Duration, burst: f64) -> f64 {
    let elapsed = now.saturating_duration_since(bucket.updated);
    (bucket.tokens + elapsed.as_secs_f64() / refill.as_secs_f64()).min(burst)
}

#[cfg(test)]
mod tests;
