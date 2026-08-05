use std::collections::HashSet;
use std::fmt;
use std::future::{Future, IntoFuture};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONNECTION, CONTENT_TYPE, HOST};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Request, Response, StatusCode};
use axum::routing::{any, get};
use axum::{Json, Router};
use futures_util::StreamExt;
use reqwest::{Client, Url};
use serde::Serialize;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tracing::Instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::config::DeploymentMode;
use crate::identity::oauth::ManagedIdentity;
use crate::platform::{PlatformCapabilities, capabilities};
use crate::telemetry::ensure_trace_context;

const GATEWAY_ERROR: &str = "agent gateway unavailable\n";
const IDENTITY_ERROR: &str = "managed identity unavailable\n";
const OVERLOAD_ERROR: &str = "connector overloaded\n";
const TIMEOUT_ERROR: &str = "agent gateway timed out\n";

#[derive(Clone, Copy, Debug)]
pub struct ProxyOptions {
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
    pub shutdown_timeout: Duration,
    pub max_in_flight: usize,
}

impl Default for ProxyOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            request_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(10),
            max_in_flight: 128,
        }
    }
}

#[derive(Clone)]
struct ProxyState {
    clients: Arc<StdMutex<ClientPool>>,
    device_identity: Option<ManagedDeviceIdentity>,
    upstream: Url,
    mode: DeploymentMode,
    identity: Option<ManagedIdentity>,
    request_timeout: Duration,
    connect_timeout: Duration,
    shutdown_timeout: Duration,
    max_in_flight: usize,
    in_flight: Arc<Semaphore>,
    metrics: Arc<OperationalMetrics>,
}

#[derive(Default)]
struct OperationalMetrics {
    requests: AtomicU64,
    upstream_responses: AtomicU64,
    identity_failures: AtomicU64,
    overload_rejections: AtomicU64,
    upstream_timeouts: AtomicU64,
    upstream_failures: AtomicU64,
}

#[derive(Serialize)]
struct MetricsSnapshot {
    requests: u64,
    upstream_responses: u64,
    identity_failures: u64,
    overload_rejections: u64,
    upstream_timeouts: u64,
    upstream_failures: u64,
}

impl OperationalMetrics {
    fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            upstream_responses: self.upstream_responses.load(Ordering::Relaxed),
            identity_failures: self.identity_failures.load(Ordering::Relaxed),
            overload_rejections: self.overload_rejections.load(Ordering::Relaxed),
            upstream_timeouts: self.upstream_timeouts.load(Ordering::Relaxed),
            upstream_failures: self.upstream_failures.load(Ordering::Relaxed),
        }
    }
}

struct ClientPool {
    // A reqwest client owns its connection pool, so credential changes require a new client.
    credential_generation: u64,
    device_generation: u64,
    client: Client,
}

#[derive(Clone)]
pub struct ManagedDeviceIdentity {
    state: Arc<StdMutex<DeviceIdentityState>>,
}

struct DeviceIdentityState {
    generation: u64,
    identity: reqwest::Identity,
}

impl ManagedDeviceIdentity {
    pub fn new(identity: reqwest::Identity) -> Self {
        Self {
            state: Arc::new(StdMutex::new(DeviceIdentityState {
                generation: 1,
                identity,
            })),
        }
    }

    pub fn replace(&self, identity: reqwest::Identity) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("managed device identity lock poisoned"))?;
        state.identity = identity;
        state.generation = state.generation.wrapping_add(1);
        Ok(())
    }

    fn snapshot(&self) -> Result<(u64, reqwest::Identity)> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("managed device identity lock poisoned"))?;
        Ok((state.generation, state.identity.clone()))
    }
}

#[derive(Debug)]
struct IdentityError(anyhow::Error);

#[derive(Debug)]
struct OverloadError;

#[derive(Debug)]
struct UpstreamTimeout;

impl fmt::Display for IdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for IdentityError {}

impl fmt::Display for OverloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("maximum in-flight request limit reached")
    }
}

impl std::error::Error for OverloadError {}

impl fmt::Display for UpstreamTimeout {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("upstream response header timeout")
    }
}

impl std::error::Error for UpstreamTimeout {}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    mode: &'static str,
    gateway: &'static str,
}

#[derive(Serialize)]
struct StatusResponse {
    version: &'static str,
    mode: &'static str,
    gateway: &'static str,
    identity: &'static str,
    in_flight: usize,
    max_in_flight: usize,
    connect_timeout_ms: u128,
    request_timeout_ms: u128,
    shutdown_timeout_ms: u128,
    platform: PlatformCapabilities,
    metrics: MetricsSnapshot,
}

pub async fn serve(
    listener: TcpListener,
    upstream: Url,
    mode: DeploymentMode,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    serve_with_identity(
        listener,
        upstream,
        mode,
        None,
        ProxyOptions::default(),
        shutdown,
    )
    .await
}

