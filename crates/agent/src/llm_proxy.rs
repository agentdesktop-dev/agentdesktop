mod cache;
mod routes;

pub(crate) use routes::{CAPI_ROUTE, CODEX_ROUTE, PAIRING_HEADER};
use routes::{
    GIT_ROUTE, GIT_SSH_ROUTE, RouteCredential, RouteIdentity, SANDBOX_ROUTE, Upstream,
    bearer_or_header, constant_time_eq, host_is_loopback, match_route, path_escapes_base,
    resembles_route, split_path_pairing, upstream_base,
};

pub(crate) use cache::CredentialCache;
use cache::{Deadline, now_unix};

use std::{convert::Infallible, net::SocketAddr, path::Path, sync::Arc, time::Duration};

use anyhow::Context;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full, combinators::BoxBody};
use hyper::{
    HeaderMap, Method, Request, Response, StatusCode, Uri,
    body::Incoming,
    header::{AUTHORIZATION, CONNECTION, CONTENT_TYPE, HOST, HeaderValue, ORIGIN},
    service::service_fn,
};
use hyper_rustls::HttpsConnectorBuilder;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::{TokioExecutor, TokioIo},
};
use tokio::{net::TcpListener, task::JoinSet};

use agentdesktop_core::config::GitHubTokenSource;

use crate::api::{self, AppState};

pub(crate) mod sessions;
use sessions::SessionRegistry;

type ProxyClient = Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>;
type ProxyBody = BoxBody<Bytes, hyper::Error>;

/// Largest request body the proxy buffers. Bodies are buffered so a request can
/// be retried once with a fresh credential; LLM requests are far below this.
const MAX_REQUEST_BODY: usize = if cfg!(test) {
    64 * 1024
} else {
    32 * 1024 * 1024
};
/// How long to wait for the gateway's response headers. Streaming bodies have
/// no total timeout: a long completion is normal. A non-streaming completion
/// sends nothing until the model is done, so this also bounds those.
const RESPONSE_HEADERS_TIMEOUT: Duration = Duration::from_secs(600);
/// How long a request waits for a controller credential fetch before it fails
/// with `agentdesktop_credential` instead of hanging the tool. The fetch itself
/// bounds its controller connection and call (remote.rs, 15 s), so it normally
/// ends with its own error first; this outer wait covers the OAuth refresh in
/// front of it. Applies to controller-issued credentials only: an OIDC
/// credential may need a browser login.
const CREDENTIAL_FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// File under the daemon state directory that holds the pairing value.
const PAIRING_FILE: &str = "llm-proxy-pairing";

/// Whether the request asks for a WebSocket upgrade: `Connection` lists the
/// `upgrade` token and `Upgrade` lists `websocket`, both case-insensitive.
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

/// The tunnel an upgraded request leaves behind. The connection task awaits it
/// after hyper hands the connection over, so every tunnel runs under the
/// server's JoinSet and dropping the server closes it.
type PendingTunnel =
    Arc<std::sync::Mutex<Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>>>;

/// Relay bytes between the two upgraded connections until one side closes or
/// the gateway credential the tunnel was opened with reaches its deadline.
async fn run_tunnel(
    inbound: hyper::upgrade::OnUpgrade,
    outbound: hyper::upgrade::OnUpgrade,
    deadline: Option<Deadline>,
) {
    let (inbound, outbound) = match tokio::try_join!(inbound, outbound) {
        Ok(streams) => streams,
        Err(error) => {
            tracing::debug!(%error, "LLM proxy upgrade did not complete");
            return;
        }
    };
    let mut inbound = TokioIo::new(inbound);
    let mut outbound = TokioIo::new(outbound);
    let relay = tokio::io::copy_bidirectional(&mut inbound, &mut outbound);
    let expiry = async move {
        let Some(deadline) = deadline else {
            std::future::pending::<()>().await;
            return;
        };
        // The monotonic clock does not advance across a suspend, so the wall
        // clock is checked as well, at most every 10 s.
        loop {
            let now = std::time::Instant::now();
            if now >= deadline.monotonic || now_unix() >= deadline.unix {
                return;
            }
            tokio::time::sleep((deadline.monotonic - now).min(Duration::from_secs(10))).await;
        }
    };
    tokio::select! {
        result = relay => {
            if let Err(error) = result {
                tracing::debug!(%error, "LLM proxy tunnel closed with an error");
            }
        }
        () = expiry => {
            tracing::info!("LLM proxy tunnel closed: the gateway credential it was opened with expired");
        }
    }
}

/// Runtime settings of the proxy listener.
#[derive(Clone)]
pub(crate) struct ProxyConfig {
    /// Client id for requests that match no prefixed route (the original,
    /// hand-configured shape from the README).
    pub default_client_id: String,
    /// Device pairing value required on every route except the sandbox
    /// route, which takes a session pairing from `sessions`.
    pub pairing: Arc<str>,
    /// Per-session pairings accepted on the `/sandbox` route.
    pub sessions: SessionRegistry,
    /// `daemon.containment.git`: the fetch/push allowlists and static
    /// credentials the git routes serve. `None` (or an empty allow list)
    /// means no git egress: the listener is exactly the 3b shape.
    pub git: Option<agentdesktop_core::config::GitConfig>,
    /// `daemon.containment.egressUpstream.url`: when set, the git leg is sent
    /// to this central L7 egress endpoint instead of directly to the remote.
    pub egress_upstream: Option<url::Url>,
}

impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("default_client_id", &self.default_client_id)
            .field("pairing", &"<redacted>")
            .field("sessions", &self.sessions.len(now_unix()))
            .field(
                "git_allow",
                &self.git.as_ref().map_or(0, |git| git.allow.len()),
            )
            .field("egress_upstream", &self.egress_upstream)
            .finish()
    }
}

/// What a reconciler needs to point a client file at the proxy.
///
/// The pairing is a secret: write it only into files created owner-only (0600,
/// or under an owner-only directory; on Windows `secure_fs::atomic_write`
/// relies on the parent directory's ACL). A reconciler that sees `None` here
/// must remove or neutralise a pointer it wrote earlier, so a stale client file
/// never sends the pairing, or the user's own token, to a port the daemon no
/// longer owns.
#[derive(Clone)]
pub struct LlmProxyContext {
    pub address: SocketAddr,
    pub pairing: Arc<str>,
}

impl std::fmt::Debug for LlmProxyContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmProxyContext")
            .field("address", &self.address)
            .field("pairing", &"<redacted>")
            .finish()
    }
}

/// Read the pairing value from the state directory, creating it on first use.
///
/// The value is a bearer secret for the loopback listener, required on every
/// route: it stops other local users on a shared host and browser-origin
/// traffic from using the proxy. It does not, and cannot, stop the current
/// user's own processes, which can ask the daemon for a credential directly.
/// Kept across restarts so client files stay valid.
pub(crate) fn load_or_create_pairing(state_dir: &Path) -> anyhow::Result<Arc<str>> {
    let path = state_dir.join(PAIRING_FILE);
    let existing = std::fs::read_to_string(&path);
    match existing {
        Ok(contents) if contents.trim().len() >= 32 => Ok(Arc::from(contents.trim())),
        // Only a missing or unusable file (too short, not text) is replaced; any
        // other read error (permissions, I/O) is an error, so an existing pairing
        // is never silently rotated and every client file invalidated by a
        // transient fault.
        Err(error)
            if !matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
            ) =>
        {
            Err(anyhow::Error::from(error).context(format!("read {}", path.display())))
        }
        Ok(_) | Err(_) => {
            if !matches!(&existing, Err(error) if error.kind() == std::io::ErrorKind::NotFound) {
                tracing::warn!(path = %path.display(), "LLM proxy pairing file is unusable; regenerating (client files must be re-applied)");
            }
            let mut bytes = [0u8; 32];
            rand::fill(&mut bytes);
            let value = URL_SAFE_NO_PAD.encode(bytes);
            crate::secure_fs::ensure_private_dir(state_dir)?;
            crate::secure_fs::atomic_write(&path, value.as_bytes(), 0o600)
                .with_context(|| format!("write {}", path.display()))?;
            Ok(Arc::from(value.as_str()))
        }
    }
}

