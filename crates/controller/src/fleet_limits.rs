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
}

/// Production limits.
pub const FLEET_LIMITS: FleetLimits = FleetLimits {
    tls_handshake: Duration::from_secs(10),
    request: Duration::from_secs(30),
};

impl FleetLimits {
    /// Adds the handshake timeout to the fleet listener's TLS settings.
    pub fn tls(&self, config: ServerTlsConfig) -> ServerTlsConfig {
        config.timeout(self.tls_handshake)
    }

    /// A server builder with the fleet listener's limits applied.
    pub fn server(&self) -> Server {
        Server::builder().timeout(self.request)
    }
}

#[cfg(test)]
mod tests;
