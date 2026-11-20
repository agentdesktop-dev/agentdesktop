//! HTTP listeners with connection limits: the management listener and the
//! gateway listener.
//!
//! Both get the fleet listener's connection admission
//! (`fleet_limits::Connections`): total and per-address caps that close the
//! longest-idle connection to make room, and first-request and idle deadlines.
//! No request marks its connection busy, so the deadlines bound every
//! connection, and any connection can be closed to make room at a cap. Each
//! listener's caps come from its `limits` configuration; the timeouts below are
//! the same for both.
//!
//! They are served with axum and, for HTTPS, the rustls configuration from
//! `agentdesktop_core::tls`, so clients may use HTTP/1.1 or HTTP/2.

use std::{io, net::SocketAddr, path::Path, sync::Arc, time::Duration};

use anyhow::Context as _;
use axum::{
    Router,
    extract::{ConnectInfo, Request, State},
    middleware::{self, Next},
    response::Response,
};
use axum_server::{
    accept::Accept,
    tls_rustls::{RustlsAcceptor, RustlsConfig},
};
use tokio::net::TcpStream;

use crate::{
    fleet_limits::{ConnectionLimits, Connections, LimitedConnection},
    metrics::LimitMetrics,
};

/// Time a new connection has to make its first request.
const FIRST_REQUEST: Duration = Duration::from_secs(20);
/// Time a connection may stay open without a request after its last one.
const IDLE: Duration = Duration::from_secs(120);
/// Time a client has to finish the TLS handshake after connecting.
const TLS_HANDSHAKE: Duration = Duration::from_secs(10);
/// HTTP/2 streams one connection may have open at once.
const CONCURRENT_STREAMS_PER_CONNECTION: u32 = 64;

/// Limits for one HTTP listener.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HttpListenerLimits {
    pub connections: ConnectionLimits,
    pub tls_handshake: Duration,
    pub concurrent_streams_per_connection: u32,
}

impl HttpListenerLimits {
    /// The configured caps, with the shared timeouts.
    pub const fn new(max_connections: usize, connections_per_address: usize) -> Self {
        Self {
            connections: ConnectionLimits {
                max_connections,
                connections_per_address,
                first_request: FIRST_REQUEST,
                idle: IDLE,
            },
            tls_handshake: TLS_HANDSHAKE,
            concurrent_streams_per_connection: CONCURRENT_STREAMS_PER_CONNECTION,
        }
    }
}

/// PEM certificate and key for an HTTPS listener.
pub struct HttpListenerTls<'a> {
    pub certificate: &'a Path,
    pub key: &'a Path,
}

/// A bound HTTP listener.
pub struct HttpListener {
    name: &'static str,
    listener: std::net::TcpListener,
    tls: Option<RustlsConfig>,
    limits: HttpListenerLimits,
    connections: Connections,
}

impl HttpListener {
    /// Binds `address`. `name` labels logs, errors and `metrics`, e.g.
    /// `gateway`.
    pub fn bind(
        name: &'static str,
        address: SocketAddr,
        tls: Option<HttpListenerTls<'_>>,
        limits: HttpListenerLimits,
        metrics: &LimitMetrics,
    ) -> anyhow::Result<Self> {
        let listener = std::net::TcpListener::bind(address)
            .with_context(|| format!("bind {name} listener on {address}"))?;
        Self::from_std(name, listener, tls, limits, metrics)
    }

    /// Serves on an already bound `listener`.
    pub fn from_std(
        name: &'static str,
        listener: std::net::TcpListener,
        tls: Option<HttpListenerTls<'_>>,
        limits: HttpListenerLimits,
        metrics: &LimitMetrics,
    ) -> anyhow::Result<Self> {
        let tls = tls
            .map(|tls| {
                agentdesktop_core::tls::server_config(tls.certificate, tls.key, name)
                    .map(|config| RustlsConfig::from_config(Arc::new(config)))
            })
            .transpose()?;
        Ok(Self {
            name,
            listener,
            tls,
            limits,
            connections: Connections::new(limits.connections, name, metrics),
        })
    }

    /// The address the listener is bound to.
    pub fn local_address(&self) -> anyhow::Result<SocketAddr> {
        self.listener
            .local_addr()
            .with_context(|| format!("read {} listener address", self.name))
    }

    /// The listener's connection registry.
    #[cfg(test)]
    pub(crate) fn connections(&self) -> &Connections {
        &self.connections
    }

    /// Serves `router` until the listener fails.
    pub async fn serve(self, router: Router) -> anyhow::Result<()> {
        let name = self.name;
        tracing::info!(
            listen = %self.local_address()?,
            tls = self.tls.is_some(),
            "{name} listener listening"
        );
        let connections = self.connections;
        let app = router
            .layer(middleware::from_fn_with_state(
                connections.clone(),
                track_request,
            ))
            .into_make_service_with_connect_info::<SocketAddr>();
        let admit = Admit { connections };
        let mut server = axum_server::from_tcp(self.listener)
            .with_context(|| format!("start {name} listener"))?;
        server
            .http_builder()
            .http2()
            .max_concurrent_streams(self.limits.concurrent_streams_per_connection);
        match self.tls {
            Some(tls) => {
                let acceptor = RustlsAcceptor::new(tls)
                    .handshake_timeout(self.limits.tls_handshake)
                    .acceptor(admit);
                server.acceptor(acceptor).serve(app).await
            }
            None => server.acceptor(admit).serve(app).await,
        }
        .with_context(|| format!("serve {name} listener"))
    }
}

