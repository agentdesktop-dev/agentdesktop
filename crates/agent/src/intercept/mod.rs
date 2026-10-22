//! Local intercepting proxy for first-party Claude Desktop.
//!
//! Claude Desktop is pinned to this listener with its `egressProxyUrl` managed
//! setting, so the app, its agent and Cowork's sandbox send every connection
//! here as an HTTP `CONNECT`. For the intercepted Anthropic hosts the listener
//! terminates TLS with a leaf from the [`InterceptCa`] and forwards each
//! request to the configured gateway with one added identity header
//! ([`IDENTITY_HEADER`]); the user's own `Cookie` and `Authorization` pass
//! through unchanged, so the claude.ai sign-in is the same as without a
//! gateway. Every other host is tunnelled without decryption.
//!
//! Nothing here connects to an intercepted host directly: when the gateway
//! cannot be reached the request fails (fail closed).
//!
//! The listener is built to keep working on a laptop that sleeps and changes
//! networks:
//! - every connection resolves its host through the system resolver at connect
//!   time, so a DNS server change takes effect at once;
//! - pooled gateway connections are dropped after a wall-clock idle gap, which
//!   also covers a sleep, and a request without a body that fails before any
//!   response is sent once more;
//! - [`Health`] records forwarded requests and the last failure, for the
//!   daemon status.

use std::{
    io,
    net::SocketAddr,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime},
};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Response, StatusCode, header::CONTENT_TYPE};
use tokio::net::TcpStream;

mod ca;
mod forward;
mod listener;
mod run;
mod store;
#[cfg(test)]
mod walkthrough;

pub use ca::InterceptCa;
pub use forward::{CredentialSource, GatewayTarget, IDENTITY_HEADER};
pub use listener::{InterceptConfig, Interceptor, serve};
pub use run::Status;
pub(crate) use run::supervise;
pub use store::{certificate_path, fingerprint};

/// The hosts the daemon decrypts and forwards to the gateway. Every other host
/// is tunnelled.
pub const INTERCEPTED_HOSTS: [&str; 2] = ["claude.ai", "api.anthropic.com"];

/// How long one connection attempt to a resolved address may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A tunnel that an upgrade (a `CONNECT` or a WebSocket) leaves behind. The
/// connection task awaits it after hyper hands the connection over, so every
/// tunnel runs under the server's task set and stops with the server.
pub(crate) type PendingTunnel =
    Arc<Mutex<Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>>>;

/// What the listener has done, for the daemon status. Counts only: no hosts
/// beyond the failure message, no paths, no headers.
#[derive(Debug, Default)]
pub struct Health {
    state: Mutex<HealthSnapshot>,
}

#[derive(Clone, Debug, Default)]
pub struct HealthSnapshot {
    /// Requests that the gateway answered.
    pub forwarded: u64,
    /// Requests that failed because the gateway or its credential was not available.
    pub gateway_failures: u64,
    /// Connections tunnelled without decryption.
    pub tunnels: u64,
    /// Tunnels that could not reach their destination.
    pub tunnel_failures: u64,
    /// When the gateway last answered.
    pub last_forwarded: Option<SystemTime>,
    /// The last failure, with its time.
    pub last_failure: Option<(SystemTime, String)>,
}

impl Health {
    pub fn snapshot(&self) -> HealthSnapshot {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn update(&self, change: impl FnOnce(&mut HealthSnapshot)) {
        change(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner));
    }

    fn record_forwarded(&self) {
        self.update(|state| {
            state.forwarded += 1;
            state.last_forwarded = Some(SystemTime::now());
        });
    }

    fn record_gateway_failure(&self, message: String) {
        self.update(|state| {
            state.gateway_failures += 1;
            state.last_failure = Some((SystemTime::now(), message));
        });
    }

    fn record_tunnel(&self) {
        self.update(|state| state.tunnels += 1);
    }

    fn record_tunnel_failure(&self, message: String) {
        self.update(|state| {
            state.tunnel_failures += 1;
            state.last_failure = Some((SystemTime::now(), message));
        });
    }
}

/// Lower case, without a trailing dot or IPv6 brackets.
fn normalize_host(host: &str) -> String {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Connect to `host:port`, resolving the name through the system resolver
/// (`getaddrinfo`) on every call. Nothing is cached here, so a laptop that
/// moves to another network uses the new DNS servers for the next connection.
async fn connect_tcp(host: &str, port: u16) -> io::Result<TcpStream> {
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await?.collect();
    let mut last_error =
        io::Error::new(io::ErrorKind::NotFound, format!("{host} has no addresses"));
    for address in addresses {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(address)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Ok(Err(error)) => last_error = error,
            Err(_) => {
                last_error = io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("connect to {address} timed out"),
                )
            }
        }
    }
    Err(last_error)
}

/// A short plain-text response for refusals and failures.
fn text_response(status: StatusCode, message: &str) -> Response<forward::Body> {
    let mut response = Response::new(
        Full::new(Bytes::from(format!("{message}\n")))
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::normalize_host;

    #[test]
    fn hosts_are_compared_in_one_form() {
        assert_eq!(normalize_host("Claude.AI."), "claude.ai");
        assert_eq!(normalize_host("[::1]"), "::1");
    }
}