pub(crate) async fn serve(
    listener: TcpListener,
    state: AppState,
    config: ProxyConfig,
) -> anyhow::Result<()> {
    serve_with_cache(
        listener,
        state,
        config,
        Arc::new(CredentialCache::default()),
    )
    .await
}

/// `serve` with a caller-provided credential cache (tests pre-seed it).
pub(crate) async fn serve_with_cache(
    listener: TcpListener,
    state: AppState,
    config: ProxyConfig,
    cache: Arc<CredentialCache>,
) -> anyhow::Result<()> {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(Duration::from_secs(30)));
    let connector = HttpsConnectorBuilder::new()
        .with_provider_and_native_roots(std::sync::Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))?
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    let client = Client::builder(TokioExecutor::new()).build(connector);
    let config = Arc::new(config);
    // Dropping the server cancels active connections and their upstream streams.
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                // A per-connection accept error (EMFILE, ECONNABORTED, ENOBUFS)
                // must not end the proxy, let alone the daemon: log, pause briefly
                // so a persistent condition does not spin, and keep accepting.
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::warn!(%error, "LLM proxy accept failed; retrying");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let client = client.clone();
                let state = state.clone();
                let config = config.clone();
                let cache = cache.clone();
                connections.spawn(async move {
                    let pending: PendingTunnel = Arc::default();
                    let slot = pending.clone();
                    let service = service_fn(move |request| {
                        let client = client.clone();
                        let state = state.clone();
                        let config = config.clone();
                        let cache = cache.clone();
                        let slot = slot.clone();
                        async move {
                            let mut log = RequestLog::default();
                            let response = match forward(request, &client, &state, &config, &cache, &slot, &mut log).await {
                                Ok(response) => match log.sandbox.take() {
                                    // A sandbox request: the event is emitted
                                    // when the response body ends, so it can
                                    // carry the bytes delivered and the outcome.
                                    Some(sandbox) => {
                                        let status = response.status().as_u16();
                                        response.map(|body| LoggedBody::new(body, sandbox.with_status(status, None)).boxed())
                                    }
                                    None => response,
                                },
                                Err(error) => {
                                    tracing::warn!(status = %error.status, code = error.code, message = %error.message, "LLM proxy request failed");
                                    if let Some(sandbox) = log.sandbox.take() {
                                        sandbox.with_status(error.status.as_u16(), Some(error.code)).emit(0, "complete");
                                    }
                                    error.into_response()
                                }
                            };
                            Ok::<_, Infallible>(response)
                        }
                    });
                    if let Err(error) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await
                    {
                        tracing::debug!(%error, "LLM proxy connection closed");
                    }
                    // An upgraded connection leaves its tunnel here; awaiting it
                    // in this task keeps it under the server's JoinSet.
                    let tunnel = pending
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .take();
                    if let Some(tunnel) = tunnel {
                        tunnel.await;
                    }
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    tracing::warn!(%error, "LLM proxy connection task failed");
                }
            }
        }
    }
}

/// What `forward` records about a request for the sandbox request log. Only a
/// request on the sandbox route fills `sandbox`; every other route logs
/// nothing new. Drop guard: if the handler future is dropped (client
/// disconnected) before `forward` or `LoggedBody` emitted the log, RequestLog's
/// Drop emits with outcome=canceled.
#[derive(Debug, Default)]
pub(crate) struct RequestLog {
    pub sandbox: Option<SandboxRequest>,
}

impl Drop for RequestLog {
    fn drop(&mut self) {
        // If sandbox is still present, the request was canceled before the
        // normal log paths (error arm or LoggedBody) took it and emitted.
        if let Some(request) = self.sandbox.take() {
            request.emit(0, "canceled");
        }
    }
}

/// One request on the sandbox route, as its log event reports it: never a
/// header, a body, a query string or a pairing value.
#[derive(Debug)]
pub(crate) struct SandboxRequest {
    /// Session id once the pairing matched; `-` until then.
    pub session: Option<String>,
    pub client_id: Option<String>,
    pub method: String,
    /// The path as sent, prefix stripped, query omitted.
    pub path: String,
    /// Upstream status, or the proxy's refusal status.
    pub status: u16,
    /// The refusal code; `-` for a forwarded request.
    pub code: Option<&'static str>,
    /// Buffered request body length; 0 when refused before the body was read.
    pub bytes_in: usize,
    /// Destination host of a git or CONNECT request; `-` on model lines.
    pub host: Option<String>,
    /// `allow` / `refuse` / `upstream` on git and CONNECT lines; `-` on
    /// model lines.
    pub decision: Option<&'static str>,
    /// Set when a session revocation cut the response body short: the line
    /// then reports `outcome="canceled"` (git routes only).
    pub revoked: Option<Arc<std::sync::atomic::AtomicBool>>,
    started: std::time::Instant,
}

impl SandboxRequest {
    fn new(method: &Method, path: &str) -> Self {
        Self {
            session: None,
            client_id: None,
            method: method.to_string(),
            path: path.to_owned(),
            status: 0,
            code: None,
            bytes_in: 0,
            host: None,
            decision: None,
            revoked: None,
            started: std::time::Instant::now(),
        }
    }

    fn with_status(mut self, status: u16, code: Option<&'static str>) -> Self {
        self.status = status;
        self.code = code;
        self
    }

    /// The one `info` event per sandbox request.
    fn emit(&self, bytes_out: u64, outcome: &'static str) {
        tracing::info!(
            session = self.session.as_deref().unwrap_or("-"),
            client_id = self.client_id.as_deref().unwrap_or("-"),
            method = %self.method,
            path = %self.path,
            status = self.status,
            code = self.code.unwrap_or("-"),
            bytes_in = self.bytes_in,
            bytes_out,
            duration_ms = self.started.elapsed().as_millis() as u64,
            outcome,
            host = self.host.as_deref().unwrap_or("-"),
            decision = self.decision.unwrap_or("-"),
            "LLM proxy sandbox request"
        );
    }
}

/// Response body of a sandbox request: counts the data bytes delivered and
/// emits the request log once, when the upstream body ends (`complete`, also
/// when hyper drops a body that has reported its end without polling for the
/// trailing `None`), fails, or is dropped before its end because the client
/// went away (`aborted`).
pub(crate) struct LoggedBody {
    inner: ProxyBody,
    log: SandboxRequest,
    bytes_out: u64,
    emitted: bool,
}

impl LoggedBody {
    fn new(inner: ProxyBody, log: SandboxRequest) -> Self {
        Self {
            inner,
            log,
            bytes_out: 0,
            emitted: false,
        }
    }

    fn finish(&mut self, outcome: &'static str) {
        let revoked = self
            .log
            .revoked
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst));
        let outcome = if revoked { "canceled" } else { outcome };
        if !self.emitted {
            self.emitted = true;
            self.log.emit(self.bytes_out, outcome);
        }
    }
}

impl hyper::body::Body for LoggedBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, hyper::Error>>> {
        let polled = std::pin::Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            std::task::Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    self.bytes_out += data.len() as u64;
                }
                // hyper stops polling once the body reports its end (a
                // Content-Length body after its last frame) and never asks
                // for the trailing `None`: that is a completed response too.
                if hyper::body::Body::is_end_stream(&self.inner) {
                    self.finish("complete");
                }
            }
            std::task::Poll::Ready(Some(Err(_))) => self.finish("aborted"),
            std::task::Poll::Ready(None) => self.finish("complete"),
            std::task::Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for LoggedBody {
    fn drop(&mut self) {
        // An empty body (204, HEAD) is never polled at all; a body dropped
        // before its end was cut off by the client.
        let outcome = if hyper::body::Body::is_end_stream(&self.inner) {
            "complete"
        } else {
            "aborted"
        };
        self.finish(outcome);
    }
}

