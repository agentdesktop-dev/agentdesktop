use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{Arc, Mutex, PoisonError},
    task::{Context, Poll},
    time::{Duration, SystemTime},
};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, combinators::BoxBody};
use hyper::{
    HeaderMap, Method, Request, Response, StatusCode, Uri, Version,
    body::{Body as _, Incoming},
    header::{CONNECTION, HOST, HeaderValue},
};
use hyper_util::{
    client::legacy::{
        Client,
        connect::{Connected, Connection},
    },
    rt::{TokioExecutor, TokioIo, TokioTimer},
};
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
};

use super::{Health, PendingTunnel, connect_tcp, text_response};

/// The header that carries the gateway credential. The listener removes any
/// copy the client sends and adds its own; the gateway validates it and
/// removes it before the request goes upstream.
pub const IDENTITY_HEADER: &str = "x-agentdesktop-identity";

/// How long to wait for the gateway's response headers. Streaming bodies have
/// no total timeout.
const RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(600);
/// Pooled gateway connections idle for longer than this are not reused.
const DEFAULT_IDLE_RESET: Duration = Duration::from_secs(60);

pub(super) type Body = BoxBody<Bytes, hyper::Error>;

/// The gateway that decrypted requests are forwarded to.
///
/// Every request keeps its original host (`Host` on HTTP/1.1, `:authority` on
/// HTTP/2), so one gateway listener can route claude.ai and api.anthropic.com
/// by host. The connection itself always goes to the gateway's address.
#[derive(Clone)]
pub struct GatewayTarget {
    host: String,
    port: u16,
    /// TLS to the gateway: ALPN `h2` and `http/1.1`, and `http/1.1` only for
    /// WebSocket upgrades (an upgrade cannot cross an HTTP/2 connection).
    tls: Option<(Arc<ClientConfig>, Arc<ClientConfig>)>,
}

impl std::fmt::Debug for GatewayTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatewayTarget")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("tls", &self.tls.is_some())
            .finish()
    }
}

impl GatewayTarget {
    /// A plain HTTP/1.1 gateway, for a gateway on the same host.
    pub fn http(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            tls: None,
        }
    }

    /// A gateway over TLS, verified against `roots`.
    pub fn https(host: impl Into<String>, port: u16, roots: RootCertStore) -> anyhow::Result<Self> {
        let roots = Arc::new(roots);
        let config = |alpn: &[&[u8]]| -> anyhow::Result<Arc<ClientConfig>> {
            let mut config = ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()?
            .with_root_certificates(roots.clone())
            .with_no_client_auth();
            config.alpn_protocols = alpn.iter().map(|proto| proto.to_vec()).collect();
            Ok(Arc::new(config))
        };
        Ok(Self {
            host: host.into(),
            port,
            tls: Some((config(&[b"h2", b"http/1.1"])?, config(&[b"http/1.1"])?)),
        })
    }
}

/// Where the identity header's value comes from.
#[async_trait::async_trait]
pub trait CredentialSource: Send + Sync {
    /// The current gateway credential, or `None` when the gateway has no
    /// authentication configured. An error fails the request: it is never
    /// forwarded without the identity it should carry.
    async fn credential(&self) -> Result<Option<String>, String>;
}

/// Opens gateway connections. The request URI names the original host and is
/// ignored here: every connection goes to the gateway.
#[derive(Clone)]
struct GatewayConnector {
    target: Arc<GatewayTarget>,
    /// Offer HTTP/2 when the gateway uses TLS.
    allow_h2: bool,
}

impl tower_service::Service<Uri> for GatewayConnector {
    type Response = GatewayIo;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<GatewayIo>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _uri: Uri) -> Self::Future {
        let target = self.target.clone();
        let allow_h2 = self.allow_h2;
        Box::pin(async move {
            let tcp = connect_tcp(&target.host, target.port).await?;
            let Some((with_h2, http1_only)) = &target.tls else {
                return Ok(GatewayIo {
                    io: TokioIo::new(GatewayStream::Plain(tcp)),
                    h2: false,
                });
            };
            let config = if allow_h2 { with_h2 } else { http1_only };
            let name = ServerName::try_from(target.host.clone())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
            let tls = tokio_rustls::TlsConnector::from(config.clone())
                .connect(name, tcp)
                .await?;
            let h2 = tls.get_ref().1.alpn_protocol() == Some(b"h2".as_slice());
            Ok(GatewayIo {
                io: TokioIo::new(GatewayStream::Tls(Box::new(tls))),
                h2,
            })
        })
    }
}