pub async fn serve_with_identity(
    listener: TcpListener,
    upstream: Url,
    mode: DeploymentMode,
    identity: Option<ManagedIdentity>,
    options: ProxyOptions,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    serve_with_managed_identity(listener, upstream, mode, identity, None, options, shutdown).await
}

pub async fn serve_with_managed_identity(
    listener: TcpListener,
    upstream: Url,
    mode: DeploymentMode,
    identity: Option<ManagedIdentity>,
    device_identity: Option<reqwest::Identity>,
    options: ProxyOptions,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let device_identity = device_identity.map(ManagedDeviceIdentity::new);
    serve_with_rotating_managed_identity(
        listener,
        upstream,
        mode,
        identity,
        device_identity,
        options,
        shutdown,
    )
    .await
}

pub async fn serve_with_rotating_managed_identity(
    listener: TcpListener,
    upstream: Url,
    mode: DeploymentMode,
    identity: Option<ManagedIdentity>,
    device_identity: Option<ManagedDeviceIdentity>,
    options: ProxyOptions,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let (device_generation, initial_identity) = match &device_identity {
        Some(identity) => {
            let (generation, identity) = identity.snapshot()?;
            (generation, Some(identity))
        }
        None => (0, None),
    };
    let client = build_client(options.connect_timeout, initial_identity)?;
    let state = ProxyState {
        clients: Arc::new(StdMutex::new(ClientPool {
            credential_generation: 0,
            device_generation,
            client,
        })),
        device_identity,
        upstream,
        mode,
        identity,
        request_timeout: options.request_timeout,
        connect_timeout: options.connect_timeout,
        shutdown_timeout: options.shutdown_timeout,
        max_in_flight: options.max_in_flight,
        in_flight: Arc::new(Semaphore::new(options.max_in_flight)),
        metrics: Arc::new(OperationalMetrics::default()),
    };
    let app = Router::new()
        .route("/_agentdesktop/healthz", get(health))
        .route("/_agentdesktop/status", get(status))
        .fallback(any(forward))
        .with_state(state);

    // One signal drives graceful shutdown and the independent hard deadline.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        shutdown.await;
        let _ = shutdown_tx.send(true);
    });
    let mut server_shutdown = shutdown_rx.clone();
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = server_shutdown.wait_for(|shutdown| *shutdown).await;
        })
        .into_future();
    tokio::pin!(server);
    let mut timeout_shutdown = shutdown_rx;
    tokio::select! {
        result = &mut server => result?,
        _ = async {
            let _ = timeout_shutdown.wait_for(|shutdown| *shutdown).await;
            tokio::time::sleep(options.shutdown_timeout).await;
        } => anyhow::bail!("graceful shutdown exceeded configured timeout"),
    }
    Ok(())
}

async fn status(State(state): State<ProxyState>) -> Json<StatusResponse> {
    let gateway = if gateway_reachable(&state.upstream).await {
        "reachable"
    } else {
        "unreachable"
    };
    let identity = match &state.identity {
        Some(identity) => identity.status().await.unwrap_or("unavailable"),
        None if state.mode == DeploymentMode::Managed => "not-configured",
        None => "not-required",
    };
    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION"),
        mode: state.mode.as_str(),
        gateway,
        identity,
        in_flight: state
            .max_in_flight
            .saturating_sub(state.in_flight.available_permits()),
        max_in_flight: state.max_in_flight,
        connect_timeout_ms: state.connect_timeout.as_millis(),
        request_timeout_ms: state.request_timeout.as_millis(),
        shutdown_timeout_ms: state.shutdown_timeout.as_millis(),
        platform: capabilities(),
        metrics: state.metrics.snapshot(),
    })
}

async fn health(State(state): State<ProxyState>) -> (StatusCode, Json<HealthResponse>) {
    let reachable = gateway_reachable(&state.upstream).await;
    let status = if reachable {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        status,
        Json(HealthResponse {
            status: if reachable { "ok" } else { "degraded" },
            mode: state.mode.as_str(),
            gateway: if reachable {
                "reachable"
            } else {
                "unreachable"
            },
        }),
    )
}

async fn gateway_reachable(upstream: &Url) -> bool {
    let Some(host) = upstream.host_str() else {
        return false;
    };
    let Some(port) = upstream.port_or_known_default() else {
        return false;
    };
    TcpStream::connect((host, port)).await.is_ok()
}