/// A refused or failed request, rendered as an OpenAI-style JSON error so the
/// LLM clients display the message instead of dropping the connection.
#[derive(Debug)]
pub(crate) struct ProxyError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ProxyError {
    fn pairing_invalid() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "pairing_invalid",
            "agentdesktop proxy: pairing value missing or wrong; re-apply the managed configuration",
        )
    }

    /// The sandbox route's refusal: a session pairing that matches nothing.
    fn session_invalid() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "pairing_invalid",
            "agentdesktop proxy: session pairing missing, expired or revoked; start a new session",
        )
    }

    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn into_response(self) -> Response<ProxyBody> {
        let body = serde_json::json!({
            "error": {
                "message": self.message,
                "type": "agentdesktop_error",
                "code": self.code,
            }
        })
        .to_string();
        Response::builder()
            .status(self.status)
            .header(CONTENT_TYPE, "application/json")
            .body(
                Full::new(Bytes::from(body))
                    .map_err(|never| match never {})
                    .boxed(),
            )
            .expect("static response")
    }
}

impl ProxyError {
    /// A failure to obtain the gateway identity for this device: not enrolled,
    /// revoked, controller unreachable, OIDC refresh failed. The status comes
    /// from the credential path; the code tells the client what kind of problem
    /// it is so the message is actionable ("run agentdesktop login" and so on).
    fn credential((status, message): (StatusCode, String)) -> Self {
        Self::new(
            status,
            "agentdesktop_credential",
            format!("agentdesktop: no gateway credential for this device: {message}"),
        )
    }
}