enum GatewayStream {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for GatewayStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for GatewayStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

/// A gateway connection, telling the client whether TLS negotiated HTTP/2.
struct GatewayIo {
    io: TokioIo<GatewayStream>,
    h2: bool,
}

impl Connection for GatewayIo {
    fn connected(&self) -> Connected {
        if self.h2 {
            Connected::new().negotiated_h2()
        } else {
            Connected::new()
        }
    }
}

impl hyper::rt::Read for GatewayIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: hyper::rt::ReadBufCursor<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_read(cx, buf)
    }
}

impl hyper::rt::Write for GatewayIo {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().io).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }
}

type GatewayClient = Client<GatewayConnector, Body>;

struct Clients {
    /// Normal requests: HTTP/2 when the gateway negotiates it.
    main: GatewayClient,
    /// WebSocket upgrades: HTTP/1.1 only.
    upgrade: GatewayClient,
    /// Wall-clock time of the last request.
    last_used: SystemTime,
}

impl Clients {
    fn new(target: &Arc<GatewayTarget>, idle_reset: Duration) -> Self {
        let build = |allow_h2| {
            // retry_canceled_requests (on by default) resends a request that
            // failed on a reused connection before any of it was written.
            Client::builder(TokioExecutor::new())
                .pool_timer(TokioTimer::new())
                .pool_idle_timeout(idle_reset)
                .retry_canceled_requests(true)
                .build(GatewayConnector {
                    target: target.clone(),
                    allow_h2,
                })
        };
        Self {
            main: build(true),
            upgrade: build(false),
            last_used: SystemTime::now(),
        }
    }
}

/// Whether the pooled gateway connections should be dropped before the next
/// request. The pool's idle timer runs on the monotonic clock, which stops
/// while the Mac sleeps, so a connection that died during sleep can look
/// fresh. The wall clock keeps running, so a gap measured on it catches both a
/// long idle period and a sleep.
pub(super) fn needs_fresh_pool(
    last_used: SystemTime,
    now: SystemTime,
    idle_reset: Duration,
) -> bool {
    match now.duration_since(last_used) {
        Ok(gap) => gap > idle_reset,
        // The clock went backwards: start clean.
        Err(_) => true,
    }
}