async fn forward(State(state): State<ProxyState>, mut request: Request<Body>) -> Response<Body> {
    state.metrics.requests.fetch_add(1, Ordering::Relaxed);
    let trace_context = ensure_trace_context(request.headers_mut());
    let span = tracing::info_span!(
        "forward",
        mode = state.mode.as_str(),
        http.response.status_code = tracing::field::Empty
    );
    let _ = span.set_parent(trace_context.parent());
    let mut response = async {
        // Keep transport errors private and expose only stable application-facing failures.
        match forward_request(&state, request).await {
            Ok(response) => {
                state
                    .metrics
                    .upstream_responses
                    .fetch_add(1, Ordering::Relaxed);
                response
            }
            Err(error) => {
                tracing::warn!(event = "forward_failed", reason = failure_reason(&error));
                if error.downcast_ref::<IdentityError>().is_some() {
                    state
                        .metrics
                        .identity_failures
                        .fetch_add(1, Ordering::Relaxed);
                    Response::builder()
                        .status(StatusCode::UNAUTHORIZED)
                        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
                        .header("x-agentdesktop-error", "identity-unavailable")
                        .body(Body::from(IDENTITY_ERROR))
                        .expect("static error response must be valid")
                } else if error.downcast_ref::<OverloadError>().is_some() {
                    state
                        .metrics
                        .overload_rejections
                        .fetch_add(1, Ordering::Relaxed);
                    Response::builder()
                        .status(StatusCode::SERVICE_UNAVAILABLE)
                        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
                        .header("x-agentdesktop-error", "overloaded")
                        .body(Body::from(OVERLOAD_ERROR))
                        .expect("static error response must be valid")
                } else if error.downcast_ref::<UpstreamTimeout>().is_some() {
                    state
                        .metrics
                        .upstream_timeouts
                        .fetch_add(1, Ordering::Relaxed);
                    Response::builder()
                        .status(StatusCode::GATEWAY_TIMEOUT)
                        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
                        .header("x-agentdesktop-error", "upstream-timeout")
                        .body(Body::from(TIMEOUT_ERROR))
                        .expect("static error response must be valid")
                } else {
                    state
                        .metrics
                        .upstream_failures
                        .fetch_add(1, Ordering::Relaxed);
                    Response::builder()
                        .status(StatusCode::BAD_GATEWAY)
                        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
                        .header("x-agentdesktop-error", "upstream-unavailable")
                        .body(Body::from(GATEWAY_ERROR))
                        .expect("static error response must be valid")
                }
            }
        }
    }
    .instrument(span.clone())
    .await;
    span.record("http.response.status_code", response.status().as_u16());
    trace_context.apply_to_response(response.headers_mut());
    response
}

fn failure_reason(error: &anyhow::Error) -> &'static str {
    if error.downcast_ref::<IdentityError>().is_some() {
        "identity_unavailable"
    } else if error.downcast_ref::<OverloadError>().is_some() {
        "overloaded"
    } else if error.downcast_ref::<UpstreamTimeout>().is_some() {
        "upstream_timeout"
    } else {
        "upstream_unavailable"
    }
}

async fn forward_request(state: &ProxyState, request: Request<Body>) -> Result<Response<Body>> {
    let permit = state
        .in_flight
        .clone()
        .try_acquire_owned()
        .map_err(|_| anyhow::Error::new(OverloadError))?;
    let (parts, body) = request.into_parts();
    let url = upstream_url(&state.upstream, &parts.uri)?;
    let mut headers = parts.headers;
    remove_hop_by_hop_headers(&mut headers);
    headers.remove(HOST);
    if state.mode == DeploymentMode::Managed {
        headers.remove("dpop");
    }
    let mut credential_generation = 0;
    if let Some(identity) = &state.identity {
        let credentials = identity.credentials().await.map_err(identity_error)?;
        headers.insert(
            "x-agentdesktop-authorization",
            HeaderValue::from_str(&format!("Bearer {}", credentials.access_token))
                .map_err(|error| identity_error(error.into()))?,
        );
        credential_generation = credentials.generation;
    }

    let client = client_for_generation(state, credential_generation)?;
    let send = client
        .request(parts.method, url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send();
    let upstream_response = tokio::time::timeout(state.request_timeout, send)
        .await
        .map_err(|_| anyhow::Error::new(UpstreamTimeout))??;

    let status = upstream_response.status();
    let mut headers = upstream_response.headers().clone();
    remove_hop_by_hop_headers(&mut headers);
    // Hold the concurrency permit until the downstream client finishes consuming the stream.
    let body = Body::from_stream(upstream_response.bytes_stream().map(move |chunk| {
        let _permit = &permit;
        chunk
    }));

    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response)
}

fn client_for_generation(state: &ProxyState, generation: u64) -> Result<Client> {
    let (device_generation, device_identity) = match &state.device_identity {
        Some(identity) => {
            let (generation, identity) = identity.snapshot()?;
            (generation, Some(identity))
        }
        None => (0, None),
    };
    let mut pool = state
        .clients
        .lock()
        .map_err(|_| anyhow::anyhow!("upstream client pool lock poisoned"))?;
    // New bearer or device credentials must never reuse connections authenticated earlier.
    if pool.credential_generation != generation || pool.device_generation != device_generation {
        pool.client = build_client(state.connect_timeout, device_identity)?;
        pool.credential_generation = generation;
        pool.device_generation = device_generation;
        tracing::info!(event = "upstream_pool_rotated");
    }
    Ok(pool.client.clone())
}