async fn forward(
    mut request: Request<Incoming>,
    client: &ProxyClient,
    state: &AppState,
    config: &ProxyConfig,
    cache: &Arc<CredentialCache>,
    tunnel_slot: &PendingTunnel,
    log: &mut RequestLog,
) -> Result<Response<ProxyBody>, ProxyError> {
    // Pure string work on the path first; no configuration is read yet. It
    // comes before every check so a refused sandbox request is logged with
    // its route and path.
    let path_and_query = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str())
        .to_owned();
    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path_and_query.as_str(), None),
    };
    // Matched once; the route decides pairing placement, upgrade handling,
    // the ping exemption, client id, credential mode and upstream.
    let matched = match_route(path);
    let matched_route = matched.map(|(route, _)| route);
    if let Some((route, rest)) = matched
        && route.identity == RouteIdentity::Session
    {
        log.sandbox = Some(SandboxRequest::new(request.method(), rest));
    }
    // CONNECT and absolute-form requests (the forward-proxy request forms)
    // skip the Host-loopback gate (their target is not this listener), carry
    // their pairing as Proxy-Authorization credentials, and are ALWAYS
    // refused after the pairing check: the egress architecture is L7 git
    // routes on the loopback listener, never tunnels (AC 3).
    if request.method() == Method::CONNECT || request.uri().scheme().is_some() {
        return handle_connect_request(&request, config, log);
    }
    if !host_is_loopback(request.headers()) {
        return Err(ProxyError::new(
            StatusCode::FORBIDDEN,
            "host_not_allowed",
            "agentdesktop proxy: Host must be a loopback address",
        ));
    }
    // This listener is for local native clients, not browser scripts.
    if request.headers().contains_key(ORIGIN) {
        return Err(ProxyError::new(
            StatusCode::FORBIDDEN,
            "browser_not_allowed",
            "agentdesktop proxy: browser requests are not supported",
        ));
    }
    if request.method() == Method::OPTIONS {
        return Err(ProxyError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "agentdesktop proxy: OPTIONS is not supported",
        ));
    }
    // The origin-form git routes (AC 5) and the ssh refusal route: session
    // paired (the generated gitconfig carries the pairing in PAIRING_HEADER),
    // served only for the hosts `git.allow` names. Not reachable without a
    // registered session.
    let git_route = match (route_rest(path, GIT_SSH_ROUTE), route_rest(path, GIT_ROUTE)) {
        (Some(rest), _) => Some((true, rest)),
        (None, Some(rest)) => Some((false, rest)),
        (None, None) => None,
    };
    if let Some((ssh_refusal, rest)) = git_route {
        log.sandbox = Some(SandboxRequest::new(request.method(), rest));
        let offered = request
            .headers()
            .get(PAIRING_HEADER)
            .and_then(|value| value.to_str().ok());
        let found = offered
            .and_then(|offered| config.sessions.authorize(offered, now_unix()))
            .ok_or_else(ProxyError::session_invalid)?;
        if let Some(sandbox) = log.sandbox.as_mut() {
            sandbox.session = Some(found.id.clone());
            sandbox.client_id = Some(found.client_id.clone());
            sandbox.decision = Some("refuse");
        }
        if path_escapes_base(path) {
            return Err(ProxyError::new(
                StatusCode::BAD_REQUEST,
                "path_invalid",
                "agentdesktop proxy: path must not contain '..' segments",
            ));
        }
        if ssh_refusal {
            return Ok(ssh_refused_response(rest, query, log));
        }
        return handle_git_request(request, client, config, found, rest, query, log).await;
    }
    // A pairing value is required on every route: the device pairing, or a
    // session pairing on the sandbox route. It travels in its own
    // header, so Authorization keeps one meaning (the client's own token on the
    // pass-through shapes) and the pairing is never mistaken for a credential.
    // The CAPI route's client (VS Code) cannot add a header, so there the
    // pairing is the first path segment and the header is ignored. Checked
    // before anything that reads configuration, so an unpaired caller learns
    // nothing about this device and costs it no work.
    let pairing_ok =
        |offered: &str| constant_time_eq(offered.as_bytes(), config.pairing.as_bytes());
    // The sandbox route's session, once its pairing matched.
    let mut session: Option<sessions::SessionRef> = None;
    let capi_rest = match matched {
        Some((route, rest)) if route.pairing_in_path => {
            let Some((segment, remainder)) = split_path_pairing(rest) else {
                return Err(ProxyError::pairing_invalid());
            };
            if !pairing_ok(segment) {
                return Err(ProxyError::pairing_invalid());
            }
            Some(remainder.to_owned())
        }
        // The sandbox route takes a session pairing in the header: looked up
        // and its idle timeout restarted under one lock. The device pairing
        // never opens this route (a file that holds it must not become a
        // session), and a session pairing opens no other route (below, it
        // never equals the device pairing).
        Some((route, _)) if route.identity == RouteIdentity::Session => {
            let offered = request
                .headers()
                .get(PAIRING_HEADER)
                .and_then(|value| value.to_str().ok());
            let found = offered
                .and_then(|offered| config.sessions.authorize(offered, now_unix()))
                .ok_or_else(ProxyError::session_invalid)?;
            if let Some(sandbox) = log.sandbox.as_mut() {
                sandbox.session = Some(found.id.clone());
                sandbox.client_id = Some(found.client_id.clone());
            }
            session = Some(found);
            None
        }
        _ => {
            let offered = request
                .headers()
                .get(PAIRING_HEADER)
                .and_then(|value| value.to_str().ok());
            if !offered.is_some_and(pairing_ok) {
                return Err(ProxyError::pairing_invalid());
            }
            None
        }
    };
    let websocket = is_websocket_upgrade(request.headers());
    if websocket && !matched_route.is_some_and(|route| route.tunnels_upgrades) {
        return Err(ProxyError::new(
            StatusCode::BAD_REQUEST,
            "upgrade_not_supported",
            "agentdesktop proxy: WebSocket upgrades are supported on the /vscode-copilot-capi route only",
        ));
    }
    if path_escapes_base(path) {
        return Err(ProxyError::new(
            StatusCode::BAD_REQUEST,
            "path_invalid",
            "agentdesktop proxy: path must not contain '..' segments",
        ));
    }
    if matched.is_none() && resembles_route(path) {
        // A first segment that looks like a route name but is not one (case,
        // suffix, doubled slash) is a mistyped client file, not a request for
        // the hand-configured route with its different identity and credential mode.
        return Err(ProxyError::new(
            StatusCode::NOT_FOUND,
            "route_unknown",
            "agentdesktop proxy: unknown route; the managed prefixes are /copilot-cli, /vscode-copilot, /vscode-copilot-passthrough, /vscode-copilot-capi and /sandbox",
        ));
    }
    let effective =
        api::load_effective_config(&state.config, &state.state_dir).map_err(|error| {
            tracing::warn!(
                error = %format!("{error:#}"),
                "LLM proxy could not read the applied configuration"
            );
            ProxyError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "agentdesktop_unavailable",
                "agentdesktop: could not read the applied configuration; see the daemon log",
            )
        })?;
    let gateway = effective.llm_gateway.as_ref().ok_or_else(|| {
        ProxyError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "agentdesktop_unavailable",
            "agentdesktop: no LLM gateway configured on this device yet",
        )
    })?;

    // Which route, which client id, which credential mode, which upstream.
    let (client_id, credential_mode, base, rest) = match matched {
        Some((route, rest)) => {
            if route.upstream == Upstream::ProxyUrl && gateway.proxy_url.is_none() {
                // The pass-through shape needs the gateway route that restores the
                // client token; sending x-llm-token to the plain LLM route would
                // fail in a way that looks like a gateway fault.
                return Err(ProxyError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "agentdesktop_unavailable",
                    "agentdesktop proxy: this route needs llmGateway.proxyUrl, which is not configured",
                ));
            }
            (
                match route.identity {
                    RouteIdentity::Fixed(client_id) => client_id.to_owned(),
                    RouteIdentity::Session => session
                        .as_ref()
                        .map(|session| session.client_id.clone())
                        .ok_or_else(ProxyError::session_invalid)?,
                },
                Some(route.credential),
                upstream_base(gateway, route.upstream),
                capi_rest.as_deref().unwrap_or(rest),
            )
        }
        // No prefix: the original hand-configured shape. Credential mode comes
        // from llmGateway.githubOAuth, upstream from proxyUrl.
        None => (
            config.default_client_id.clone(),
            None,
            upstream_base(gateway, Upstream::ProxyUrl),
            path,
        ),
    };
    let uri: Uri = match query {
        Some(query) => format!("{}{rest}?{query}", base.as_str().trim_end_matches('/')),
        None => format!("{}{rest}", base.as_str().trim_end_matches('/')),
    }
    .parse()
    .map_err(|error: hyper::http::uri::InvalidUri| {
        ProxyError::new(
            StatusCode::BAD_GATEWAY,
            "gateway_unreachable",
            error.to_string(),
        )
    })?;

    // Capture the client's own credential before the headers are cleared. A
    // client such as VS Code Copilot has already done its provider handshake and
    // sends the resulting token; on a pass-through route (or with
    // `githubOAuth.source: request` on the default route) that token is what
    // goes upstream, and the daemon only adds the gateway identity.
    //
    // x-llm-token wins over Authorization so a caller can send both: its identity
    // in one and the provider credential in the other.
    // The pairing value is never a client credential, even if a client put it in
    // both places.
    let client_credential = bearer_or_header(request.headers(), "x-llm-token")
        .filter(|value| !constant_time_eq(value.as_bytes(), config.pairing.as_bytes()));
    // GitHub's health ping carries no credential and needs none: on the CAPI
    // route a GET without one is forwarded as is instead of being refused.
    let credential_less_ping = matched_route.is_some_and(|route| route.credential_less_ping)
        && capi_rest.as_deref() == Some("/_ping")
        && request.method() == Method::GET
        && client_credential.is_none();
    // The inbound upgrade handle is taken before the body is consumed.
    let inbound_upgrade = websocket.then(|| hyper::upgrade::on(&mut request));

    // Buffer the body (capped) so the request can be retried once with a fresh
    // credential; the response is streamed as it arrives.
    let (parts, body) = request.into_parts();
    let body = http_body_util::Limited::new(body, MAX_REQUEST_BODY)
        .collect()
        .await
        .map_err(|error| {
            if error
                .downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                ProxyError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "body_too_large",
                    format!(
                        "agentdesktop proxy: request body exceeds {} bytes",
                        MAX_REQUEST_BODY
                    ),
                )
            } else {
                ProxyError::new(
                    StatusCode::BAD_REQUEST,
                    "body_invalid",
                    format!("agentdesktop proxy: could not read the request body: {error}"),
                )
            }
        })?
        .to_bytes();
    if let Some(sandbox) = log.sandbox.as_mut() {
        sandbox.bytes_in = body.len();
    }
    let mut request = Request::from_parts(parts, ());

    strip_hop_headers(request.headers_mut());
    if websocket {
        // The upgrade is the point of the request: put back the two hop headers
        // that carry it (the Sec-WebSocket-* headers are end-to-end and stay).
        request
            .headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("Upgrade"));
        request
            .headers_mut()
            .insert("upgrade", HeaderValue::from_static("websocket"));
    }
    for header in [
        AUTHORIZATION.as_str(),
        "x-api-key",
        "api-key",
        "x-llm-token",
        PAIRING_HEADER,
    ] {
        request.headers_mut().remove(header);
    }
    // Both branches put a bare token in x-llm-token. The gateway is the one
    // that re-forms the header, with `"Bearer " + request.headers['x-llm-token']`,
    // so the prefix is added in exactly one place.
    let upstream_token = match credential_mode {
        Some(RouteCredential::Gateway) => None,
        Some(RouteCredential::Passthrough) if credential_less_ping => None,
        Some(RouteCredential::Passthrough) => Some(client_credential.ok_or_else(|| {
            ProxyError::new(
                StatusCode::UNAUTHORIZED,
                "client_credential_missing",
                "agentdesktop proxy: no client credential to pass through; send it in Authorization or x-llm-token",
            )
        })?),
        None => match &gateway.github_oauth {
            None => None,
            Some(github) => Some(match github.source {
                GitHubTokenSource::DeviceFlow => {
                    let client_id = github.client_id.as_deref().ok_or_else(|| {
                        ProxyError::new(
                            StatusCode::INTERNAL_SERVER_ERROR,
                            "agentdesktop_unavailable",
                            "githubOAuth.clientId is required for deviceFlow",
                        )
                    })?;
                    crate::github_oauth::credential(client_id, &state.state_dir, None)
                        .await
                        .map_err(|error| ProxyError::new(StatusCode::BAD_GATEWAY, "gateway_unreachable", format!("GitHub OAuth: {error:#}")))?
                        .credential
                }
                // Fail rather than forward a request that cannot succeed: without a
                // client credential the gateway would swap in an empty Authorization
                // and the provider would reject it with an error that looks like a
                // gateway fault.
                GitHubTokenSource::Request => client_credential.ok_or_else(|| {
                    ProxyError::new(
                        StatusCode::UNAUTHORIZED,
                        "client_credential_missing",
                        "no client credential to forward; send it in Authorization or x-llm-token",
                    )
                })?,
            }),
        },
    };
    let client_token_sent = upstream_token.is_some();
    let subscription = client_token_sent
        && matched_route.is_some_and(|route| route.prefix == CODEX_ROUTE)
        && effective.programs.subscription_auth(&client_id)
            == Some(agentdesktop_core::config::SubscriptionAuth::ClientPassthrough);
    if let Some(token) = upstream_token {
        let mut value = HeaderValue::from_str(&token).map_err(|_| {
            ProxyError::new(
                StatusCode::BAD_GATEWAY,
                "gateway_unreachable",
                "invalid client credential header",
            )
        })?;
        value.set_sensitive(true);
        request.headers_mut().insert("x-llm-token", value);
    }
    request.headers_mut().insert(
        HOST,
        HeaderValue::from_str(
            uri.authority()
                .map(|authority| authority.as_str())
                .unwrap_or(""),
        )
        .map_err(|_| {
            ProxyError::new(
                StatusCode::BAD_GATEWAY,
                "gateway_unreachable",
                "invalid gateway host",
            )
        })?,
    );
    *request.uri_mut() = uri;
    // Identity last: no controller or OIDC round trip for a request that was
    // going to be refused anyway. Controller credentials are cached per client
    // id; on a 401 from the gateway the request is sent once more with a fresh
    // credential, which covers a token that expired between fetch and use.
    let authenticated = gateway.authentication.is_some();
    let controller_issued = matches!(
        gateway.authentication,
        Some(agentdesktop_core::config::LlmGatewayAuthentication::ControllerJwt { .. })
    );
    // Only controller-issued credentials are cached, keyed by client id,
    // gateway and audience, and tagged with the enrolled device so a logout or
    // re-enrollment (a different identity.json) never reuses an old token. The
    // device id is read from identity.json alone; the secret store is not opened.
    let cache_key = match &gateway.authentication {
        Some(agentdesktop_core::config::LlmGatewayAuthentication::ControllerJwt {
            audience,
            ..
        }) => crate::identity::load_device_id(&state.state_dir.join("identity.json"))
            .ok()
            .flatten()
            .map(|device_id| {
                let mut key = format!("{client_id}\u{0}{}\u{0}{audience}", gateway.url);
                if subscription {
                    key.push_str("\0subscription");
                }
                (key, device_id)
            }),
        _ => None,
    };
    let mut retried = false;
    loop {
        let mut attempt =
            Request::from_parts(request.clone().into_parts().0, Full::new(body.clone()));
        let mut from_cache = false;
        // A credential fetched for this attempt. It is cached right away, so
        // requests that arrive while this one waits for the model use it too,
        // and dropped again if the gateway rejects it.
        let mut fetched_now = false;
        let mut attempt_credential: Option<String> = None;
        // The expiry of the credential used, read together with it: a later
        // lookup could miss an entry that expired or was invalidated meanwhile.
        let mut credential_deadline: Option<Deadline> = None;
        if authenticated {
            let cached = cache_key
                .as_ref()
                .and_then(|(key, device_id)| cache.lookup(key, device_id));
            let credential = match cached {
                Some((credential, deadline)) => {
                    from_cache = true;
                    credential_deadline = Some(deadline);
                    credential
                }
                None => {
                    // The fetch runs on its own task so a client disconnect or the
                    // wait below does not cancel it half-way (an OAuth refresh must
                    // finish and be stored). The fetch bounds its own controller
                    // call, so a detached fetch ends by itself; an OIDC acquisition
                    // may wait for a browser login and is not bounded. The task
                    // holds the key's flight lock, so of concurrent misses one
                    // fetches and the others find its credential in the cache.
                    let fetch = {
                        let state = state.clone();
                        // On the sandbox route, the daemon never starts an interactive
                        // sign-in on behalf of a box: if the stored session is
                        // unusable, the box's request fails and the user signs in on
                        // the host.
                        let effective = if session.is_some() {
                            api::non_interactive(effective.clone())
                        } else {
                            effective.clone()
                        };
                        let client_id = client_id.clone();
                        let cache = cache.clone();
                        let cache_key = cache_key.clone();
                        tokio::spawn(async move {
                            let fetch = || {
                                api::gateway_proxy_credential(
                                    &state,
                                    &effective,
                                    &client_id,
                                    subscription,
                                )
                            };
                            match &cache_key {
                                Some((key, device_id)) => {
                                    cache.get_or_fetch(key, device_id, fetch).await
                                }
                                None => fetch().await.map(|fetched| {
                                    let deadline =
                                        Deadline::from_unix(fetched.expires_at_unix_seconds);
                                    (fetched.credential, deadline, false)
                                }),
                            }
                        })
                    };
                    let outcome = if controller_issued {
                        tokio::time::timeout(CREDENTIAL_FETCH_TIMEOUT, fetch)
                            .await
                            .map_err(|_| {
                                ProxyError::credential((
                                    StatusCode::GATEWAY_TIMEOUT,
                                    "the credential fetch (token refresh or controller call) did not finish in time".to_owned(),
                                ))
                            })?
                    } else {
                        fetch.await
                    };
                    let fetched = outcome
                        .map_err(|error| {
                            ProxyError::credential((
                                StatusCode::INTERNAL_SERVER_ERROR,
                                format!("credential fetch failed: {error}"),
                            ))
                        })?
                        .map_err(|(status, message)| {
                            // On the sandbox route, a 401 means deviceAuthorization is
                            // false or the token is missing/expired. The endpoint for
                            // device flow (agentdesktop-headless login) refuses with 424
                            // in that configuration, so the honest guidance is to sign in
                            // on the host session (the daemon's own environment).
                            let message = if session.is_some() && status == StatusCode::UNAUTHORIZED {
                                "LLM gateway sign-in required. Sign in on the host (the machine running the daemon), not inside the sandbox.".to_owned()
                            } else {
                                message
                            };
                            ProxyError::credential((status, message))
                        })?;
                    let (credential, deadline, cached) = fetched;
                    from_cache = cached;
                    fetched_now = !cached;
                    credential_deadline = Some(deadline);
                    credential
                }
            };
            let mut authorization = HeaderValue::from_str(&format!("Bearer {credential}"))
                .map_err(|_| {
                    ProxyError::new(
                        StatusCode::BAD_GATEWAY,
                        "gateway_unreachable",
                        "invalid gateway credential header",
                    )
                })?;
            authorization.set_sensitive(true);
            attempt.headers_mut().insert(AUTHORIZATION, authorization);
            attempt_credential = Some(credential);
        }
        let response = tokio::time::timeout(RESPONSE_HEADERS_TIMEOUT, client.request(attempt))
            .await
            .map_err(|_| {
                ProxyError::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "gateway_timeout",
                    "agentdesktop proxy: the gateway did not answer in time",
                )
            })?
            .map_err(|error| {
                ProxyError::new(
                    StatusCode::BAD_GATEWAY,
                    "gateway_unreachable",
                    format!("agentdesktop proxy: gateway unreachable: {error}"),
                )
            })?;
        // Retry only when the rejected credential came from the cache and no
        // client token was forwarded: a freshly fetched credential that is
        // rejected would be rejected again, and with a client token in
        // x-llm-token the 401 may be about that token, not the gateway identity
        // (the cached gateway credential is then kept: dropping it on every
        // client-token 401 would turn a bad client token into a controller
        // fetch per request).
        if response.status() == StatusCode::UNAUTHORIZED
            && from_cache
            && !client_token_sent
            && !retried
        {
            tracing::info!(
                client_id,
                "gateway rejected a cached credential; retrying once with a fresh one"
            );
            if let Some((key, _)) = &cache_key {
                let rejected = attempt_credential.as_deref().unwrap_or_default();
                cache.invalidate_if(key, rejected);
            }
            retried = true;
            continue;
        }
        if response.status() == StatusCode::UNAUTHORIZED
            && fetched_now
            && let (Some((key, _)), Some(rejected)) = (&cache_key, &attempt_credential)
        {
            cache.invalidate_if(key, rejected);
        }
        let mut response = response;
        if websocket && response.status() == StatusCode::SWITCHING_PROTOCOLS {
            // The upstream accepted the upgrade. The 101 goes back with the
            // hop headers stripped except the two that carry the upgrade
            // (the Sec-WebSocket-* headers are end-to-end and stay), and the
            // tunnel between the two upgraded connections is handed to the
            // connection task. It closes when either side closes or when the
            // gateway credential it was opened with expires (its own expiry,
            // not the cache's reuse window). Without a gateway credential
            // (no authentication configured) there is no deadline.
            let deadline = credential_deadline;
            let upgrade_value = response.headers().get("upgrade").cloned();
            strip_hop_headers(response.headers_mut());
            response
                .headers_mut()
                .insert(CONNECTION, HeaderValue::from_static("Upgrade"));
            if let Some(value) = upgrade_value {
                response.headers_mut().insert("upgrade", value);
            }
            let outbound_upgrade = hyper::upgrade::on(&mut response);
            let inbound_upgrade = inbound_upgrade.ok_or_else(|| {
                ProxyError::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "agentdesktop_unavailable",
                    "agentdesktop proxy: upgrade handle missing",
                )
            })?;
            *tunnel_slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Box::pin(run_tunnel(
                inbound_upgrade,
                outbound_upgrade,
                deadline,
            )));
            return Ok(response.map(|_| {
                Empty::<Bytes>::new()
                    .map_err(|never| match never {})
                    .boxed()
            }));
        }
        strip_hop_headers(response.headers_mut());
        return Ok(response.map(BodyExt::boxed));
    }
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