/// Whether a request that failed before a response may be sent once more. Only
/// requests with no body and no side effects qualify; a streamed body cannot
/// be sent twice.
pub(super) fn may_retry(method: &Method, body_is_empty: bool) -> bool {
    body_is_empty && matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// Sends decrypted requests to the gateway.
pub(super) struct Forwarder {
    target: Arc<GatewayTarget>,
    credentials: Arc<dyn CredentialSource>,
    idle_reset: Duration,
    clients: Mutex<Clients>,
}

impl Forwarder {
    pub(super) fn new(
        target: GatewayTarget,
        credentials: Arc<dyn CredentialSource>,
        idle_reset: Option<Duration>,
    ) -> Self {
        let target = Arc::new(target);
        let idle_reset = idle_reset.unwrap_or(DEFAULT_IDLE_RESET);
        Self {
            clients: Mutex::new(Clients::new(&target, idle_reset)),
            target,
            credentials,
            idle_reset,
        }
    }

    fn clients(&self) -> (GatewayClient, GatewayClient) {
        let now = SystemTime::now();
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        if needs_fresh_pool(clients.last_used, now, self.idle_reset) {
            tracing::debug!("intercept: dropping pooled gateway connections after an idle gap");
            *clients = Clients::new(&self.target, self.idle_reset);
        }
        clients.last_used = now;
        (clients.main.clone(), clients.upgrade.clone())
    }

    /// Forward one request that arrived on a connection decrypted for `host`.
    pub(super) async fn forward(
        &self,
        mut request: Request<Incoming>,
        host: &str,
        health: &Health,
        tunnel_slot: &PendingTunnel,
    ) -> Response<Body> {
        if request.method() == Method::CONNECT {
            return text_response(
                StatusCode::METHOD_NOT_ALLOWED,
                "agentdesktop intercept: CONNECT is not allowed inside an intercepted connection",
            );
        }
        // The host of every request must be the host the connection was
        // decrypted for. HTTP/2 can carry several hosts on one connection, and
        // a local process could otherwise reach another host with this
        // device's identity.
        if !request_host_matches(&request, host) {
            return text_response(
                StatusCode::MISDIRECTED_REQUEST,
                "agentdesktop intercept: the request host does not match the connection",
            );
        }
        let websocket =
            request.version() == Version::HTTP_11 && is_websocket_upgrade(request.headers());
        let inbound_upgrade = websocket.then(|| hyper::upgrade::on(&mut request));

        let credential = match self.credentials.credential().await {
            Ok(credential) => credential,
            Err(error) => {
                tracing::warn!(host, error, "intercept: no gateway credential");
                health.record_gateway_failure(format!("gateway credential: {error}"));
                return text_response(
                    StatusCode::BAD_GATEWAY,
                    "agentdesktop intercept: could not get the gateway credential",
                );
            }
        };
        let identity = match credential
            .map(|credential| HeaderValue::from_str(&format!("Bearer {credential}")))
        {
            None => None,
            Some(Ok(mut value)) => {
                value.set_sensitive(true);
                Some(value)
            }
            Some(Err(_)) => {
                health.record_gateway_failure(
                    "gateway credential is not a valid header value".to_owned(),
                );
                return text_response(
                    StatusCode::BAD_GATEWAY,
                    "agentdesktop intercept: the gateway credential is not a valid header value",
                );
            }
        };

        let (parts, body) = request.into_parts();
        let path = parts.uri.path_and_query().map_or("/", |path| path.as_str());
        let Ok(uri) = format!("https://{host}{path}").parse::<Uri>() else {
            return text_response(
                StatusCode::BAD_REQUEST,
                "agentdesktop intercept: invalid request path",
            );
        };
        let mut headers = parts.headers;
        strip_hop_headers(&mut headers);
        if websocket {
            headers.insert(CONNECTION, HeaderValue::from_static("upgrade"));
            headers.insert("upgrade", HeaderValue::from_static("websocket"));
        }
        // The client sets the host from the URI: `Host` on HTTP/1.1,
        // `:authority` on HTTP/2.
        headers.remove(HOST);
        headers.remove(IDENTITY_HEADER);
        if let Some(identity) = identity {
            headers.insert(IDENTITY_HEADER, identity);
        }
        let body_is_empty = body.is_end_stream();
        let method = parts.method;
        let build = |body: Body| {
            let mut outbound = Request::new(body);
            *outbound.method_mut() = method.clone();
            *outbound.uri_mut() = uri.clone();
            // The gateway connection decides the protocol; hyper sends an
            // HTTP/1.1 request as HTTP/2 when the connection negotiated it.
            *outbound.version_mut() = Version::HTTP_11;
            *outbound.headers_mut() = headers.clone();
            outbound
        };

        let (main, upgrade) = self.clients();
        let client = if websocket { upgrade } else { main };
        let first_body = if websocket {
            empty_body()
        } else {
            body.boxed()
        };
        let mut outcome = send(&client, build(first_body)).await;
        if outcome.is_err() && may_retry(&method, body_is_empty) && !websocket {
            tracing::debug!(
                host,
                "intercept: retrying a request that failed before any response"
            );
            outcome = send(&self.clients().0, build(empty_body())).await;
        }
        let mut response = match outcome {
            Ok(response) => response,
            Err(message) => {
                tracing::warn!(host, error = %message, "intercept: gateway request failed");
                health.record_gateway_failure(message);
                return text_response(
                    StatusCode::BAD_GATEWAY,
                    "agentdesktop intercept: gateway unreachable",
                );
            }
        };
        health.record_forwarded();

        if let Some(inbound_upgrade) = inbound_upgrade
            && response.status() == StatusCode::SWITCHING_PROTOCOLS
        {
            let upgrade_value = response.headers().get("upgrade").cloned();
            strip_hop_headers(response.headers_mut());
            response
                .headers_mut()
                .insert(CONNECTION, HeaderValue::from_static("upgrade"));
            if let Some(value) = upgrade_value {
                response.headers_mut().insert("upgrade", value);
            }
            let outbound_upgrade = hyper::upgrade::on(&mut response);
            // Unlike llm_proxy's tunnel, this one is not tied to a credential
            // lifetime: Remote Control and Code-tab sockets stay open for a
            // long time.
            *tunnel_slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::pin(
                async move {
                    match tokio::try_join!(inbound_upgrade, outbound_upgrade) {
                        Ok((inbound, outbound)) => {
                            let mut inbound = TokioIo::new(inbound);
                            let mut outbound = TokioIo::new(outbound);
                            if let Err(error) =
                                tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await
                            {
                                tracing::debug!(%error, "intercept: WebSocket closed with an error");
                            }
                        }
                        Err(error) => {
                            tracing::debug!(%error, "intercept: WebSocket upgrade did not complete")
                        }
                    }
                },
            ));
            return response.map(|_| empty_body());
        }
        strip_hop_headers(response.headers_mut());
        response.map(BodyExt::boxed)
    }
}