fn build_client(
    connect_timeout: Duration,
    device_identity: Option<reqwest::Identity>,
) -> Result<Client> {
    let mut builder = Client::builder().connect_timeout(connect_timeout);
    if let Some(identity) = device_identity {
        builder = builder.identity(identity);
    }
    Ok(builder.build()?)
}

fn identity_error(error: anyhow::Error) -> anyhow::Error {
    IdentityError(error).into()
}

fn upstream_url(upstream: &Url, uri: &axum::http::Uri) -> Result<Url> {
    let mut target = upstream.as_str().trim_end_matches('/').to_owned();
    target.push_str(uri.path());
    if let Some(query) = uri.query() {
        target.push('?');
        target.push_str(query);
    }
    Ok(Url::parse(&target)?)
}

fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
    // Connection may nominate additional per-hop headers beyond the standard fixed set.
    let connection_headers: HashSet<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();

    for name in connection_headers {
        headers.remove(name);
    }
    // Connector-only authorization is consumed locally and must not cross either boundary.
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "x-agentdesktop-authorization",
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
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, Uri};
    use axum::routing::any;
    use futures_util::StreamExt;
    use http_body_util::BodyExt;
    use rcgen::{CertificateParams, KeyPair, PKCS_ECDSA_P256_SHA256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::{Mutex, mpsc, oneshot};
    use tokio_stream::wrappers::ReceiverStream;

    use super::*;
    use crate::identity::oauth::StoredSession;
    use crate::identity::storage::{CredentialStorageMode, CredentialStore};

    #[derive(Clone, Debug)]
    struct CapturedRequest {
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    }

    async fn start_proxy(upstream: Url) -> (std::net::SocketAddr, oneshot::Sender<()>) {
        start_proxy_with_options(upstream, ProxyOptions::default()).await
    }

    async fn start_proxy_with_options(
        upstream: Url,
        options: ProxyOptions,
    ) -> (std::net::SocketAddr, oneshot::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            serve_with_identity(
                listener,
                upstream,
                DeploymentMode::Standalone,
                None,
                options,
                async {
                    let _ = shutdown_rx.await;
                },
            )
            .await
            .unwrap();
        });
        (address, shutdown_tx)
    }

    #[tokio::test]
    async fn rejects_overload_while_response_is_streaming() {
        let (release_tx, release_rx) = oneshot::channel();
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let fake_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = fake_listener.local_addr().unwrap();
        let fake_app = Router::new()
            .fallback(any(
                move |State(release_rx): State<Arc<Mutex<Option<oneshot::Receiver<()>>>>>| async move {
                    let (body_tx, body_rx) = mpsc::channel::<Result<Bytes, Infallible>>(1);
                    let release_rx = release_rx.lock().await.take();
                    tokio::spawn(async move {
                        body_tx.send(Ok(Bytes::from_static(b"started"))).await.unwrap();
                        if let Some(release_rx) = release_rx {
                            let _ = release_rx.await;
                        }
                    });
                    Body::from_stream(ReceiverStream::new(body_rx))
                },
            ))
            .with_state(release_rx);
        tokio::spawn(async move {
            axum::serve(fake_listener, fake_app).await.unwrap();
        });
        let options = ProxyOptions {
            max_in_flight: 1,
            ..ProxyOptions::default()
        };
        let (proxy_address, shutdown) = start_proxy_with_options(
            Url::parse(&format!("http://{fake_address}")).unwrap(),
            options,
        )
        .await;
        let client = Client::new();
        let first = client
            .get(format!("http://{proxy_address}/first"))
            .send()
            .await
            .unwrap();

        let second = client
            .get(format!("http://{proxy_address}/second"))
            .send()
            .await
            .unwrap();

        assert_eq!(second.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(second.headers()["x-agentdesktop-error"], "overloaded");
        release_tx.send(()).unwrap();
        drop(first);
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn times_out_waiting_for_upstream_headers() {
        let fake_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = fake_listener.local_addr().unwrap();
        let fake_app =
            Router::new().fallback(any(|| async { std::future::pending::<StatusCode>().await }));
        tokio::spawn(async move {
            axum::serve(fake_listener, fake_app).await.unwrap();
        });
        let options = ProxyOptions {
            request_timeout: Duration::from_millis(20),
            ..ProxyOptions::default()
        };
        let (proxy_address, shutdown) = start_proxy_with_options(
            Url::parse(&format!("http://{fake_address}")).unwrap(),
            options,
        )
        .await;

        let response = Client::new()
            .get(format!("http://{proxy_address}/slow"))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            response.headers()["x-agentdesktop-error"],
            "upstream-timeout"
        );
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn times_out_stalled_streaming_request_body() {
        let fake_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = fake_listener.local_addr().unwrap();
        let fake_app = Router::new().fallback(any(|request: Request<Body>| async move {
            let mut body = request.into_body().into_data_stream();
            let _ = body.next().await;
            std::future::pending::<StatusCode>().await
        }));
        tokio::spawn(async move {
            axum::serve(fake_listener, fake_app).await.unwrap();
        });
        let options = ProxyOptions {
            request_timeout: Duration::from_millis(20),
            ..ProxyOptions::default()
        };
        let (proxy_address, shutdown) = start_proxy_with_options(
            Url::parse(&format!("http://{fake_address}")).unwrap(),
            options,
        )
        .await;
        let (body_tx, body_rx) = mpsc::channel::<Result<Bytes, Infallible>>(1);
        body_tx
            .send(Ok(Bytes::from_static(b"partial")))
            .await
            .unwrap();

        let response = Client::new()
            .post(format!("http://{proxy_address}/slow-upload"))
            .body(reqwest::Body::wrap_stream(ReceiverStream::new(body_rx)))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            response.headers()["x-agentdesktop-error"],
            "upstream-timeout"
        );
        drop(body_tx);
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn forces_shutdown_after_drain_timeout() {
        let fake_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = fake_listener.local_addr().unwrap();
        let fake_app = Router::new().fallback(any(|| async {
            let stream = futures_util::stream::once(async {
                Ok::<_, Infallible>(Bytes::from_static(b"started"))
            })
            .chain(futures_util::stream::pending());
            Body::from_stream(stream)
        }));
        tokio::spawn(async move {
            axum::serve(fake_listener, fake_app).await.unwrap();
        });

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let options = ProxyOptions {
            shutdown_timeout: Duration::from_millis(20),
            ..ProxyOptions::default()
        };
        let server = tokio::spawn(async move {
            serve_with_identity(
                listener,
                Url::parse(&format!("http://{fake_address}")).unwrap(),
                DeploymentMode::Standalone,
                None,
                options,
                async {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });
        let response = Client::new()
            .get(format!("http://{proxy_address}/stream"))
            .send()
            .await
            .unwrap();
        shutdown_tx.send(()).unwrap();

        let error = tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("server ignored shutdown deadline")
            .unwrap()
            .unwrap_err();

        assert!(error.to_string().contains("graceful shutdown exceeded"));
        drop(response);
    }

    #[test]
    fn failure_logging_never_formats_sensitive_error_details() {
        let error =
            anyhow::anyhow!("request failed for https://gateway.example/private?prompt=secret");

        assert_eq!(failure_reason(&error), "upstream_unavailable");
    }

    #[test]
    fn rotates_upstream_pool_for_new_credential_generation() {
        let state = ProxyState {
            clients: Arc::new(StdMutex::new(ClientPool {
                credential_generation: 1,
                device_generation: 0,
                client: Client::new(),
            })),
            device_identity: None,
            upstream: Url::parse("https://gateway.example").unwrap(),
            mode: DeploymentMode::Managed,
            identity: None,
            request_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(1),
            max_in_flight: 1,
            in_flight: Arc::new(Semaphore::new(1)),
            metrics: Arc::new(OperationalMetrics::default()),
        };

        client_for_generation(&state, 2).unwrap();

        assert_eq!(state.clients.lock().unwrap().credential_generation, 2);
    }

    #[test]
    fn rotates_upstream_pool_for_new_device_identity_generation() {
        let device_identity = ManagedDeviceIdentity::new(test_device_identity());
        let state = ProxyState {
            clients: Arc::new(StdMutex::new(ClientPool {
                credential_generation: 0,
                device_generation: 1,
                client: Client::new(),
            })),
            device_identity: Some(device_identity.clone()),
            upstream: Url::parse("https://gateway.example").unwrap(),
            mode: DeploymentMode::Managed,
            identity: None,
            request_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(1),
            max_in_flight: 1,
            in_flight: Arc::new(Semaphore::new(1)),
            metrics: Arc::new(OperationalMetrics::default()),
        };

        device_identity.replace(test_device_identity()).unwrap();
        client_for_generation(&state, 0).unwrap();

        assert_eq!(state.clients.lock().unwrap().device_generation, 2);
    }

    fn test_device_identity() -> reqwest::Identity {
        let key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let certificate = CertificateParams::default().self_signed(&key).unwrap();
        reqwest::Identity::from_pem(
            format!("{}{}", certificate.pem(), key.serialize_pem()).as_bytes(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn managed_identity_replaces_connector_headers_and_preserves_authorization() {
        let captured = Arc::new(Mutex::new(None));
        let fake_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = fake_listener.local_addr().unwrap();
        let fake_app = Router::new()
            .fallback(any(
                move |State(captured): State<Arc<Mutex<Option<HeaderMap>>>>,
                      request: Request<Body>| async move {
                    *captured.lock().await = Some(request.headers().clone());
                    StatusCode::NO_CONTENT
                },
            ))
            .with_state(captured.clone());
        tokio::spawn(async move {
            axum::serve(fake_listener, fake_app).await.unwrap();
        });

        let session = StoredSession {
            issuer: Url::parse("https://identity.example/").unwrap(),
            gateway_origin: Url::parse(&format!("http://{fake_address}/")).unwrap(),
            client_id: "connector".into(),
            audience: "gateway".into(),
            access_token: "managed-token".into(),
            expires_at: u64::MAX,
            scope: "agentgateway.invoke".into(),
            refresh_token: "refresh-token".into(),
            generation: 1,
        };
        let temporary = tempfile::tempdir().unwrap();
        let store = CredentialStore::setup(
            CredentialStorageMode::File,
            &temporary.path().join("identity"),
        )
        .unwrap();
        let identity = ManagedIdentity::new(session, store);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            serve_with_identity(
                listener,
                Url::parse(&format!("http://{fake_address}")).unwrap(),
                DeploymentMode::Managed,
                Some(identity),
                ProxyOptions::default(),
                async {
                    let _ = shutdown_rx.await;
                },
            )
            .await
            .unwrap();
        });

        let response = Client::new()
            .post(format!("http://{proxy_address}/v1/messages"))
            .header("authorization", "Bearer application-token")
            .header("proxy-authorization", "Bearer proxy-token")
            .header("x-agentdesktop-authorization", "DPoP attacker-token")
            .header("dpop", "attacker-proof")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let headers = captured.lock().await.take().unwrap();
        assert_eq!(headers["authorization"], "Bearer application-token");
        assert_eq!(
            headers["x-agentdesktop-authorization"],
            "Bearer managed-token"
        );
        assert!(!headers.contains_key("proxy-authorization"));
        assert!(!headers.contains_key("dpop"));
        shutdown_tx.send(()).unwrap();
    }

    #[tokio::test]
    async fn expired_managed_identity_fails_locally() {
        let temporary = tempfile::tempdir().unwrap();
        let store = CredentialStore::setup(
            CredentialStorageMode::File,
            &temporary.path().join("identity"),
        )
        .unwrap();
        let identity = ManagedIdentity::new(
            StoredSession {
                issuer: Url::parse("https://identity.example/").unwrap(),
                gateway_origin: Url::parse("http://127.0.0.1:9/").unwrap(),
                client_id: "connector".into(),
                audience: "gateway".into(),
                access_token: "expired-token".into(),
                expires_at: 0,
                scope: "agentgateway.invoke".into(),
                refresh_token: "expired-refresh-token".into(),
                generation: 1,
            },
            store,
        );
        assert_eq!(identity.status().await.unwrap(), "refresh-required");
        let state = ProxyState {
            clients: Arc::new(StdMutex::new(ClientPool {
                credential_generation: 0,
                device_generation: 0,
                client: Client::new(),
            })),
            device_identity: None,
            upstream: Url::parse("http://127.0.0.1:9").unwrap(),
            mode: DeploymentMode::Managed,
            identity: Some(identity),
            request_timeout: Duration::from_secs(1),
            connect_timeout: Duration::from_secs(1),
            shutdown_timeout: Duration::from_secs(1),
            max_in_flight: 1,
            in_flight: Arc::new(Semaphore::new(1)),
            metrics: Arc::new(OperationalMetrics::default()),
        };
        let metrics = state.metrics.clone();
        let request = Request::builder()
            .uri("/v1/messages")
            .body(Body::empty())
            .unwrap();

        let response = forward(State(state), request).await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()["x-agentdesktop-error"],
            "identity-unavailable"
        );
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.requests, 1);
        assert_eq!(snapshot.identity_failures, 1);
        assert_eq!(snapshot.upstream_responses, 0);
    }

    #[tokio::test]
    async fn preserves_request_and_response_http_semantics() {
        let captured = Arc::new(Mutex::new(None));
        let fake_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = fake_listener.local_addr().unwrap();
        let fake_app = Router::new()
            .fallback(any(
                move |State(captured): State<Arc<Mutex<Option<CapturedRequest>>>>,
                      request: Request<Body>| async move {
                    let (parts, body) = request.into_parts();
                    let body = body.collect().await.unwrap().to_bytes();
                    *captured.lock().await = Some(CapturedRequest {
                        method: parts.method,
                        uri: parts.uri,
                        headers: parts.headers,
                        body,
                    });
                    (
                        StatusCode::CREATED,
                        [("x-upstream-response", "preserved")],
                        "gateway response",
                    )
                },
            ))
            .with_state(captured.clone());
        tokio::spawn(async move {
            axum::serve(fake_listener, fake_app).await.unwrap();
        });

        let (proxy_address, shutdown) =
            start_proxy(Url::parse(&format!("http://{fake_address}/gateway-base/")).unwrap()).await;
        let response = Client::new()
            .post(format!("http://{proxy_address}/v1/messages?beta=true"))
            .header("x-api-key", "placeholder")
            .header("x-request-marker", "preserved")
            .header(
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            )
            .header("tracestate", "vendor=value")
            .body("claude request")
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()["x-upstream-response"], "preserved");
        assert_eq!(
            response.headers()["traceparent"],
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
        assert_eq!(response.text().await.unwrap(), "gateway response");
        let request = captured.lock().await.take().unwrap();
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.uri, "/gateway-base/v1/messages?beta=true");
        assert_eq!(request.headers["x-api-key"], "placeholder");
        assert_eq!(request.headers["x-request-marker"], "preserved");
        assert_eq!(
            request.headers["traceparent"],
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        );
        assert_eq!(request.headers["tracestate"], "vendor=value");
        assert_eq!(request.body, "claude request");
        let metrics: serde_json::Value = Client::new()
            .get(format!("http://{proxy_address}/_agentdesktop/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(metrics["metrics"]["requests"], 1);
        assert_eq!(metrics["metrics"]["upstream_responses"], 1);
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn streams_response_chunks_without_waiting_for_completion() {
        let (release_tx, release_rx) = oneshot::channel();
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let fake_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = fake_listener.local_addr().unwrap();
        let fake_app = Router::new()
            .fallback(any(
                move |State(release_rx): State<Arc<Mutex<Option<oneshot::Receiver<()>>>>>| async move {
                    let (body_tx, body_rx) = mpsc::channel::<Result<Bytes, Infallible>>(1);
                    let release_rx = release_rx.lock().await.take().unwrap();
                    tokio::spawn(async move {
                        body_tx.send(Ok(Bytes::from_static(b"first"))).await.unwrap();
                        let _ = release_rx.await;
                        body_tx.send(Ok(Bytes::from_static(b"second"))).await.unwrap();
                    });
                    Body::from_stream(ReceiverStream::new(body_rx))
                },
            ))
            .with_state(release_rx);
        tokio::spawn(async move {
            axum::serve(fake_listener, fake_app).await.unwrap();
        });

        let (proxy_address, shutdown) =
            start_proxy(Url::parse(&format!("http://{fake_address}")).unwrap()).await;
        let response = Client::new()
            .get(format!("http://{proxy_address}/v1/messages"))
            .send()
            .await
            .unwrap();
        let mut body = response.bytes_stream();

        let first = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .expect("first chunk was buffered until response completion")
            .unwrap()
            .unwrap();
        assert_eq!(first, "first");
        release_tx.send(()).unwrap();
        assert_eq!(body.next().await.unwrap().unwrap(), "second");
        assert!(body.next().await.is_none());
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn streams_request_chunks_without_waiting_for_completion() {
        let (observed_tx, observed_rx) = oneshot::channel();
        let observed_tx = Arc::new(Mutex::new(Some(observed_tx)));
        let fake_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = fake_listener.local_addr().unwrap();
        let fake_app = Router::new()
            .fallback(any(
                move |State(observed_tx): State<Arc<Mutex<Option<oneshot::Sender<()>>>>>,
                      request: Request<Body>| async move {
                    let mut body = request.into_body().into_data_stream();
                    let first = body.next().await.unwrap().unwrap();
                    observed_tx.lock().await.take().unwrap().send(()).unwrap();
                    let second = body.next().await.unwrap().unwrap();
                    if first == "first" && second == "second" && body.next().await.is_none() {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::BAD_REQUEST
                    }
                },
            ))
            .with_state(observed_tx);
        tokio::spawn(async move {
            axum::serve(fake_listener, fake_app).await.unwrap();
        });

        let (proxy_address, shutdown) =
            start_proxy(Url::parse(&format!("http://{fake_address}")).unwrap()).await;
        let (body_tx, body_rx) = mpsc::channel::<Result<Bytes, Infallible>>(1);
        let request = tokio::spawn(async move {
            Client::new()
                .post(format!("http://{proxy_address}/v1/messages"))
                .body(reqwest::Body::wrap_stream(ReceiverStream::new(body_rx)))
                .send()
                .await
                .unwrap()
        });

        body_tx
            .send(Ok(Bytes::from_static(b"first")))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), observed_rx)
            .await
            .expect("first request chunk was buffered until request completion")
            .unwrap();
        body_tx
            .send(Ok(Bytes::from_static(b"second")))
            .await
            .unwrap();
        drop(body_tx);
        assert_eq!(request.await.unwrap().status(), StatusCode::NO_CONTENT);
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn reports_reachable_gateway_health() {
        let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (proxy_address, shutdown) =
            start_proxy(Url::parse(&format!("http://{}", gateway.local_addr().unwrap())).unwrap())
                .await;

        let response = Client::new()
            .get(format!("http://{proxy_address}/_agentdesktop/healthz"))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.text().await.unwrap(),
            r#"{"status":"ok","mode":"standalone","gateway":"reachable"}"#
        );
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn reports_unreachable_gateway_health() {
        let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = Url::parse(&format!("http://{}", gateway.local_addr().unwrap())).unwrap();
        drop(gateway);
        let (proxy_address, shutdown) = start_proxy(upstream).await;

        let response = Client::new()
            .get(format!("http://{proxy_address}/_agentdesktop/healthz"))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.text().await.unwrap(),
            r#"{"status":"degraded","mode":"standalone","gateway":"unreachable"}"#
        );
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn reports_privacy_safe_operational_status() {
        let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let options = ProxyOptions {
            connect_timeout: Duration::from_millis(101),
            request_timeout: Duration::from_millis(202),
            shutdown_timeout: Duration::from_millis(303),
            max_in_flight: 7,
        };
        let (proxy_address, shutdown) = start_proxy_with_options(
            Url::parse(&format!("http://{}", gateway.local_addr().unwrap())).unwrap(),
            options,
        )
        .await;

        let response: serde_json::Value = Client::new()
            .get(format!("http://{proxy_address}/_agentdesktop/status"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(response["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(response["mode"], "standalone");
        assert_eq!(response["gateway"], "reachable");
        assert_eq!(response["identity"], "not-required");
        assert_eq!(response["in_flight"], 0);
        assert_eq!(response["max_in_flight"], 7);
        assert_eq!(response["connect_timeout_ms"], 101);
        assert_eq!(response["request_timeout_ms"], 202);
        assert_eq!(response["shutdown_timeout_ms"], 303);
        assert_eq!(response["platform"]["os"], std::env::consts::OS);
        assert_eq!(response["platform"]["native_gateway"], true);
        assert_eq!(
            response["platform"]["transparent_capture"],
            cfg!(target_os = "linux")
        );
        assert_eq!(
            response["platform"]["trust_installation"],
            cfg!(target_os = "linux")
        );
        assert_eq!(response["metrics"]["requests"], 0);
        assert_eq!(response["metrics"]["upstream_responses"], 0);
        assert_eq!(response["metrics"]["identity_failures"], 0);
        assert_eq!(response["metrics"]["overload_rejections"], 0);
        assert_eq!(response["metrics"]["upstream_timeouts"], 0);
        assert_eq!(response["metrics"]["upstream_failures"], 0);
        assert_eq!(response["metrics"].as_object().unwrap().len(), 6);
        assert_eq!(response.as_object().unwrap().len(), 11);
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn exits_cleanly_after_shutdown_signal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(serve(
            listener,
            Url::parse("http://127.0.0.1:1").unwrap(),
            DeploymentMode::Standalone,
            async {
                let _ = shutdown_rx.await;
            },
        ));

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("server did not shut down")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn never_retries_non_idempotent_requests_after_upstream_disconnect() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap();
        let observer = tokio::spawn(async move {
            let (stream, _) = upstream.accept().await.unwrap();
            drop(stream);
            tokio::time::timeout(Duration::from_millis(100), upstream.accept())
                .await
                .is_err()
        });
        let (proxy_address, shutdown) =
            start_proxy(Url::parse(&format!("http://{upstream_address}")).unwrap()).await;

        let response = Client::new()
            .post(format!("http://{proxy_address}/v1/messages"))
            .body("must not be replayed")
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert!(
            observer.await.unwrap(),
            "connector retried the POST request"
        );
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn supports_repeated_startup_and_shutdown() {
        for _ in 0..10 {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            let server = tokio::spawn(serve(
                listener,
                Url::parse("http://127.0.0.1:1").unwrap(),
                DeploymentMode::Standalone,
                async {
                    let _ = shutdown_rx.await;
                },
            ));

            shutdown_tx.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(1), server)
                .await
                .expect("server did not shut down")
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn rejects_malformed_http_without_contacting_upstream() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = upstream.local_addr().unwrap();
        let (proxy_address, shutdown) =
            start_proxy(Url::parse(&format!("http://{upstream_address}")).unwrap()).await;
        let mut connection = TcpStream::connect(proxy_address).await.unwrap();
        connection
            .write_all(b"THIS IS NOT HTTP\r\n\r\n")
            .await
            .unwrap();
        connection.shutdown().await.unwrap();
        let mut response = Vec::new();

        tokio::time::timeout(
            Duration::from_secs(1),
            connection.read_to_end(&mut response),
        )
        .await
        .expect("connector did not reject malformed request")
        .unwrap();

        assert!(String::from_utf8_lossy(&response).contains("400 Bad Request"));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), upstream.accept())
                .await
                .is_err(),
            "malformed request reached Agent Gateway"
        );
        let health = Client::new()
            .get(format!("http://{proxy_address}/_agentdesktop/healthz"))
            .send()
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        shutdown.send(()).unwrap();
    }

    #[test]
    fn removes_standard_and_connection_nominated_hop_headers() {
        let mut headers = HeaderMap::from_iter([
            (CONNECTION, "keep-alive, x-private-hop".parse().unwrap()),
            ("keep-alive".parse().unwrap(), "timeout=5".parse().unwrap()),
            ("x-private-hop".parse().unwrap(), "remove".parse().unwrap()),
            ("x-end-to-end".parse().unwrap(), "keep".parse().unwrap()),
        ]);

        remove_hop_by_hop_headers(&mut headers);

        assert!(!headers.contains_key(CONNECTION));
        assert!(!headers.contains_key("keep-alive"));
        assert!(!headers.contains_key("x-private-hop"));
        assert_eq!(headers["x-end-to-end"], "keep");
    }
}
