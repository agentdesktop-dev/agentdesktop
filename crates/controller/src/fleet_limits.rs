//! Bounds on the device-facing fleet API.
//!
//! The fleet listener has to be reachable by every enrolled device, and client
//! certificates are optional on it because enrollment runs before a device has
//! one. These limits keep a client from holding resources on it indefinitely.

use std::time::Duration;

use tonic::transport::{Server, ServerTlsConfig};

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
}

#[cfg(test)]
mod tests;