// --- PR 4: git over the relay (origin-form routes) and the CONNECT refusal.

/// The smart-HTTP endpoint shapes the git routes serve (AC 5). Everything
/// else under `/git/...` — including dumb-protocol requests — is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitEndpoint {
    FetchDiscovery,
    FetchRpc,
    PushDiscovery,
    PushRpc,
}

impl GitEndpoint {
    fn is_push(self) -> bool {
        matches!(self, Self::PushDiscovery | Self::PushRpc)
    }

    fn is_rpc(self) -> bool {
        matches!(self, Self::FetchRpc | Self::PushRpc)
    }

    fn service(self) -> &'static str {
        match self {
            Self::FetchDiscovery | Self::FetchRpc => "git-upload-pack",
            Self::PushDiscovery | Self::PushRpc => "git-receive-pack",
        }
    }
}

/// Classifies a request under the git route by the final path segment and the
/// `service` query parameter only: discovery `GET <repo>/info/refs?service=…`,
/// RPC `POST <repo>/git-upload-pack` / `<repo>/git-receive-pack`. `None` for
/// every other shape, notably dumb-protocol requests (`info/refs` without
/// `service`, `/objects/...`).
fn classify_git_endpoint(path: &str, query: Option<&str>) -> Option<GitEndpoint> {
    let service = query.and_then(|query| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("service="))
    });
    if path.ends_with("/info/refs") {
        return match service {
            Some("git-upload-pack") => Some(GitEndpoint::FetchDiscovery),
            Some("git-receive-pack") => Some(GitEndpoint::PushDiscovery),
            _ => None,
        };
    }
    match path.rsplit('/').next() {
        Some("git-upload-pack") => Some(GitEndpoint::FetchRpc),
        Some("git-receive-pack") => Some(GitEndpoint::PushRpc),
        _ => None,
    }
}