async fn send(
    client: &GatewayClient,
    request: Request<Body>,
) -> Result<Response<Incoming>, String> {
    match tokio::time::timeout(RESPONSE_HEADERS_TIMEOUT, client.request(request)).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(error)) => Err(error_chain(&error)),
        Err(_) => Err("the gateway did not answer in time".to_owned()),
    }
}

/// The error and its causes on one line, such as "client error (Connect):
/// Connection refused (os error 61)". The client error alone does not say why.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        message.push_str(": ");
        message.push_str(&cause.to_string());
        source = cause.source();
    }
    message
}

pub(super) fn empty_body() -> Body {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

/// Whether the request's host (the URI authority on HTTP/2, `Host` on
/// HTTP/1.1) is `host`, on the default HTTPS port.
fn request_host_matches(request: &Request<Incoming>, host: &str) -> bool {
    let from_uri = request
        .uri()
        .authority()
        .map(|authority| authority.as_str());
    let from_header = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok());
    let names_host = |authority: &str| {
        let (name, port) = match authority.rsplit_once(':') {
            Some((name, port)) if !name.contains(':') => (name, Some(port)),
            _ => (authority, None),
        };
        super::normalize_host(name) == host && port.is_none_or(|port| port == "443")
    };
    match (from_uri, from_header) {
        (None, None) => false,
        (Some(authority), None) | (None, Some(authority)) => names_host(authority),
        (Some(uri), Some(header)) => names_host(uri) && names_host(header),
    }
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    let lists = |name: &str, token: &str| {
        headers
            .get_all(name)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|item| item.trim().eq_ignore_ascii_case(token))
    };
    lists(CONNECTION.as_str(), "upgrade") && lists("upgrade", "websocket")
}

fn strip_hop_headers(headers: &mut HeaderMap) {
    let nominated: Vec<_> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_owned())
        .collect();
    for name in nominated {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pool_is_dropped_after_a_wall_clock_gap_or_a_clock_jump_back() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let idle = Duration::from_secs(60);
        assert!(!needs_fresh_pool(
            base,
            base + Duration::from_secs(59),
            idle
        ));
        assert!(needs_fresh_pool(base, base + Duration::from_secs(61), idle));
        // After a night of sleep the wall clock has moved on even though the
        // monotonic clock has not.
        assert!(needs_fresh_pool(
            base,
            base + Duration::from_secs(8 * 60 * 60),
            idle
        ));
        assert!(needs_fresh_pool(base, base - Duration::from_secs(1), idle));
    }

    #[test]
    fn only_bodiless_safe_requests_are_retried() {
        assert!(may_retry(&Method::GET, true));
        assert!(may_retry(&Method::HEAD, true));
        assert!(!may_retry(&Method::GET, false));
        assert!(!may_retry(&Method::POST, true));
        assert!(!may_retry(&Method::PUT, true));
    }
}