/// Admits accepted connections into the registry, closing them over the caps.
/// TLS, when configured, runs on the admitted connection, so the first-request
/// deadline also bounds the handshake.
#[derive(Clone)]
struct Admit {
    connections: Connections,
}

impl<S> Accept<TcpStream, S> for Admit {
    type Stream = LimitedConnection;
    type Service = S;
    type Future = std::future::Ready<io::Result<(LimitedConnection, S)>>;

    fn accept(&self, stream: TcpStream, service: S) -> Self::Future {
        std::future::ready(
            self.connections
                .admit(stream)
                .map(|connection| (connection, service))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::ConnectionRefused,
                        "listener connection limit",
                    )
                }),
        )
    }
}

/// Records each request as activity on its connection.
async fn track_request(
    State(connections): State<Connections>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
    next: Next,
) -> Response {
    let _activity = connections.start_request(peer, false);
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use axum::{Router, routing::get};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    use super::{HttpListener, HttpListenerLimits, HttpListenerTls};
    use crate::{
        fleet_limits::Connections,
        metrics::{Limit, LimitMetrics, Outcome, Telemetry},
    };

    const LIMITS: HttpListenerLimits = HttpListenerLimits::new(1024, 64);

    /// Fails the test, instead of hanging it, when `future` takes over 5 s.
    async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("timed out")
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    /// Serves a one-route app on a loopback listener.
    fn serve(
        limits: HttpListenerLimits,
        tls: Option<HttpListenerTls<'_>>,
    ) -> (SocketAddr, Connections) {
        serve_measured(limits, tls, &LimitMetrics::disabled())
    }

    /// Like `serve`, counting the limits that act in `metrics` as the `test`
    /// listener.
    fn serve_measured(
        limits: HttpListenerLimits,
        tls: Option<HttpListenerTls<'_>>,
        metrics: &LimitMetrics,
    ) -> (SocketAddr, Connections) {
        let listener = HttpListener::bind(
            "test",
            SocketAddr::from(([127, 0, 0, 1], 0)),
            tls,
            limits,
            metrics,
        )
        .unwrap();
        let address = listener.local_address().unwrap();
        let connections = listener.connections().clone();
        tokio::spawn(listener.serve(Router::new().route("/", get(|| async { "ok" }))));
        (address, connections)
    }

    /// Waits until the listener has admitted `count` connections.
    async fn until_open(connections: &Connections, count: usize) {
        bounded(async {
            while connections.open_count() < count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
    }

    /// Waits until the `test` listener's connection gauge reads `count`.
    async fn until_gauge(telemetry: &Telemetry, count: i64) {
        bounded(async {
            while telemetry.connections("test") != count {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await;
    }

    /// Waits until the server closes `stream`.
    async fn until_closed(mut stream: TcpStream) {
        let mut buffer = [0; 1024];
        bounded(async { while matches!(stream.read(&mut buffer).await, Ok(read) if read > 0) {} })
            .await;
    }

    #[tokio::test]
    async fn https_serves_http1_and_http2_clients() {
        let directory = std::env::temp_dir().join(format!(
            "agentdesktop-http-listener-tls-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir(&directory).unwrap();
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
        let certificate = directory.join("controller.pem");
        let key = directory.join("controller-key.pem");
        std::fs::write(&certificate, cert.pem()).unwrap();
        std::fs::write(&key, signing_key.serialize_pem()).unwrap();
        let (address, _) = serve(
            LIMITS,
            Some(HttpListenerTls {
                certificate: &certificate,
                key: &key,
            }),
        );

        let tls_client = |builder: reqwest::ClientBuilder| {
            builder
                .add_root_certificate(
                    reqwest::Certificate::from_pem(cert.pem().as_bytes()).unwrap(),
                )
                .timeout(Duration::from_secs(5))
                .build()
                .unwrap()
        };
        for (client, version) in [
            (
                tls_client(reqwest::Client::builder()),
                reqwest::Version::HTTP_2,
            ),
            (
                tls_client(reqwest::Client::builder().http1_only()),
                reqwest::Version::HTTP_11,
            ),
        ] {
            let response = client
                .get(format!("https://{address}/"))
                .send()
                .await
                .unwrap();
            assert_eq!(response.version(), version);
            assert_eq!(response.text().await.unwrap(), "ok");
        }
        assert!(
            client()
                .get(format!("http://{address}/"))
                .send()
                .await
                .map_or(true, |response| !response.status().is_success()),
            "plaintext must not reach the TLS listener"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn a_connection_that_never_makes_a_request_is_closed() {
        let mut limits = LIMITS;
        limits.connections.first_request = Duration::from_millis(200);
        let telemetry = Telemetry::new().unwrap();
        let (address, _) = serve_measured(limits, None, &telemetry.limits());

        until_closed(bounded(TcpStream::connect(address)).await.unwrap()).await;

        assert_eq!(
            telemetry.hits("test", Limit::FirstRequest, Outcome::TimedOut),
            1
        );
        assert_eq!(
            telemetry.hits("test", Limit::IdleConnection, Outcome::TimedOut),
            0
        );
    }

    #[tokio::test]
    async fn a_keep_alive_connection_is_closed_once_idle() {
        let mut limits = LIMITS;
        // Far past the test's bound, so only the idle close can end the connection.
        limits.connections.first_request = Duration::from_secs(60);
        limits.connections.idle = Duration::from_millis(200);
        let telemetry = Telemetry::new().unwrap();
        let (address, _) = serve_measured(limits, None, &telemetry.limits());
        let mut stream = bounded(TcpStream::connect(address)).await.unwrap();

        bounded(stream.write_all(b"GET / HTTP/1.1\r\nHost: listener\r\n\r\n"))
            .await
            .unwrap();
        let mut status = [0; 12];
        bounded(stream.read_exact(&mut status)).await.unwrap();
        assert_eq!(&status, b"HTTP/1.1 200");

        until_closed(stream).await;

        assert_eq!(
            telemetry.hits("test", Limit::IdleConnection, Outcome::TimedOut),
            1
        );
        assert_eq!(
            telemetry.hits("test", Limit::FirstRequest, Outcome::TimedOut),
            0
        );
    }

    #[tokio::test]
    async fn the_connection_gauge_follows_the_open_connections() {
        let mut limits = LIMITS;
        limits.connections.first_request = Duration::from_secs(60);
        let telemetry = Telemetry::new().unwrap();
        let (address, connections) = serve_measured(limits, None, &telemetry.limits());
        assert_eq!(telemetry.connections("test"), 0);

        let first = bounded(TcpStream::connect(address)).await.unwrap();
        let second = bounded(TcpStream::connect(address)).await.unwrap();
        until_open(&connections, 2).await;
        until_gauge(&telemetry, 2).await;

        drop(first);
        until_gauge(&telemetry, 1).await;
        assert_eq!(connections.open_count(), 1);

        drop(second);
        until_gauge(&telemetry, 0).await;
    }

    #[tokio::test]
    async fn an_evicted_connection_leaves_the_gauge_at_the_open_count() {
        let mut limits = LIMITS;
        limits.connections.first_request = Duration::from_secs(60);
        limits.connections.max_connections = 2;
        let telemetry = Telemetry::new().unwrap();
        let (address, connections) = serve_measured(limits, None, &telemetry.limits());
        let oldest = bounded(TcpStream::connect(address)).await.unwrap();
        until_open(&connections, 1).await;
        let _second = bounded(TcpStream::connect(address)).await.unwrap();
        until_open(&connections, 2).await;

        let _third = bounded(TcpStream::connect(address)).await.unwrap();
        until_closed(oldest).await;

        // The eviction counted, and the evicted connection's close took it
        // out of the gauge as the new one's open added it.
        assert_eq!(
            telemetry.hits("test", Limit::MaxConnections, Outcome::Evicted),
            1
        );
        until_gauge(&telemetry, 2).await;
        assert_eq!(connections.open_count(), 2);
    }

    #[tokio::test]
    async fn at_the_address_cap_the_longest_idle_connection_makes_room() {
        let mut limits = LIMITS;
        limits.connections.first_request = Duration::from_secs(60);
        limits.connections.connections_per_address = 2;
        let (address, connections) = serve(limits, None);
        // Each connection is admitted in its own task, so wait for it to register
        // before the next one connects: `oldest` must be the longest idle.
        let oldest = bounded(TcpStream::connect(address)).await.unwrap();
        until_open(&connections, 1).await;
        let _second = bounded(TcpStream::connect(address)).await.unwrap();
        until_open(&connections, 2).await;

        let response = client()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        until_closed(oldest).await;
    }

    #[tokio::test]
    async fn at_the_address_cap_a_connection_that_just_made_a_request_is_kept() {
        let mut limits = LIMITS;
        limits.connections.first_request = Duration::from_secs(60);
        limits.connections.connections_per_address = 2;
        let (address, connections) = serve(limits, None);
        let mut older = bounded(TcpStream::connect(address)).await.unwrap();
        until_open(&connections, 1).await;
        let newer = bounded(TcpStream::connect(address)).await.unwrap();
        until_open(&connections, 2).await;
        // The older connection makes a request after the newer one connected,
        // so the newer one has gone longer without a request.
        bounded(older.write_all(b"GET / HTTP/1.1\r\nHost: listener\r\n\r\n"))
            .await
            .unwrap();
        let mut status = [0; 12];
        bounded(older.read_exact(&mut status)).await.unwrap();
        assert_eq!(&status, b"HTTP/1.1 200");

        let response = client()
            .get(format!("http://{address}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.text().await.unwrap(), "ok");
        until_closed(newer).await;
    }
}