/// A policy refusal rendered as git itself expects it: `200 OK` with the
/// service's advertisement (discovery) or result (RPC) content type and a
/// single `ERR` pkt-line, which git prints as `fatal: remote error: <message>`.
fn pkt_line_err_response(service: &str, is_rpc: bool, message: &str) -> Response<ProxyBody> {
    let kind = if is_rpc { "result" } else { "advertisement" };
    let line = format!("ERR {message}\n");
    let body = format!("{:04x}{line}", line.len() + 4);
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, format!("application/x-{service}-{kind}"))
        .body(
            Full::new(Bytes::from(body))
                .map_err(|never| match never {})
                .boxed(),
        )
        .expect("static response")
}

/// The upload-pack shape of [`pkt_line_err_response`]; the receive-pack legs
/// go through `pkt_line_err_response` with their own service name.
fn pkt_line_err(message: &str, is_rpc: bool) -> Response<ProxyBody> {
    pkt_line_err_response("git-upload-pack", is_rpc, message)
}

/// The path remainder after `prefix` when `path` matches it at a `/` boundary
/// (the `match_route` rule), starting with `/`.
fn route_rest<'p>(path: &'p str, prefix: &str) -> Option<&'p str> {
    let rest = path.strip_prefix(prefix)?;
    if rest.is_empty() {
        Some("/")
    } else {
        rest.starts_with('/').then_some(rest)
    }
}

/// Whether `url` is under the configured prefix (AC 5): host compared
/// case-insensitively (both sides go through `url::Url`, which lowercases
/// hosts), explicit and default ports equal under `port_or_known_default`,
/// and the path matched on the configured prefix per path element —
/// case-sensitive, trailing slash normalized, never on a partial segment.
/// The scheme is not compared: startup validation pins configured prefixes
/// to `https`, and the relay connects with the prefix's scheme.
fn git_prefix_matches(prefix: &str, url: &url::Url) -> bool {
    let Ok(prefix) = url::Url::parse(prefix) else {
        return false;
    };
    let (Some(prefix_host), Some(host)) = (prefix.host_str(), url.host_str()) else {
        return false;
    };
    if !prefix_host.eq_ignore_ascii_case(host)
        || prefix.port_or_known_default() != url.port_or_known_default()
    {
        return false;
    }
    let wanted: Vec<&str> = prefix.path().split('/').filter(|s| !s.is_empty()).collect();
    let got: Vec<&str> = url.path().split('/').filter(|s| !s.is_empty()).collect();
    got.len() >= wanted.len() && got[..wanted.len()] == wanted[..]
}

/// Whether a redirect target stays inside the allow entry: same scheme as the
/// entry (a Location may not downgrade the credentialed leg, e.g.
/// `http://host:443/…`, which the port-aware prefix match admits) and under
/// its prefix.
fn git_redirect_stays_in(allow_entry: &str, next: &url::Url) -> bool {
    url::Url::parse(allow_entry).is_ok_and(|entry| entry.scheme() == next.scheme())
        && git_prefix_matches(allow_entry, next)
}

/// How specific a configured prefix is: its path-element count, then its
/// length (the longest-prefix rule for credential selection).
fn git_prefix_specificity(prefix: &str) -> (usize, usize) {
    let elements = url::Url::parse(prefix)
        .map(|parsed| parsed.path().split('/').filter(|s| !s.is_empty()).count())
        .unwrap_or(0);
    (elements, prefix.trim_end_matches('/').len())
}

/// The longest-prefix credential entry for the destination, if any. The
/// caller gates on the allowlist first: no credential is ever attached for a
/// host that is not in `git.allow`.
fn select_git_credential<'c>(
    credentials: &'c [agentdesktop_core::config::GitCredentialEntry],
    url: &url::Url,
) -> Option<&'c agentdesktop_core::config::GitCredentialEntry> {
    credentials
        .iter()
        .filter(|entry| git_prefix_matches(&entry.url_prefix, url))
        .max_by_key(|entry| git_prefix_specificity(&entry.url_prefix))
}

/// The credential the relay attaches for `url`: `None` unless the destination
/// is admitted by `git.allow` (no credential is ever injected for a host
/// outside the allowlist, chained or not), else the longest-prefix static
/// entry, if any.
fn git_credential_for<'c>(
    git: &'c agentdesktop_core::config::GitConfig,
    url: &url::Url,
) -> Option<&'c agentdesktop_core::config::GitCredentialEntry> {
    if !git
        .allow
        .iter()
        .any(|prefix| git_prefix_matches(prefix, url))
    {
        return None;
    }
    select_git_credential(&git.credentials, url)
}

/// Answer on the `/git-ssh` refusal route: the generated gitconfig maps
/// ssh-form remotes of allowlisted hosts without a
/// `daemon.containment.git.ssh.hosts` entry here so the user sees a pkt-line
/// `ERR` instead of a deny-mode timeout.
fn ssh_refused_response(
    rest: &str,
    query: Option<&str>,
    log: &mut RequestLog,
) -> Response<ProxyBody> {
    if let Some(sandbox) = log.sandbox.as_mut() {
        let host = rest.trim_start_matches('/').split('/').next().unwrap_or("");
        sandbox.host = Some(host.to_owned());
        sandbox.decision = Some("refuse");
    }
    const MESSAGE: &str = "ssh remotes are not enabled for this host; use the https remote or add the host to daemon.containment.git.ssh.hosts";
    match classify_git_endpoint(rest, query) {
        Some(endpoint) if endpoint.is_push() => {
            pkt_line_err_response(endpoint.service(), endpoint.is_rpc(), MESSAGE)
        }
        Some(endpoint) => pkt_line_err(MESSAGE, endpoint.is_rpc()),
        None => pkt_line_err(MESSAGE, false),
    }
}

/// CONNECT and absolute-form requests: the pairing check first (the session
/// pairing travels as `Proxy-Authorization: Basic base64("<pairing>:")`,
/// compared constant-time through the registry's digest comparison), then an
/// unconditional refusal — the product egress contract is L7 routes, there
/// is no tunnel to establish (AC 3). Never forwards, never returns `Ok`.
fn handle_connect_request(
    request: &Request<Incoming>,
    config: &ProxyConfig,
    log: &mut RequestLog,
) -> Result<Response<ProxyBody>, ProxyError> {
    let host = request
        .uri()
        .authority()
        .map(|authority| authority.to_string())
        .or_else(|| {
            request
                .headers()
                .get(HOST)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "-".to_owned());
    let mut entry = SandboxRequest::new(request.method(), "/");
    entry.host = Some(host);
    entry.decision = Some("refuse");
    log.sandbox = Some(entry);
    let session = request
        .headers()
        .get("proxy-authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            let (scheme, credentials) = value.split_once(' ')?;
            scheme
                .eq_ignore_ascii_case("basic")
                .then(|| credentials.trim())
        })
        .and_then(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        })
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|credentials| {
            // Pairing as the username, empty password.
            let (pairing, password) = credentials.split_once(':')?;
            password
                .is_empty()
                .then(|| config.sessions.authorize(pairing, now_unix()))
                .flatten()
        })
        .ok_or_else(ProxyError::session_invalid)?;
    if let Some(sandbox) = log.sandbox.as_mut() {
        sandbox.session = Some(session.id);
        sandbox.client_id = Some(session.client_id);
    }
    Err(ProxyError::new(
        StatusCode::FORBIDDEN,
        "egress_not_supported",
        "agentdesktop proxy: egress tunnels are not supported; outbound access is limited to the configured git routes",
    ))
}

/// How often an in-flight git request (and the ssh broker and legs) checks
/// whether its session was revoked.
pub(crate) const REVOCATION_POLL: Duration = Duration::from_millis(50);

/// A git response body that ends as soon as its session is revoked (AC 1).
/// The registry is read without restarting the idle timeout; the revocation
/// is recorded in `revoked` so the request log reports `outcome="canceled"`.
struct RevocableBody {
    inner: ProxyBody,
    sessions: SessionRegistry,
    session_id: String,
    check: std::pin::Pin<Box<tokio::time::Sleep>>,
    revoked: Arc<std::sync::atomic::AtomicBool>,
}

impl RevocableBody {
    fn cut(&self) -> bool {
        if self.revoked.load(std::sync::atomic::Ordering::SeqCst) {
            return true;
        }
        if self.sessions.live(&self.session_id, now_unix()) {
            return false;
        }
        self.revoked
            .store(true, std::sync::atomic::Ordering::SeqCst);
        true
    }
}

impl hyper::body::Body for RevocableBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Bytes>, hyper::Error>>> {
        if self.cut() {
            return std::task::Poll::Ready(None);
        }
        let polled = std::pin::Pin::new(&mut self.inner).poll_frame(cx);
        if polled.is_pending() {
            // Wake up periodically to notice a revocation while the upstream
            // is silent.
            while self.check.as_mut().poll(cx).is_ready() {
                if self.cut() {
                    return std::task::Poll::Ready(None);
                }
                let next = tokio::time::Instant::now() + REVOCATION_POLL;
                self.check.as_mut().reset(next);
            }
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.revoked.load(std::sync::atomic::Ordering::SeqCst) || self.inner.is_end_stream()
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }
}

/// How many upstream redirects the daemon follows itself (never relayed to
/// the box); the next one is refused (AC 5).
const GIT_REDIRECT_HOPS: usize = 3;

/// A request on the origin-form git route: allowlist gate, smart-HTTP
/// classification, pushAllow gate, host-side credential injection, redirect
/// following, and the chained leg through `egressUpstream` when configured
/// (direct otherwise). Revocation aborts an in-flight request; the log entry
/// then reports `outcome="canceled"`.
async fn handle_git_request(
    request: Request<Incoming>,
    client: &ProxyClient,
    config: &ProxyConfig,
    session: sessions::SessionRef,
    rest: &str,
    query: Option<&str>,
    log: &mut RequestLog,
) -> Result<Response<ProxyBody>, ProxyError> {
    let refuse = |message: String| {
        Err(ProxyError::new(
            StatusCode::FORBIDDEN,
            "git_host_not_allowed",
            message,
        ))
    };
    // `/git/<host[:port]>/<path>`: authority and path, taken as sent.
    let without_slash = rest.trim_start_matches('/');
    let (authority, repo_path) = match without_slash.find('/') {
        Some(index) => (&without_slash[..index], &without_slash[index..]),
        None => (without_slash, "/"),
    };
    if let Some(sandbox) = log.sandbox.as_mut() {
        sandbox.host = Some(authority.to_owned());
    }
    if authority.is_empty() {
        return refuse("agentdesktop proxy: the git route is /git/<host[:port]>/<path>".to_owned());
    }
    // The reconstructed destination is matched against the configured
    // prefixes again before connecting: the gitconfig rewrite is a
    // convenience, not the gate (AC 5).
    let destination_text = match query {
        Some(query) => format!("https://{authority}{repo_path}?{query}"),
        None => format!("https://{authority}{repo_path}"),
    };
    let Ok(destination) = url::Url::parse(&destination_text) else {
        return refuse(format!(
            "agentdesktop proxy: not a valid git destination: {authority}{repo_path}"
        ));
    };
    let git = config.git.as_ref();
    let allow_entry = git.and_then(|git| {
        git.allow
            .iter()
            .find(|prefix| git_prefix_matches(prefix, &destination))
    });
    let Some(allow_entry) = allow_entry else {
        return refuse(format!(
            "agentdesktop proxy: {authority} is not in daemon.containment.git.allow"
        ));
    };
    // The connection scheme comes from the matched allow entry; startup
    // validation pins configured entries to https.
    let scheme = url::Url::parse(allow_entry)
        .map(|parsed| parsed.scheme().to_owned())
        .unwrap_or_else(|_| "https".to_owned());
    let mut destination = destination;
    let _ = destination.set_scheme(&scheme);
    let Some(endpoint) = classify_git_endpoint(repo_path, query) else {
        return refuse(
            "agentdesktop proxy: only the smart-HTTP git endpoints are served (info/refs?service=…, git-upload-pack, git-receive-pack)"
                .to_owned(),
        );
    };
    let method_ok = if endpoint.is_rpc() {
        request.method() == Method::POST
    } else {
        request.method() == Method::GET
    };
    if !method_ok {
        return refuse(format!(
            "agentdesktop proxy: {} is not how {} is requested",
            request.method(),
            endpoint.service(),
        ));
    }
    if endpoint.is_push() {
        let push_allowed = git.is_some_and(|git| {
            git.push_allow
                .iter()
                .any(|prefix| git_prefix_matches(prefix, &destination))
        });
        if !push_allowed {
            // git expects its own envelope here, not a JSON refusal.
            return Ok(pkt_line_err_response(
                endpoint.service(),
                endpoint.is_rpc(),
                &format!("push to {authority} is not allowed by daemon.containment.git.pushAllow"),
            ));
        }
    }
    // Host-side credential: static entries only in this PR, longest prefix,
    // only for hosts the allowlist admitted (checked above). The box never
    // sees it and the secret is never serialized back.
    let credential = git
        .and_then(|git| git_credential_for(git, &destination))
        .map(|entry| {
            base64::engine::general_purpose::STANDARD
                .encode(format!("{}:{}", entry.username, entry.secret))
        });

    // Buffer the body so redirects and the 503 retry can resend it.
    let (parts, body) = request.into_parts();
    let body = http_body_util::Limited::new(body, MAX_REQUEST_BODY)
        .collect()
        .await
        .map_err(|error| {
            if error
                .downcast_ref::<http_body_util::LengthLimitError>()
                .is_some()
            {
                ProxyError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "body_too_large",
                    format!(
                        "agentdesktop proxy: request body exceeds {} bytes",
                        MAX_REQUEST_BODY
                    ),
                )
            } else {
                ProxyError::new(
                    StatusCode::BAD_REQUEST,
                    "body_invalid",
                    format!("agentdesktop proxy: could not read the request body: {error}"),
                )
            }
        })?
        .to_bytes();
    if let Some(sandbox) = log.sandbox.as_mut() {
        sandbox.bytes_in = body.len();
    }
    let mut base_headers = parts.headers;
    strip_hop_headers(&mut base_headers);
    for header in [
        AUTHORIZATION.as_str(),
        "x-api-key",
        "api-key",
        "x-llm-token",
        PAIRING_HEADER,
    ] {
        base_headers.remove(header);
    }

    let decision = if config.egress_upstream.is_some() {
        "upstream"
    } else {
        "allow"
    };
    let mut current = destination;
    let mut hops = 0;
    let mut retried_503 = false;
    loop {
        // The git leg: direct, or chained as plain smart-HTTP to the central
        // L7 egress endpoint with the path case preserved and Host/SNI = the
        // egress host. The Authorization header carries only the git
        // credential — no gateway-hop auth in v1 (AC 2).
        let target = match &config.egress_upstream {
            Some(upstream) => {
                let path_and_query = match current.query() {
                    Some(query) => format!("{}?{query}", current.path()),
                    None => current.path().to_owned(),
                };
                format!(
                    "{}{path_and_query}",
                    upstream.as_str().trim_end_matches('/')
                )
            }
            None => current.to_string(),
        };
        let uri: Uri = target.parse().map_err(|_| {
            ProxyError::new(
                StatusCode::BAD_GATEWAY,
                "git_upstream_unreachable",
                "agentdesktop proxy: could not form the upstream git URL",
            )
        })?;
        let host_value = HeaderValue::from_str(
            uri.authority()
                .map(|authority| authority.as_str())
                .unwrap_or(""),
        )
        .map_err(|_| {
            ProxyError::new(
                StatusCode::BAD_GATEWAY,
                "git_upstream_unreachable",
                "agentdesktop proxy: invalid upstream git host",
            )
        })?;
        let mut attempt = Request::new(Full::new(body.clone()));
        *attempt.method_mut() = parts.method.clone();
        *attempt.uri_mut() = uri;
        *attempt.headers_mut() = base_headers.clone();
        attempt.headers_mut().insert(HOST, host_value);
        if let Some(credential) = &credential {
            let mut value =
                HeaderValue::from_str(&format!("Basic {credential}")).map_err(|_| {
                    ProxyError::new(
                        StatusCode::BAD_GATEWAY,
                        "git_upstream_unreachable",
                        "agentdesktop proxy: invalid git credential header",
                    )
                })?;
            value.set_sensitive(true);
            attempt.headers_mut().insert(AUTHORIZATION, value);
        }
        // Revocation cancels an in-flight request (AC 1): the watch polls the
        // registry read-only, so waiting does not keep the session alive.
        let revoked = async {
            loop {
                if !config.sessions.live(&session.id, now_unix()) {
                    return;
                }
                tokio::time::sleep(REVOCATION_POLL).await;
            }
        };
        // `biased` with the revoked arm first: the revoked future checks
        // `sessions.live` on its first poll, so a session revoked during body
        // collect or between hops never sends anything upstream.
        let response = tokio::select! {
            biased;
            () = revoked => {
                if let Some(sandbox) = log.sandbox.take() {
                    sandbox
                        .with_status(StatusCode::FORBIDDEN.as_u16(), Some("pairing_invalid"))
                        .emit(0, "canceled");
                }
                return Err(ProxyError::session_invalid());
            }
            sent = tokio::time::timeout(RESPONSE_HEADERS_TIMEOUT, client.request(attempt)) => sent,
        };
        let response = response
            .map_err(|_| {
                ProxyError::new(
                    StatusCode::GATEWAY_TIMEOUT,
                    "git_upstream_unreachable",
                    "agentdesktop proxy: the git upstream did not answer in time",
                )
            })?
            .map_err(|error| {
                ProxyError::new(
                    StatusCode::BAD_GATEWAY,
                    "git_upstream_unreachable",
                    format!("agentdesktop proxy: git upstream unreachable: {error}"),
                )
            })?;
        // A 503 from the egress upstream is retried exactly once (observed
        // once in validation, assumed transient), except a push RPC, which
        // is not replayed (DV-7: a replayed push is not idempotent); the
        // second — or a push's first — surfaces.
        if config.egress_upstream.is_some()
            && response.status() == StatusCode::SERVICE_UNAVAILABLE
            && !retried_503
            && !matches!(endpoint, GitEndpoint::PushRpc)
        {
            retried_503 = true;
            continue;
        }
        // Redirects are never relayed to the box: followed here, at most
        // GIT_REDIRECT_HOPS hops, only while the target keeps the allow
        // entry's scheme and prefix.
        if response.status().is_redirection() {
            let location = response
                .headers()
                .get("location")
                .and_then(|value| value.to_str().ok())
                .and_then(|location| current.join(location).ok());
            let Some(next) = location else {
                return refuse(format!(
                    "agentdesktop proxy: {authority} answered with an unusable redirect"
                ));
            };
            if hops >= GIT_REDIRECT_HOPS {
                return refuse(format!(
                    "agentdesktop proxy: {authority} redirected more than {GIT_REDIRECT_HOPS} times"
                ));
            }
            if !git_redirect_stays_in(allow_entry, &next) {
                return refuse(format!(
                    "agentdesktop proxy: {authority} redirected outside the allowed prefix"
                ));
            }
            hops += 1;
            current = next;
            continue;
        }
        let revoked = Arc::new(std::sync::atomic::AtomicBool::new(false));
        if let Some(sandbox) = log.sandbox.as_mut() {
            sandbox.decision = Some(decision);
            sandbox.revoked = Some(revoked.clone());
        }
        let mut response = response;
        strip_hop_headers(response.headers_mut());
        // Revocation also cuts a response body that is already streaming
        // (a long pack transfer), not just a request waiting for headers.
        let sessions = config.sessions.clone();
        return Ok(response.map(|body| {
            RevocableBody {
                inner: body.boxed(),
                sessions,
                session_id: session.id.clone(),
                check: Box::pin(tokio::time::sleep(REVOCATION_POLL)),
                revoked,
            }
            .boxed()
        }));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests;
