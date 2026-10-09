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

use agentdesktop_core::config::{GitHubTokenSource, LlmGatewayConfig};

use crate::api::{self, AppState};

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
/// A cached controller credential is not used within this margin of its expiry.
const CREDENTIAL_EXPIRY_MARGIN: Duration = Duration::from_secs(30);
/// Longest a controller credential is served from the cache, counted from its
/// fetch. This bounds how long a revoked device keeps using the proxy:
/// revocation is enforced at issuance, the gateway validates tokens statelessly.
const CREDENTIAL_CACHE_TTL: Duration = Duration::from_secs(60);

/// Listener-local cache of controller-issued gateway credentials, one per client
/// id. A controller fetch is a fresh mTLS connection and round trip; agent-mode
/// sessions issue bursts of requests, so a short cache keeps them off the
/// controller. Fetches are single-flight per key: concurrent misses wait for
/// the one fetch in progress and then read its result from the cache. The
/// entries lock is never held across a fetch, so a slow controller delays only
/// the requests waiting for that key, not cache hits on other routes.
/// OIDC credentials never go through here; the secret store is their cache.
#[derive(Default)]
pub(crate) struct CredentialCache {
    entries: std::sync::Mutex<std::collections::HashMap<String, CachedCredential>>,
    flights: std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

struct CachedCredential {
    credential: String,
    /// The enrolled device the credential was issued to. A logout or a
    /// re-enrollment changes it, and the entry is then ignored.
    device_id: String,
    /// Monotonic deadline (wall-clock expiry at fetch time, minus the margin,
    /// capped by CREDENTIAL_CACHE_TTL) and the wall-clock deadline itself. Both
    /// must be in the future: the monotonic clock does not advance across a
    /// suspend, the wall clock can jump; neither alone is trusted.
    valid_until: std::time::Instant,
    valid_until_unix: u64,
    /// The credential's own expiry (the controller JWT `exp`), on both clocks:
    /// a tunnel opened with the credential runs until then, not until the
    /// cache stops reusing it.
    expires: std::time::Instant,
    expires_unix: u64,
}

/// A point in time on both clocks (see `CachedCredential`): a tunnel closes
/// when either says the credential has expired.
#[derive(Clone, Copy, Debug)]
struct Deadline {
    monotonic: std::time::Instant,
    unix: u64,
}

impl Deadline {
    fn from_unix(expires_unix: u64) -> Self {
        Self {
            monotonic: std::time::Instant::now()
                + Duration::from_secs(expires_unix.saturating_sub(now_unix())),
            unix: expires_unix,
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl CredentialCache {
    /// The lock a fetch for the key holds; one per key, kept for the
    /// listener's lifetime (keys are client id, gateway and audience).
    fn flight(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.flights
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(key.to_owned())
            .or_default()
            .clone()
    }

    /// The cached credential for the key, or one from `fetch`, which is cached
    /// at once. Single-flight: a caller that finds a fetch in progress waits
    /// for it and reads its result. Returns the credential, its own expiry, and
    /// whether it came from the cache.
    async fn get_or_fetch<F, Fut, E>(
        &self,
        key: &str,
        device_id: &str,
        fetch: F,
    ) -> Result<(String, Deadline, bool), E>
    where
        F: FnOnce() -> Fut,
        Fut:
            std::future::Future<Output = Result<agentdesktop_core::model::LlmGatewayCredential, E>>,
    {
        let flight = self.flight(key);
        let _flight = flight.lock().await;
        if let Some((credential, deadline)) = self.lookup(key, device_id) {
            return Ok((credential, deadline, true));
        }
        let fetched = fetch().await?;
        self.insert(key, device_id, &fetched);
        let deadline = Deadline::from_unix(fetched.expires_at_unix_seconds);
        Ok((fetched.credential, deadline, false))
    }

    /// A cached credential for the key and device, if one is still valid.
    #[cfg(all(test, target_os = "linux"))]
    fn get(&self, key: &str, device_id: &str) -> Option<String> {
        self.lookup(key, device_id)
            .map(|(credential, _)| credential)
    }

    /// A cached credential together with its own expiry, read in one go so a
    /// tunnel opened with it always gets the deadline of the credential it
    /// was actually opened with.
    fn lookup(&self, key: &str, device_id: &str) -> Option<(String, Deadline)> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        entries
            .get(key)
            .filter(|entry| {
                entry.device_id == device_id
                    && std::time::Instant::now() < entry.valid_until
                    && now_unix() < entry.valid_until_unix
            })
            .map(|entry| {
                (
                    entry.credential.clone(),
                    Deadline {
                        monotonic: entry.expires,
                        unix: entry.expires_unix,
                    },
                )
            })
    }

    /// Remember a freshly fetched credential until the earlier of its expiry
    /// margin and the cache TTL.
    fn insert(
        &self,
        key: &str,
        device_id: &str,
        fetched: &agentdesktop_core::model::LlmGatewayCredential,
    ) {
        let now = now_unix();
        let remaining = Duration::from_secs(fetched.expires_at_unix_seconds.saturating_sub(now));
        let lifetime = remaining
            .saturating_sub(CREDENTIAL_EXPIRY_MARGIN)
            .min(CREDENTIAL_CACHE_TTL);
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if lifetime > Duration::ZERO {
            entries.insert(
                key.to_owned(),
                CachedCredential {
                    credential: fetched.credential.clone(),
                    device_id: device_id.to_owned(),
                    valid_until: std::time::Instant::now() + lifetime,
                    valid_until_unix: now + lifetime.as_secs(),
                    expires: std::time::Instant::now() + remaining,
                    expires_unix: fetched.expires_at_unix_seconds,
                },
            );
        } else {
            tracing::debug!(
                key,
                "controller credential already within its expiry margin; not cached"
            );
            entries.remove(key);
        }
    }

    /// Drop the entry for the key if it still holds the given credential. A
    /// concurrent request that already replaced it with a fresh one is left alone.
    fn invalidate_if(&self, key: &str, credential: &str) {
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if entries
            .get(key)
            .is_some_and(|entry| entry.credential == credential)
        {
            entries.remove(key);
        }
    }
}

/// Header that carries the per-device pairing value on every route.
pub(crate) const PAIRING_HEADER: &str = "x-agentdesktop-pairing";
/// File under the daemon state directory that holds the pairing value.
const PAIRING_FILE: &str = "llm-proxy-pairing";

/// How a route obtains the credential that goes upstream in `x-llm-token`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RouteCredential {
    /// Only the gateway identity is sent; any client credential is dropped.
    Gateway,
    /// The client's own bearer token is moved to `x-llm-token` and the gateway
    /// identity takes `Authorization` (the VS Code Copilot pass-through shape).
    Passthrough,
}

/// Which configured gateway base URL a route forwards to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Upstream {
    /// `llmGateway.url`, shared with the credential-helper programs.
    Url,
    /// `llmGateway.proxyUrl` when set, else `llmGateway.url`.
    ProxyUrl,
}

/// A fixed path prefix the proxy serves for one managed program.
///
/// The prefix is what a reconciler writes into that program's client file, so
/// the client id is decided by the file the daemon owns, never by request
/// headers. The prefix is stripped before forwarding.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProxyRoute {
    pub prefix: &'static str,
    pub client_id: &'static str,
    pub credential: RouteCredential,
    pub upstream: Upstream,
    /// The pairing is the first path segment after the prefix (the header is
    /// ignored): for a client that cannot add a header to its requests.
    pub pairing_in_path: bool,
    /// WebSocket upgrades are tunnelled instead of refused.
    pub tunnels_upgrades: bool,
    /// A credential-less `GET /_ping` is forwarded without `x-llm-token`.
    pub credential_less_ping: bool,
}

/// VS Code Copilot Chat's CAPI pass-through route (the target of
/// `github.copilot.advanced.debug.overrideCapiUrl`): the pairing travels as
/// the first path segment after the prefix instead of in `PAIRING_HEADER`,
/// because VS Code sends these requests itself and has no setting that adds a
/// header to them. Also the only route a WebSocket upgrade is tunnelled on
/// (the `GET /responses` Auto/agent conversation).
pub(crate) const CAPI_ROUTE: &str = "/vscode-copilot-capi";

/// Routes for the Copilot programs. A prefix matches only at a `/` boundary, so
/// `/vscode-copilot-passthrough/...` never matches `/vscode-copilot`.
pub(crate) const ROUTES: &[ProxyRoute] = &[
    ProxyRoute {
        prefix: "/vscode-copilot-passthrough",
        client_id: "vscode-copilot",
        credential: RouteCredential::Passthrough,
        upstream: Upstream::ProxyUrl,
        pairing_in_path: false,
        tunnels_upgrades: false,
        credential_less_ping: false,
    },
    ProxyRoute {
        prefix: CAPI_ROUTE,
        client_id: "vscode-copilot",
        credential: RouteCredential::Passthrough,
        upstream: Upstream::ProxyUrl,
        pairing_in_path: true,
        tunnels_upgrades: true,
        credential_less_ping: true,
    },
    ProxyRoute {
        prefix: "/vscode-copilot",
        client_id: "vscode-copilot",
        credential: RouteCredential::Gateway,
        upstream: Upstream::Url,
        pairing_in_path: false,
        tunnels_upgrades: false,
        credential_less_ping: false,
    },
    ProxyRoute {
        prefix: "/copilot-cli",
        client_id: "copilot-cli",
        credential: RouteCredential::Gateway,
        upstream: Upstream::Url,
        pairing_in_path: false,
        tunnels_upgrades: false,
        credential_less_ping: false,
    },
];

/// Splits `rest` (the path remainder after `CAPI_ROUTE`'s prefix, always
/// starting with `/`) into the first non-empty path segment (the pairing
/// value, still percent-encoded as sent) and the remainder starting with `/`
/// (or `/` itself when nothing follows), so the pairing segment never reaches
/// `match_route`'s caller, the forwarded upstream path, logs, or plan
/// reports. `None` when the first segment is missing or empty (bare prefix,
/// `/`, or `//...`).
///
/// Called from `forward` for `CAPI_ROUTE` only, in place of the `PAIRING_HEADER`
/// check; the segment is compared in constant time and stripped before the
/// request is forwarded.
pub(crate) fn split_path_pairing(rest: &str) -> Option<(&str, &str)> {
    let without_slash = rest.strip_prefix('/')?;
    let (segment, remainder) = match without_slash.find('/') {
        Some(index) => (&without_slash[..index], &without_slash[index..]),
        None => (without_slash, "/"),
    };
    (!segment.is_empty()).then_some((segment, remainder))
}

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
    /// Pairing value required on every route.
    pub pairing: Arc<str>,
}

impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("default_client_id", &self.default_client_id)
            .field("pairing", &"<redacted>")
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
                            let response = match forward(request, &client, &state, &config, &cache, &slot).await {
                                Ok(response) => response,
                                Err(error) => {
                                    tracing::warn!(status = %error.status, code = error.code, message = %error.message, "LLM proxy request failed");
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

/// Only loopback hosts: a page on another origin that resolves a name to
/// 127.0.0.1 (DNS rebinding) sends that name in `Host`. Accepts any loopback
/// IP (the listen validation allows all of 127.0.0.0/8 and ::1), bracketed or
/// bare IPv6, `localhost` in any case with an optional trailing dot, each with
/// or without a port. A request without `Host` (HTTP/1.0) is refused.
fn host_is_loopback(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(HOST).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let host = host.trim();
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((name, port)) if port.is_empty() || port.starts_with(':') => name,
            _ => return false,
        }
    } else if host.matches(':').count() > 1 {
        // Bare IPv6 without brackets cannot carry a port.
        host
    } else {
        host.rsplit_once(':').map_or(host, |(name, _)| name)
    };
    if let Ok(ip) = name.parse::<std::net::IpAddr>() {
        return ip.is_loopback();
    }
    name.trim_end_matches('.').eq_ignore_ascii_case("localhost")
}

/// Refuse `..` segments so a route cannot escape the upstream base it pins.
/// The check runs on a percent-decoded copy (the forwarded path stays as sent),
/// so `%2e%2e` and encoded separators are caught too; a malformed escape is
/// refused outright.
fn path_escapes_base(path: &str) -> bool {
    let Some(decoded) = percent_decode(path) else {
        return true;
    };
    decoded
        .split(|byte| *byte == b'/' || *byte == b'\\')
        .any(|segment| segment == b"..")
}

/// Decodes `%XX` escapes to bytes. Escapes that are not two hex digits are
/// malformed and yield `None`; the bytes are not required to be UTF-8, since
/// the check only looks for separators and dots.
fn percent_decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = bytes.get(i + 1..i + 3)?;
            if !hex.iter().all(u8::is_ascii_hexdigit) {
                return None;
            }
            let value = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            out.push(value);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    Some(out)
}

/// Whether the first path segment looks like one of the managed route names
/// without matching one exactly (wrong case, extra suffix, doubled slash).
/// A gateway sub-path whose first segment starts with a route name (for
/// example `/vscode-copilot-proxy/...`) is refused on the prefix-less route
/// as well; the check is on the literal path, so an encoded near miss is not
/// caught and reaches the gateway on the prefix-less route.
fn resembles_route(path: &str) -> bool {
    let first = path
        .trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    !first.is_empty()
        && ROUTES.iter().any(|route| {
            let name = &route.prefix[1..];
            first == name || first.starts_with(name) || name.starts_with(&first) && first.len() >= 8
        })
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The route for a request path, and the path with the prefix removed.
fn match_route(path: &str) -> Option<(&'static ProxyRoute, &str)> {
    ROUTES.iter().find_map(|route| {
        let rest = path.strip_prefix(route.prefix)?;
        (rest.is_empty() || rest.starts_with('/'))
            .then_some((route, if rest.is_empty() { "/" } else { rest }))
    })
}

fn bearer_or_header(headers: &HeaderMap, header: &str) -> Option<String> {
    headers
        .get(header)
        .or_else(|| headers.get(AUTHORIZATION))
        .and_then(|value| value.to_str().ok())
        .map(|value| strip_bearer(value).trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The token of a `Bearer` credential; authentication schemes are
/// case-insensitive (RFC 9110, section 11.1).
fn strip_bearer(value: &str) -> &str {
    match value.split_once(' ') {
        Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => token,
        _ => value,
    }
}

fn upstream_base(gateway: &LlmGatewayConfig, upstream: Upstream) -> &url::Url {
    match upstream {
        Upstream::Url => &gateway.url,
        Upstream::ProxyUrl => gateway.proxy_url.as_ref().unwrap_or(&gateway.url),
    }
}

async fn forward(
    mut request: Request<Incoming>,
    client: &ProxyClient,
    state: &AppState,
    config: &ProxyConfig,
    cache: &Arc<CredentialCache>,
    tunnel_slot: &PendingTunnel,
) -> Result<Response<ProxyBody>, ProxyError> {
    if !host_is_loopback(request.headers()) {
        return Err(ProxyError::new(
            StatusCode::FORBIDDEN,
            "host_not_allowed",
            "agentdesktop proxy: Host must be a loopback address",
        ));
    }
    // This listener is for local native clients, not browser scripts or tunnels.
    if request.headers().contains_key(ORIGIN) || request.method() == Method::CONNECT {
        return Err(ProxyError::new(
            StatusCode::FORBIDDEN,
            "browser_not_allowed",
            "agentdesktop proxy: browser requests and CONNECT are not supported",
        ));
    }
    if request.method() == Method::OPTIONS {
        return Err(ProxyError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "agentdesktop proxy: OPTIONS is not supported",
        ));
    }
    // Pure string work on the path first; no configuration is read yet.
    let path_and_query = request
        .uri()
        .path_and_query()
        .map_or("/", |path| path.as_str())
        .to_owned();
    let (path, query) = match path_and_query.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path_and_query.as_str(), None),
    };
    // The pairing value is required on every route. It travels in its own
    // header, so Authorization keeps one meaning (the client's own token on the
    // pass-through shapes) and the pairing is never mistaken for a credential.
    // The CAPI route's client (VS Code) cannot add a header, so there the
    // pairing is the first path segment and the header is ignored. Checked
    // before anything that reads configuration, so an unpaired caller learns
    // nothing about this device and costs it no work.
    let pairing_ok =
        |offered: &str| constant_time_eq(offered.as_bytes(), config.pairing.as_bytes());
    // Matched once; the route decides pairing placement, upgrade handling,
    // the ping exemption, client id, credential mode and upstream.
    let matched = match_route(path);
    let matched_route = matched.map(|(route, _)| route);
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
            "agentdesktop proxy: unknown route; the managed prefixes are /copilot-cli, /vscode-copilot, /vscode-copilot-passthrough and /vscode-copilot-capi",
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
                route.client_id.to_owned(),
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
                (
                    format!("{client_id}\u{0}{}\u{0}{audience}", gateway.url),
                    device_id,
                )
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
                        let effective = effective.clone();
                        let client_id = client_id.clone();
                        let cache = cache.clone();
                        let cache_key = cache_key.clone();
                        tokio::spawn(async move {
                            let fetch =
                                || api::gateway_credential(&state, &effective, &client_id, false);
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
                        .map_err(ProxyError::credential)?;
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::{enrollment::EnrollmentState, secret_store::SecretStore};
    use agentdesktop_core::{
        config::parse_daemon,
        model::{DaemonInfo, DaemonScope, Discovery},
    };
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use http_body_util::StreamBody;
    use hyper::body::Frame;
    use sha2::{Digest, Sha256};
    use std::sync::Arc;
    use tokio::sync::{mpsc, oneshot, watch};
    use tokio_stream::wrappers::ReceiverStream;

    // Covers the real socket path, credential replacement/rotation, opaque bytes,
    // header forwarding, and delivery before the upstream response is complete.
    #[test]
    fn bearer_scheme_is_case_insensitive() {
        for value in ["Bearer tok", "bearer tok", "BEARER tok", "tok"] {
            let mut headers = HeaderMap::new();
            headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
            assert_eq!(
                bearer_or_header(&headers, "x-llm-token").as_deref(),
                Some("tok"),
                "{value}"
            );
        }
        let mut headers = HeaderMap::new();
        headers.insert("x-llm-token", HeaderValue::from_static("bearer gho_x"));
        assert_eq!(
            bearer_or_header(&headers, "x-llm-token").as_deref(),
            Some("gho_x")
        );
    }

    #[tokio::test]
    async fn streams_opaque_requests_with_current_credentials() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let (release_tx, release_rx) = oneshot::channel();
            let release = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                hyper::server::conn::http1::Builder::new().serve_connection(
                    TokioIo::new(socket), service_fn(move |request: Request<Incoming>| {
                        let release = release.clone();
                        async move {
                            assert_eq!(request.uri(), "/gateway/v1/messages?value=%2F");
                            assert_eq!(request.headers()[HOST], address.to_string());
                            assert!(!request.headers().contains_key("x-api-key"));
                            assert!(!request.headers().contains_key("x-hop"));
                            assert!(!request.headers().contains_key(PAIRING_HEADER));
                            assert_eq!(request.headers()["x-llm-token"], "ghu_test");
                            assert_eq!(request.headers()["anthropic-version"], "2023-06-01");
                            let gate = release.lock().await.take();
                            assert_eq!(request.headers()[AUTHORIZATION],
                                if gate.is_some() { "Bearer first" } else { "Bearer second" });
                            assert_eq!(request.into_body().collect().await.unwrap().to_bytes(),
                                Bytes::from_static(b"\xffnot-json\x00"));
                            let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(2);
                            tokio::spawn(async move {
                                tx.send(Ok(Frame::data(Bytes::from_static(b"data: first\n\n")))).await.unwrap();
                                if let Some(gate) = gate { gate.await.unwrap(); }
                                let _ = tx.send(Ok(Frame::data(Bytes::from_static(b"data: last\n\n")))).await;
                            });
                            Ok::<_, Infallible>(Response::builder()
                                .header("content-type", "text/event-stream")
                                .header("connection", "x-upstream-hop")
                                .header("x-upstream-hop", "remove")
                                .body(StreamBody::new(ReceiverStream::new(rx))).unwrap())
                        }
                    })
                ).await.unwrap();
            });
            let dir = tempfile::tempdir().unwrap();
            let store = SecretStore::new(dir.path()).unwrap();
            let account = URL_SAFE_NO_PAD.encode(Sha256::digest(b"https://issuer.example/\0proxy-test"));
            let save_token = |token: &str| store.set("dev.agentdesktop.gateway-oidc", &account,
                &serde_json::json!({"accessToken": token, "refreshToken": null,
                    "expiresAtUnixSeconds": 4_000_000_000u64,
                    "tokenEndpoint": "https://issuer.example/token"}).to_string()).unwrap();
            save_token("first");
            store.set("dev.agentdesktop.github-oauth", "github-test", &serde_json::json!({
                "accessToken": "ghu_test", "expiresAt": 4_000_000_000u64,
                "refreshToken": null, "refreshExpiresAt": 0,
            }).to_string()).unwrap();
            let config = parse_daemon(&format!("llmGateway:\n  url: http://{address}/gateway\n  githubOAuth:\n    clientId: github-test\n  authentication:\n    type: oidc\n    issuer: https://issuer.example/\n    clientId: proxy-test\n")).unwrap();
            let (_, discovery) = watch::channel(Arc::new(Discovery { agents: vec![], model_runtimes: vec![] }));
            let state = AppState {
                controller_status: None,
                config,
                daemon_info: DaemonInfo { version: "test".into(), scope: DaemonScope::User,
                    config_path: String::new(), state_directory: String::new(),
                    inventory_interval: Duration::from_secs(60), controller: None, llm_proxy: None },
                discovery, enrollment: EnrollmentState::new(false), state_dir: dir.path().to_owned(),
                oidc_callback_listen: None, telemetry: None, logout: None,
            };
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> = Client::builder(TokioExecutor::new()).build_http();
            let mut release_tx = Some(release_tx);
            for _ in 0..2 {
                let request = Request::builder().method(Method::POST)
                    .uri(format!("http://{proxy_address}/v1/messages?value=%2F"))
                    .header(AUTHORIZATION, "Bearer must-not-forward")
                    .header("x-api-key", "must-not-forward")
                    .header("x-llm-token", "must-not-forward")
                    .header(PAIRING_HEADER, PAIRING)
                    .header(CONNECTION, "x-hop").header("x-hop", "remove")
                    .header("anthropic-version", "2023-06-01")
                    .body(Full::new(Bytes::from_static(b"\xffnot-json\x00"))).unwrap();
                let response = client.request(request).await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(response.headers()["content-type"], "text/event-stream");
                assert!(!response.headers().contains_key("x-upstream-hop"));
                let mut body = response.into_body();
                let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
                if let Some(release) = release_tx.take() {
                    assert_eq!(first, "data: first\n\n");
                    release.send(()).unwrap();
                }
                let rest = body.collect().await.unwrap().to_bytes();
                assert_eq!([first.as_ref(), rest.as_ref()].concat(), b"data: first\n\ndata: last\n\n");
                save_token("second");
            }
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            upstream_task.abort();
        }).await.unwrap();
    }

    // source: request. The client's own credential is what reaches the gateway
    // in x-llm-token, and the daemon contributes only the gateway identity.
    // This is the VS Code Copilot shape: Copilot has already exchanged its
    // GitHub session for a provider token and sends it in Authorization.
    #[tokio::test]
    async fn forwards_the_client_credential_when_source_is_request() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(move |request: Request<Incoming>| async move {
                            // The Bearer prefix is stripped here and re-added by the
                            // gateway, so the token travels bare.
                            assert_eq!(request.headers()["x-llm-token"], "tid=from-client");
                            assert_eq!(request.headers()[AUTHORIZATION], "Bearer identity");
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
                        }),
                    )
                    .await
                    .unwrap();
            });
            let (dir, state) = request_source_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{proxy_address}/chat/completions"))
                .header(PAIRING_HEADER, PAIRING)
                .header(AUTHORIZATION, "Bearer tid=from-client")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // Without a client credential the gateway would swap in an empty
    // Authorization and the provider would fail in a way that looks like a
    // gateway fault, so the proxy refuses instead of forwarding.
    #[tokio::test]
    async fn rejects_a_request_with_no_client_credential() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let (dir, state) = request_source_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{proxy_address}/chat/completions"))
                .header(PAIRING_HEADER, PAIRING)
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            // The default route is paired too: without the header it is refused
            // before any credential work.
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{proxy_address}/chat/completions"))
                .header(AUTHORIZATION, "Bearer tid=from-client")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                error_code(client.request(request).await.unwrap()).await,
                (StatusCode::FORBIDDEN, "pairing_invalid".to_owned())
            );
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    const PAIRING: &str = "pairing-secret-value-0123456789abcdef";

    fn default_config() -> ProxyConfig {
        ProxyConfig {
            default_client_id: "vscode".into(),
            pairing: Arc::from(PAIRING),
        }
    }

    async fn error_code(response: Response<Incoming>) -> (StatusCode, String) {
        let status = response.status();
        assert_eq!(response.headers()[CONTENT_TYPE], "application/json");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        (status, json["error"]["code"].as_str().unwrap().to_owned())
    }

    #[tokio::test]
    async fn refuses_non_loopback_hosts_and_options_with_json_errors() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (dir, state) = request_source_state(upstream.local_addr().unwrap()).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let rebound = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}/copilot-cli/v1/chat/completions"
                ))
                .header(HOST, "attacker.example")
                .header(PAIRING_HEADER, "pairing-secret-value-0123456789abcdef")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                error_code(client.request(rebound).await.unwrap()).await,
                (StatusCode::FORBIDDEN, "host_not_allowed".to_owned())
            );
            let options = Request::builder()
                .method(Method::OPTIONS)
                .uri(format!(
                    "http://{proxy_address}/copilot-cli/v1/chat/completions"
                ))
                .body(Full::new(Bytes::new()))
                .unwrap();
            assert_eq!(
                error_code(client.request(options).await.unwrap()).await,
                (
                    StatusCode::METHOD_NOT_ALLOWED,
                    "method_not_allowed".to_owned()
                )
            );
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    // A prefixed route needs the pairing value, strips its prefix, sends only the
    // gateway identity (no client credential leaks through), and is refused with a
    // JSON error without the pairing.
    #[tokio::test]
    async fn prefixed_gateway_route_requires_pairing_and_strips_the_prefix() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(move |request: Request<Incoming>| async move {
                            assert_eq!(request.uri(), "/v1/chat/completions?stream=true");
                            assert_eq!(request.headers()[AUTHORIZATION], "Bearer identity");
                            assert!(!request.headers().contains_key("x-llm-token"));
                            assert!(!request.headers().contains_key(PAIRING_HEADER));
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
                        }),
                    )
                    .await
                    .unwrap();
            });
            let (dir, state) = request_source_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state.clone(), default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let unpaired = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}/copilot-cli/v1/chat/completions"
                ))
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                error_code(client.request(unpaired).await.unwrap()).await,
                (StatusCode::FORBIDDEN, "pairing_invalid".to_owned())
            );
            // The pairing is only accepted in its own header, never as the bearer.
            let as_bearer = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}/copilot-cli/v1/chat/completions"
                ))
                .header(AUTHORIZATION, format!("Bearer {PAIRING}"))
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                error_code(client.request(as_bearer).await.unwrap()).await,
                (StatusCode::FORBIDDEN, "pairing_invalid".to_owned())
            );
            let wrong = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}/copilot-cli/v1/chat/completions"
                ))
                .header(PAIRING_HEADER, "pairing-secret-value-0123456789abcdeX")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                error_code(client.request(wrong).await.unwrap()).await,
                (StatusCode::FORBIDDEN, "pairing_invalid".to_owned())
            );
            let paired = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}/copilot-cli/v1/chat/completions?stream=true"
                ))
                .header(PAIRING_HEADER, PAIRING)
                .header(AUTHORIZATION, "Bearer a-real-key-by-mistake")
                .header("x-llm-token", "must-not-forward")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            let response = client.request(paired).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            proxy.abort();
            let _ = proxy.await;
            // A '..' segment cannot escape the upstream base the route pins.
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{proxy_address}/copilot-cli/../admin"))
                .header(PAIRING_HEADER, PAIRING)
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                error_code(client.request(request).await.unwrap()).await,
                (StatusCode::BAD_REQUEST, "path_invalid".to_owned())
            );
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // The pass-through route moves the client's token to x-llm-token, keeps the
    // gateway identity in Authorization, forwards to proxyUrl, and refuses a
    // request without a client token. The pairing must come in its own header
    // here, because Authorization carries the client token.
    #[tokio::test]
    async fn passthrough_route_moves_the_client_token_and_uses_proxy_url() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(move |request: Request<Incoming>| async move {
                            assert_eq!(request.uri(), "/copilot-proxy/chat/completions");
                            assert_eq!(request.headers()["x-llm-token"], "tid=from-client");
                            assert_eq!(request.headers()[AUTHORIZATION], "Bearer identity");
                            assert!(!request.headers().contains_key(PAIRING_HEADER));
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
                        }),
                    )
                    .await
                    .unwrap();
            });
            let (dir, mut state) = request_source_state(address).await;
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            // Without proxyUrl the pass-through route is unavailable rather than
            // sending x-llm-token to the plain LLM route.
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state.clone(), default_config()));
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}/vscode-copilot-passthrough/chat/completions"
                ))
                .header(PAIRING_HEADER, PAIRING)
                .header(AUTHORIZATION, "Bearer tid=from-client")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                error_code(client.request(request).await.unwrap()).await,
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "agentdesktop_unavailable".to_owned()
                )
            );
            proxy.abort();
            let _ = proxy.await;
            state.config.llm_gateway.as_mut().unwrap().proxy_url =
                Some(format!("http://{address}/copilot-proxy").parse().unwrap());
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let no_token = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}/vscode-copilot-passthrough/chat/completions"
                ))
                .header(PAIRING_HEADER, "pairing-secret-value-0123456789abcdef")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                error_code(client.request(no_token).await.unwrap()).await,
                (
                    StatusCode::UNAUTHORIZED,
                    "client_credential_missing".to_owned()
                )
            );
            // The pairing in Authorization is not a client token: refused, never forwarded.
            let pairing_as_token = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}/vscode-copilot-passthrough/chat/completions"
                ))
                .header(PAIRING_HEADER, PAIRING)
                .header(AUTHORIZATION, format!("Bearer {PAIRING}"))
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                error_code(client.request(pairing_as_token).await.unwrap()).await,
                (
                    StatusCode::UNAUTHORIZED,
                    "client_credential_missing".to_owned()
                )
            );
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}/vscode-copilot-passthrough/chat/completions"
                ))
                .header(PAIRING_HEADER, "pairing-secret-value-0123456789abcdef")
                .header(AUTHORIZATION, "Bearer tid=from-client")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[test]
    fn route_matching_prefers_the_longer_prefix_and_strips_it() {
        let (route, rest) = match_route("/vscode-copilot-passthrough/chat/completions").unwrap();
        assert_eq!(route.client_id, "vscode-copilot");
        assert_eq!(route.credential, RouteCredential::Passthrough);
        assert_eq!(rest, "/chat/completions");
        let (route, rest) = match_route("/vscode-copilot/v1/chat/completions").unwrap();
        assert_eq!(route.credential, RouteCredential::Gateway);
        assert_eq!(rest, "/v1/chat/completions");
        assert_eq!(match_route("/copilot-cli").unwrap().1, "/");
        assert!(match_route("/copilot-client/v1").is_none());
        assert!(match_route("/v1/chat/completions").is_none());
    }

    #[test]
    fn host_check_accepts_loopback_forms_only() {
        let mut headers = HeaderMap::new();
        let accepted = [
            "127.0.0.1:18095",
            "localhost:18095",
            "[::1]:18095",
            "localhost",
            "127.0.0.1",
            "127.0.0.2:1",
            "Localhost:18095",
            "LOCALHOST",
            "localhost.:18095",
            "[::1]",
            "::1",
        ];
        for ok in accepted {
            headers.insert(HOST, HeaderValue::from_static(ok));
            assert!(host_is_loopback(&headers), "{ok}");
        }
        let refused = [
            "attacker.example",
            "127.0.0.1.attacker.example:18095",
            "10.0.0.5:18095",
            "",
            "[::1",
            "[fe80::1]:18095",
            "localhost.attacker.example",
            "127.0.0.1:18095:1",
        ];
        for bad in refused {
            headers.insert(HOST, HeaderValue::from_static(bad));
            assert!(!host_is_loopback(&headers), "{bad}");
        }
        headers.remove(HOST);
        assert!(!host_is_loopback(&headers));
    }

    #[test]
    fn pairing_is_created_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let first = load_or_create_pairing(dir.path()).unwrap();
        let second = load_or_create_pairing(dir.path()).unwrap();
        assert_eq!(first, second);
        assert!(first.len() >= 43);
        let metadata = std::fs::metadata(dir.path().join(PAIRING_FILE)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
        let _ = metadata;
    }

    #[test]
    fn pairing_file_is_regenerated_only_when_unusable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(PAIRING_FILE);
        // Too short: replaced.
        std::fs::write(&path, b"short").unwrap();
        let regenerated = load_or_create_pairing(dir.path()).unwrap();
        assert!(regenerated.len() >= 43);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), *regenerated);
        // Not text: replaced.
        std::fs::write(&path, [0xff, 0xfe, 0x00, 0x01]).unwrap();
        let again = load_or_create_pairing(dir.path()).unwrap();
        assert_ne!(again, regenerated);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), *again);
        // Any other read error is an error, and the value on disk is untouched.
        let other = tempfile::tempdir().unwrap();
        std::fs::create_dir(other.path().join(PAIRING_FILE)).unwrap();
        let error = load_or_create_pairing(other.path()).unwrap_err();
        assert!(format!("{error:#}").contains("read "), "{error:#}");
        assert!(other.path().join(PAIRING_FILE).is_dir());
        // An existing value that cannot be read (permissions) is kept as is.
        #[cfg(unix)]
        if !nix_is_root() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
            let error = load_or_create_pairing(dir.path()).unwrap_err();
            assert!(format!("{error:#}").contains("read "), "{error:#}");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert_eq!(*load_or_create_pairing(dir.path()).unwrap(), *again);
        }
        // A state directory that cannot be created is an error, not a pairing.
        let blocked = tempfile::tempdir().unwrap();
        let file = blocked.path().join("state");
        std::fs::write(&file, b"x").unwrap();
        assert!(load_or_create_pairing(&file.join("nested")).is_err());
    }

    #[cfg(unix)]
    fn nix_is_root() -> bool {
        std::fs::metadata("/proc/self")
            .map(|m| {
                use std::os::unix::fs::MetadataExt;
                m.uid() == 0
            })
            .unwrap_or(false)
    }

    #[test]
    fn route_table_maps_each_prefix_to_its_identity_mode_and_upstream() {
        let expected = [
            (
                "/copilot-cli/v1/x",
                "copilot-cli",
                RouteCredential::Gateway,
                Upstream::Url,
            ),
            (
                "/vscode-copilot/v1/x",
                "vscode-copilot",
                RouteCredential::Gateway,
                Upstream::Url,
            ),
            (
                "/vscode-copilot-passthrough/chat/completions",
                "vscode-copilot",
                RouteCredential::Passthrough,
                Upstream::ProxyUrl,
            ),
            (
                "/vscode-copilot-capi/PAIRING/responses",
                "vscode-copilot",
                RouteCredential::Passthrough,
                Upstream::ProxyUrl,
            ),
        ];
        for (path, client_id, credential, upstream) in expected {
            let (route, _) = match_route(path).unwrap_or_else(|| panic!("{path}"));
            assert_eq!(route.client_id, client_id, "{path}");
            assert_eq!(route.credential, credential, "{path}");
            assert_eq!(route.upstream, upstream, "{path}");
        }
        assert_eq!(ROUTES.len(), 4);
        assert_eq!(CAPI_ROUTE, "/vscode-copilot-capi");
    }

    #[test]
    fn path_guard_catches_encoded_traversal_and_near_miss_routes() {
        for escaping in [
            "/copilot-cli/../admin",
            "/copilot-cli/%2e%2e/admin",
            "/copilot-cli/%2E%2E/admin",
            "/copilot-cli/%2e%2e%2fadmin",
            "/copilot-cli/..%5cadmin",
            "/copilot-cli/%zz",
            "/copilot-cli/%+f",
            "/copilot-cli/%2",
        ] {
            assert!(path_escapes_base(escaping), "{escaping}");
        }
        for fine in [
            "/copilot-cli/v1/chat/completions",
            "/v1/messages",
            "/copilot-cli/a.b/c%20d",
            "/copilot-cli/caf%e9",
            "/copilot-cli/%2e%2ex",
        ] {
            assert!(!path_escapes_base(fine), "{fine}");
        }
        for near in [
            "/copilot-cli2/v1",
            "/Copilot-cli/v1",
            "//copilot-cli/v1",
            "/vscode-copilotx/v1",
        ] {
            assert!(
                match_route(near).is_none() && resembles_route(near),
                "{near}"
            );
        }
        for other in ["/v1/chat/completions", "/chat/completions", "/models"] {
            assert!(!resembles_route(other), "{other}");
        }
    }

    // The pairing is checked before any configuration is read: an unpaired
    // caller gets 403 even when the applied configuration is unreadable, and a
    // paired caller gets a generic message without file detail.
    #[tokio::test]
    async fn pairing_is_checked_before_configuration_is_read() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(
                dir.path().join("remote-config.yaml"),
                "llmGateway: [not: an: object",
            )
            .unwrap();
            let config = parse_daemon("controller:\n  address: https://127.0.0.1:1\n").unwrap();
            let (_, discovery) = watch::channel(Arc::new(Discovery {
                agents: vec![],
                model_runtimes: vec![],
            }));
            let state = AppState {
                controller_status: None,
                config,
                daemon_info: DaemonInfo {
                    version: "test".into(),
                    scope: DaemonScope::User,
                    config_path: String::new(),
                    state_directory: String::new(),
                    inventory_interval: Duration::from_secs(60),
                    controller: None,
                    llm_proxy: None,
                },
                discovery,
                enrollment: EnrollmentState::new(true),
                state_dir: dir.path().to_owned(),
                oidc_callback_listen: None,
                telemetry: None,
                logout: None,
            };
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let unpaired = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}/copilot-cli/v1/chat/completions"
                ))
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                error_code(client.request(unpaired).await.unwrap()).await,
                (StatusCode::FORBIDDEN, "pairing_invalid".to_owned())
            );
            // The path checks are pure string checks and run before the
            // configuration is read: a paired caller with a bad path gets the
            // path error, not the configuration error.
            for (bad_path, code) in [
                ("/copilot-cli/../admin", "path_invalid"),
                ("/Copilot-cli/v1/chat/completions", "route_unknown"),
            ] {
                let request = Request::builder()
                    .method(Method::POST)
                    .uri(format!("http://{proxy_address}{bad_path}"))
                    .header(PAIRING_HEADER, PAIRING)
                    .body(Full::new(Bytes::from_static(b"{}")))
                    .unwrap();
                let (_, got) = error_code(client.request(request).await.unwrap()).await;
                assert_eq!(got, code, "{bad_path}");
            }
            let paired = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}/copilot-cli/v1/chat/completions"
                ))
                .header(PAIRING_HEADER, PAIRING)
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            let response = client.request(paired).await.unwrap();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let message = json["error"]["message"].as_str().unwrap();
            assert!(
                !message.contains("remote-config")
                    && !message.contains(dir.path().to_str().unwrap()),
                "{message}"
            );
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    // The retry applies only to a credential served from the cache. OIDC
    // credentials are never cached, so a 401 from the gateway is returned as is
    // after a single attempt, with the buffered body delivered once. The cached
    // path is covered by `cached_credential_is_used_and_dropped_on_401` below.
    #[tokio::test]
    async fn uncached_credentials_are_not_retried_on_401() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let seen = attempts.clone();
            let upstream_task = tokio::spawn(async move {
                loop {
                    let (socket, _) = upstream.accept().await.unwrap();
                    let seen = seen.clone();
                    tokio::spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                TokioIo::new(socket),
                                service_fn(move |request: Request<Incoming>| {
                                    let seen = seen.clone();
                                    async move {
                                        let n =
                                            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                        assert_eq!(
                                            request.headers()[AUTHORIZATION],
                                            "Bearer identity"
                                        );
                                        let body =
                                            request.into_body().collect().await.unwrap().to_bytes();
                                        assert_eq!(body, "{\"n\":1}");
                                        // One attempt per client request: the first
                                        // is answered 401, the second 200.
                                        let status = if n == 1 {
                                            StatusCode::OK
                                        } else {
                                            StatusCode::UNAUTHORIZED
                                        };
                                        Ok::<_, Infallible>(
                                            Response::builder()
                                                .status(status)
                                                .body(Full::new(Bytes::from_static(b"upstream")))
                                                .unwrap(),
                                        )
                                    }
                                }),
                            )
                            .await;
                    });
                }
            });
            let (dir, state) = request_source_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let request = || {
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("http://{proxy_address}/chat/completions"))
                    .header(PAIRING_HEADER, PAIRING)
                    .header(AUTHORIZATION, "Bearer tid=from-client")
                    .body(Full::new(Bytes::from_static(b"{\"n\":1}")))
                    .unwrap()
            };
            let response = client.request(request()).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
            let response = client.request(request()).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 2);
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // A pre-seeded cache stands in for a controller: the proxy sends the cached
    // credential; when the gateway accepts it the entry stays; when the gateway
    // rejects it the entry is dropped and the request is retried, which here
    // needs a controller that does not exist, so the client gets
    // `agentdesktop_credential` and the gateway saw the body exactly once.
    #[tokio::test]
    async fn cached_credential_is_used_and_dropped_on_401() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            type Seen = Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>;
            let seen: Seen = Arc::default();
            let record = seen.clone();
            let upstream_task = tokio::spawn(async move {
                loop {
                    let (socket, _) = upstream.accept().await.unwrap();
                    let record = record.clone();
                    tokio::spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                TokioIo::new(socket),
                                service_fn(move |request: Request<Incoming>| {
                                    let record = record.clone();
                                    async move {
                                        let authorization = request
                                            .headers()
                                            .get(AUTHORIZATION)
                                            .and_then(|v| v.to_str().ok())
                                            .unwrap_or("")
                                            .to_owned();
                                        let body = request
                                            .into_body()
                                            .collect()
                                            .await
                                            .unwrap()
                                            .to_bytes()
                                            .to_vec();
                                        let status = if authorization == "Bearer good" {
                                            StatusCode::OK
                                        } else {
                                            StatusCode::UNAUTHORIZED
                                        };
                                        record.lock().unwrap().push((authorization, body));
                                        Ok::<_, Infallible>(
                                            Response::builder()
                                                .status(status)
                                                .body(Full::new(Bytes::from_static(b"{}")))
                                                .unwrap(),
                                        )
                                    }
                                }),
                            )
                            .await;
                    });
                }
            });
            let (dir, state) = controller_jwt_state(address, false).await;
            let cache = Arc::new(CredentialCache::default());
            let key = format!("copilot-cli\u{0}http://{address}/\u{0}agentgateway");
            let issued = |value: &str| agentdesktop_core::model::LlmGatewayCredential {
                credential: value.to_owned(),
                expires_at_unix_seconds: now_unix() + 600,
            };
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve_with_cache(
                listener,
                state,
                default_config(),
                cache.clone(),
            ));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let request = |body: &'static [u8]| {
                Request::builder()
                    .method(Method::POST)
                    .uri(format!(
                        "http://{proxy_address}/copilot-cli/v1/chat/completions"
                    ))
                    .header(PAIRING_HEADER, PAIRING)
                    .body(Full::new(Bytes::from_static(body)))
                    .unwrap()
            };
            // Accepted: served from the cache, entry kept.
            cache.insert(&key, "device-test", &issued("good"));
            let response = client.request(request(b"{\"n\":1}")).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(cache.get(&key, "device-test").as_deref(), Some("good"));
            // Rejected: entry dropped, one retry that needs a controller.
            cache.insert(&key, "device-test", &issued("stale"));
            let (status, code) =
                error_code(client.request(request(b"{\"n\":2}")).await.unwrap()).await;
            assert_eq!(code, "agentdesktop_credential", "{status}");
            assert_eq!(cache.get(&key, "device-test"), None);
            let seen = seen.lock().unwrap().clone();
            assert_eq!(
                seen,
                vec![
                    ("Bearer good".to_owned(), b"{\"n\":1}".to_vec()),
                    ("Bearer stale".to_owned(), b"{\"n\":2}".to_vec()),
                ],
                "each body reaches the gateway exactly once per attempt"
            );
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // With a client token forwarded (prefix-less route, githubOAuth source
    // request) a 401 is returned as is: no retry, and the cached gateway
    // credential is kept.
    #[tokio::test]
    async fn client_token_401_is_not_retried_and_keeps_the_cached_credential() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let seen = attempts.clone();
            let upstream_task = tokio::spawn(async move {
                loop {
                    let (socket, _) = upstream.accept().await.unwrap();
                    let seen = seen.clone();
                    tokio::spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                TokioIo::new(socket),
                                service_fn(move |request: Request<Incoming>| {
                                    let seen = seen.clone();
                                    async move {
                                        seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                        assert_eq!(
                                            request.headers()[AUTHORIZATION],
                                            "Bearer cached"
                                        );
                                        assert_eq!(request.headers()["x-llm-token"], "tid=bad");
                                        Ok::<_, Infallible>(
                                            Response::builder()
                                                .status(StatusCode::UNAUTHORIZED)
                                                .body(Full::new(Bytes::from_static(b"{}")))
                                                .unwrap(),
                                        )
                                    }
                                }),
                            )
                            .await;
                    });
                }
            });
            let (dir, state) = controller_jwt_state(address, true).await;
            let cache = Arc::new(CredentialCache::default());
            let key = format!("vscode\u{0}http://{address}/\u{0}agentgateway");
            cache.insert(
                &key,
                "device-test",
                &agentdesktop_core::model::LlmGatewayCredential {
                    credential: "cached".to_owned(),
                    expires_at_unix_seconds: now_unix() + 600,
                },
            );
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve_with_cache(
                listener,
                state,
                default_config(),
                cache.clone(),
            ));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{proxy_address}/chat/completions"))
                .header(PAIRING_HEADER, PAIRING)
                .header(AUTHORIZATION, "Bearer tid=bad")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(cache.get(&key, "device-test").as_deref(), Some("cached"));
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // Controller-JWT state with an identity.json but no controller configured:
    // the device id is readable for the cache key, a fetch fails at once
    // ("requires a controller"), so no timeout is involved.
    async fn controller_jwt_state(
        address: std::net::SocketAddr,
        client_token: bool,
    ) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("identity.json"),
            serde_json::json!({
                "deviceId": "device-test",
                "clientCertificatePem": "",
                "clientCertificateExpiresAtUnixSeconds": 4_000_000_000u64,
                "oauthTokenEndpoint": "https://controller.example/token",
                "oauthClientId": "device",
            })
            .to_string(),
        )
        .unwrap();
        let github = if client_token {
            "  githubOAuth:\n    source: request\n"
        } else {
            ""
        };
        let config = parse_daemon(&format!(
            "llmGateway:\n  url: http://{address}\n{github}  authentication:\n    type: controllerJwt\n    audience: agentgateway\n    allowedClientIds: [copilot-cli, vscode]\n"
        )).unwrap();
        let (_, discovery) = watch::channel(Arc::new(Discovery {
            agents: vec![],
            model_runtimes: vec![],
        }));
        let state = AppState {
            controller_status: None,
            config,
            daemon_info: DaemonInfo {
                version: "test".into(),
                scope: DaemonScope::User,
                config_path: String::new(),
                state_directory: String::new(),
                inventory_interval: Duration::from_secs(60),
                controller: None,
                llm_proxy: None,
            },
            discovery,
            enrollment: EnrollmentState::new(false),
            state_dir: dir.path().to_owned(),
            oidc_callback_listen: None,
            telemetry: None,
            logout: None,
        };
        (dir, state)
    }

    #[tokio::test]
    async fn refuses_a_body_over_the_cap_with_a_json_error() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let (dir, state) = request_source_state(upstream.local_addr().unwrap()).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{proxy_address}/chat/completions"))
                .header(PAIRING_HEADER, PAIRING)
                .header(AUTHORIZATION, "Bearer tid=from-client")
                .body(Full::new(Bytes::from(vec![b'x'; MAX_REQUEST_BODY + 1])))
                .unwrap();
            assert_eq!(
                error_code(client.request(request).await.unwrap()).await,
                (StatusCode::PAYLOAD_TOO_LARGE, "body_too_large".to_owned())
            );
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn concurrent_misses_share_one_fetch_per_key() {
        let far = now_unix() + 3600;
        let cache = Arc::new(CredentialCache::default());
        let fetches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tasks = tokio::task::JoinSet::new();
        for key in ["a", "a", "a", "a", "a", "b", "b"] {
            let cache = cache.clone();
            let fetches = fetches.clone();
            tasks.spawn(async move {
                cache
                    .get_or_fetch(key, "device-1", || async move {
                        let n = fetches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok::<_, ()>(agentdesktop_core::model::LlmGatewayCredential {
                            credential: format!("{key}{n}"),
                            expires_at_unix_seconds: far,
                        })
                    })
                    .await
                    .map(|(credential, _, _)| (key, credential))
            });
        }
        let mut results = Vec::new();
        while let Some(result) = tasks.join_next().await {
            results.push(result.unwrap().unwrap());
        }
        // One fetch per key; every caller of a key got that fetch's credential.
        assert_eq!(fetches.load(std::sync::atomic::Ordering::SeqCst), 2);
        for key in ["a", "b"] {
            let mut credentials: Vec<_> = results
                .iter()
                .filter(|(k, _)| *k == key)
                .map(|(_, c)| c)
                .collect();
            credentials.dedup();
            assert_eq!(credentials.len(), 1, "{key}: {results:?}");
        }
        // Cached at fetch time, before any gateway answer.
        assert!(cache.get("a", "device-1").is_some());
    }

    #[test]
    fn credential_cache_deadline_is_the_credential_expiry_not_the_reuse_window() {
        let cache = CredentialCache::default();
        let expires = now_unix() + 3600;
        cache.insert(
            "k",
            "device-1",
            &agentdesktop_core::model::LlmGatewayCredential {
                credential: "jwt".to_owned(),
                expires_at_unix_seconds: expires,
            },
        );
        let (credential, deadline) = cache.lookup("k", "device-1").expect("cached");
        assert_eq!(credential, "jwt");
        assert_eq!(deadline.unix, expires);
        let remaining = deadline.monotonic - std::time::Instant::now();
        assert!(
            remaining > CREDENTIAL_CACHE_TTL && remaining <= Duration::from_secs(3600),
            "tunnel deadline must follow the credential's expiry, got {remaining:?}"
        );
        // The reuse window is still the short one.
        let entry = cache.entries.lock().unwrap();
        assert!(entry["k"].valid_until <= std::time::Instant::now() + CREDENTIAL_CACHE_TTL);
    }

    #[test]
    fn credential_cache_honours_device_ttl_and_invalidation() {
        let cache = CredentialCache::default();
        let far = now_unix() + 600;
        let issued = |value: &str, expires: u64| agentdesktop_core::model::LlmGatewayCredential {
            credential: value.to_owned(),
            expires_at_unix_seconds: expires,
        };
        cache.insert("k", "device-1", &issued("one", far));
        assert_eq!(cache.get("k", "device-1").as_deref(), Some("one"));
        assert_eq!(
            cache.get("k", "device-2"),
            None,
            "another device never sees it"
        );
        assert_eq!(cache.get("other", "device-1"), None);
        cache.invalidate_if("k", "someone-else");
        assert_eq!(
            cache.get("k", "device-1").as_deref(),
            Some("one"),
            "a different credential leaves the entry alone"
        );
        cache.invalidate_if("k", "one");
        assert_eq!(cache.get("k", "device-1"), None);
        // Already within the expiry margin: not cached at all.
        cache.insert("m", "device-1", &issued("m1", now_unix() + 10));
        assert_eq!(cache.get("m", "device-1"), None);
        // Lifetime is capped by the TTL, not the token's own expiry.
        cache.insert("t", "device-1", &issued("t1", far));
        let entries = cache.entries.lock().unwrap();
        let entry = &entries["t"];
        assert!(entry.valid_until <= std::time::Instant::now() + CREDENTIAL_CACHE_TTL);
        assert!(entry.valid_until_unix <= now_unix() + CREDENTIAL_CACHE_TTL.as_secs());
    }

    async fn request_source_state(address: std::net::SocketAddr) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let store = SecretStore::new(dir.path()).unwrap();
        let account =
            URL_SAFE_NO_PAD.encode(Sha256::digest(b"https://issuer.example/\0proxy-test"));
        store
            .set(
                "dev.agentdesktop.gateway-oidc",
                &account,
                &serde_json::json!({"accessToken": "identity", "refreshToken": null,
                "expiresAtUnixSeconds": 4_000_000_000u64,
                "tokenEndpoint": "https://issuer.example/token"})
                .to_string(),
            )
            .unwrap();
        let config = parse_daemon(&format!(
            "llmGateway:\n  url: http://{address}\n  githubOAuth:\n    source: request\n  authentication:\n    type: oidc\n    issuer: https://issuer.example/\n    clientId: proxy-test\n"
        )).unwrap();
        let (_, discovery) = watch::channel(Arc::new(Discovery {
            agents: vec![],
            model_runtimes: vec![],
        }));
        let state = AppState {
            controller_status: None,
            config,
            daemon_info: DaemonInfo {
                version: "test".into(),
                scope: DaemonScope::User,
                config_path: String::new(),
                state_directory: String::new(),
                inventory_interval: Duration::from_secs(60),
                controller: None,
                llm_proxy: None,
            },
            discovery,
            enrollment: EnrollmentState::new(false),
            state_dir: dir.path().to_owned(),
            oidc_callback_listen: None,
            telemetry: None,
            logout: None,
        };
        (dir, state)
    }

    // --- vscode-copilot-capi: path pairing, /_ping and the WebSocket tunnel.

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    /// `request_source_state` (OIDC identity "identity", `githubOAuth: source:
    /// request`) with `llmGateway.proxyUrl` pointed at `upstream`, matching
    /// the CAPI route's Passthrough/ProxyUrl shape used by most tests below.
    async fn capi_state(upstream: std::net::SocketAddr) -> (tempfile::TempDir, AppState) {
        let (dir, mut state) = request_source_state(upstream).await;
        state.config.llm_gateway.as_mut().unwrap().proxy_url =
            Some(format!("http://{upstream}").parse().unwrap());
        (dir, state)
    }

    /// Controller-JWT state (cacheable credential, unlike OIDC) with
    /// `llmGateway.proxyUrl` pointed at `upstream`, for the credential-expiry
    /// tunnel test, which needs a credential it can seed into the cache with a
    /// controlled deadline.
    async fn capi_controller_jwt_state(
        upstream: std::net::SocketAddr,
    ) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("identity.json"),
            serde_json::json!({
                "deviceId": "device-test",
                "clientCertificatePem": "",
                "clientCertificateExpiresAtUnixSeconds": 4_000_000_000u64,
                "oauthTokenEndpoint": "https://controller.example/token",
                "oauthClientId": "device",
            })
            .to_string(),
        )
        .unwrap();
        let config = parse_daemon(&format!(
            "llmGateway:\n  url: http://{upstream}\n  proxyUrl: http://{upstream}\n  authentication:\n    type: controllerJwt\n    audience: agentgateway\n    allowedClientIds: [vscode-copilot]\n"
        ))
        .unwrap();
        let (_, discovery) = watch::channel(Arc::new(Discovery {
            agents: vec![],
            model_runtimes: vec![],
        }));
        let state = AppState {
            controller_status: None,
            config,
            daemon_info: DaemonInfo {
                version: "test".into(),
                scope: DaemonScope::User,
                config_path: String::new(),
                state_directory: String::new(),
                inventory_interval: Duration::from_secs(60),
                controller: None,
                llm_proxy: None,
            },
            discovery,
            enrollment: EnrollmentState::new(false),
            state_dir: dir.path().to_owned(),
            oidc_callback_listen: None,
            telemetry: None,
            logout: None,
        };
        (dir, state)
    }

    /// Whether no connection reached `listener` within a short window: proof
    /// that a refused request was never forwarded upstream.
    async fn no_connection_arrives(listener: &TcpListener) -> bool {
        tokio::time::timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_err()
    }

    /// Reads a raw HTTP/1.1 request head (request line + headers, byte at a
    /// time up to the blank line) from a hand-rolled test upstream: good
    /// enough for a WebSocket upgrade request, which has no body. Header
    /// names are folded to lowercase for case-insensitive lookup.
    async fn read_http_head(
        stream: &mut TcpStream,
    ) -> (String, String, std::collections::HashMap<String, String>) {
        let mut buffer = Vec::new();
        let mut byte = [0u8; 1];
        while !buffer.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.unwrap();
            buffer.push(byte[0]);
        }
        let text = String::from_utf8(buffer).unwrap();
        let mut lines = text.split("\r\n");
        let mut parts = lines.next().unwrap().split_whitespace();
        let method = parts.next().unwrap().to_owned();
        let path = parts.next().unwrap().to_owned();
        let mut headers = std::collections::HashMap::new();
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
            }
        }
        (method, path, headers)
    }

    /// RFC 6455's fixed GUID: concatenated with the client's
    /// `Sec-WebSocket-Key` and SHA-1'd (then base64'd) to produce
    /// `Sec-WebSocket-Accept`.
    const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

    fn ws_accept(key: &str) -> String {
        use base64::engine::general_purpose::STANDARD;
        use sha1::{Digest, Sha1};
        STANDARD.encode(Sha1::digest(format!("{key}{WS_GUID}").as_bytes()))
    }

    /// Encodes a single, unfragmented text frame (opcode `0x1`). Client
    /// frames must be masked (`mask: true`, a fixed key: the test payloads
    /// are not attacker-controlled), server frames must not be.
    fn ws_frame(payload: &[u8], mask: bool) -> Vec<u8> {
        let mut frame = vec![0x81u8];
        let mask_bit = if mask { 0x80 } else { 0x00 };
        let len = payload.len();
        if len < 126 {
            frame.push(mask_bit | len as u8);
        } else {
            frame.push(mask_bit | 126);
            frame.extend_from_slice(&(len as u16).to_be_bytes());
        }
        if mask {
            let key = [0x11u8, 0x22, 0x33, 0x44];
            frame.extend_from_slice(&key);
            frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
        } else {
            frame.extend_from_slice(payload);
        }
        frame
    }

    /// Reads exactly one, unfragmented, non-huge frame and returns its opcode
    /// and unmasked payload.
    async fn ws_read_frame(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut header = [0u8; 2];
        stream.read_exact(&mut header).await.unwrap();
        let opcode = header[0] & 0x0f;
        let masked = header[1] & 0x80 != 0;
        let mut len = (header[1] & 0x7f) as usize;
        if len == 126 {
            let mut extended = [0u8; 2];
            stream.read_exact(&mut extended).await.unwrap();
            len = u16::from_be_bytes(extended) as usize;
        }
        let mask_key = if masked {
            let mut key = [0u8; 4];
            stream.read_exact(&mut key).await.unwrap();
            Some(key)
        } else {
            None
        };
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).await.unwrap();
        if let Some(key) = mask_key {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= key[index % 4];
            }
        }
        (opcode, payload)
    }

    /// A fixed, valid `Sec-WebSocket-Key` (the RFC 6455 example key): its
    /// `Sec-WebSocket-Accept` is computed by `ws_accept`, never hand-checked
    /// against a literal.
    const SEC_WEBSOCKET_KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

    fn upgrade_request(uri: String) -> Request<Full<Bytes>> {
        Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(CONNECTION, "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", SEC_WEBSOCKET_KEY)
            .body(Full::new(Bytes::new()))
            .unwrap()
    }

    #[tokio::test]
    async fn capi_route_accepts_and_strips_path_pairing_never_forwarding_it() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(move |request: Request<Incoming>| async move {
                            // The pairing segment must never reach the upstream path.
                            assert_eq!(request.uri(), "/chat/completions");
                            assert!(!request.headers().contains_key(PAIRING_HEADER));
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
                        }),
                    )
                    .await
                    .unwrap();
            });
            let (dir, state) = capi_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}{CAPI_ROUTE}/{PAIRING}/chat/completions"
                ))
                .header(AUTHORIZATION, "Bearer tid=from-client")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "path pairing must be accepted on {CAPI_ROUTE}"
            );
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn capi_route_rejects_missing_empty_or_wrong_pairing_segment_even_with_a_valid_header() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            // Dropped rather than accepted from: today none of these four
            // paths are rejected before forwarding (path-pairing extraction
            // does not exist yet), so leaving the listener up would have
            // every case hang until the gateway timeout instead of failing
            // fast with a clean (if wrong) status.
            drop(upstream);
            let (dir, state) = capi_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            for path in [
                CAPI_ROUTE.to_owned(),                                  // no segment at all
                format!("{CAPI_ROUTE}/"),                               // no segment at all
                format!("{CAPI_ROUTE}//chat/completions"),              // empty segment
                format!("{CAPI_ROUTE}/wrong-pairing/chat/completions"), // wrong segment
            ] {
                let request = Request::builder()
                    .method(Method::POST)
                    .uri(format!("http://{proxy_address}{path}"))
                    // A valid header must not substitute for the path segment
                    // on this route: the header is ignored here.
                    .header(PAIRING_HEADER, PAIRING)
                    .header(AUTHORIZATION, "Bearer tid=from-client")
                    .body(Full::new(Bytes::from_static(b"{}")))
                    .unwrap();
                assert_eq!(
                    error_code(client.request(request).await.unwrap()).await,
                    (StatusCode::FORBIDDEN, "pairing_invalid".to_owned()),
                    "{path}"
                );
            }
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn capi_route_rejects_a_double_dot_segment_after_the_pairing() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let (dir, state) = capi_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}{CAPI_ROUTE}/{PAIRING}/../admin"
                ))
                .header(AUTHORIZATION, "Bearer tid=from-client")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                error_code(client.request(request).await.unwrap()).await,
                (StatusCode::BAD_REQUEST, "path_invalid".to_owned())
            );
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn capi_route_moves_the_client_token_to_x_llm_token() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(move |request: Request<Incoming>| async move {
                            assert_eq!(request.uri(), "/chat/completions");
                            assert_eq!(request.headers()["x-llm-token"], "tid=from-client");
                            assert_eq!(request.headers()[AUTHORIZATION], "Bearer identity");
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
                        }),
                    )
                    .await
                    .unwrap();
            });
            let (dir, state) = capi_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}{CAPI_ROUTE}/{PAIRING}/chat/completions"
                ))
                .header(AUTHORIZATION, "Bearer tid=from-client")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn capi_route_ping_without_credential_is_forwarded_without_x_llm_token() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(move |request: Request<Incoming>| async move {
                            assert_eq!(request.uri(), "/_ping");
                            assert!(!request.headers().contains_key("x-llm-token"));
                            Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(b"ok"))))
                        }),
                    )
                    .await
                    .unwrap();
            });
            let (dir, state) = capi_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let request = Request::builder()
                .method(Method::GET)
                .uri(format!(
                    "http://{proxy_address}{CAPI_ROUTE}/{PAIRING}/_ping"
                ))
                .body(Full::new(Bytes::new()))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn capi_route_ping_post_without_credential_is_401() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let (dir, state) = capi_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}{CAPI_ROUTE}/{PAIRING}/_ping"
                ))
                .body(Full::new(Bytes::new()))
                .unwrap();
            assert_eq!(
                error_code(client.request(request).await.unwrap()).await,
                (
                    StatusCode::UNAUTHORIZED,
                    "client_credential_missing".to_owned()
                )
            );
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn capi_route_ping_without_pairing_is_403() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let (dir, state) = capi_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let request = Request::builder()
                .method(Method::GET)
                .uri(format!("http://{proxy_address}{CAPI_ROUTE}/wrong/_ping"))
                .body(Full::new(Bytes::new()))
                .unwrap();
            assert_eq!(
                error_code(client.request(request).await.unwrap()).await,
                (StatusCode::FORBIDDEN, "pairing_invalid".to_owned())
            );
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn capi_route_upgrade_without_pairing_is_403_and_not_forwarded() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let (dir, state) = capi_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let request = upgrade_request(format!(
                "http://{proxy_address}{CAPI_ROUTE}/wrong/responses"
            ));
            assert_eq!(
                error_code(client.request(request).await.unwrap()).await,
                (StatusCode::FORBIDDEN, "pairing_invalid".to_owned())
            );
            assert!(
                no_connection_arrives(&upstream).await,
                "an upgrade refused for its pairing must not be forwarded upstream"
            );
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn upgrade_on_another_route_is_400_but_other_upgrade_tokens_still_forward() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                loop {
                    let (socket, _) = match upstream.accept().await {
                        Ok(accepted) => accepted,
                        Err(_) => break,
                    };
                    tokio::spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                TokioIo::new(socket),
                                service_fn(|_request: Request<Incoming>| async move {
                                    Ok::<_, Infallible>(Response::new(Full::new(
                                        Bytes::from_static(b"ok"),
                                    )))
                                }),
                            )
                            .await;
                    });
                }
            });
            let (dir, state) = request_source_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            // Only /vscode-copilot-capi tunnels an upgrade; on any other
            // route Upgrade: websocket is refused outright.
            let websocket_upgrade = Request::builder()
                .method(Method::GET)
                .uri(format!("http://{proxy_address}/copilot-cli/v1/models"))
                .header(PAIRING_HEADER, PAIRING)
                .header(AUTHORIZATION, "Bearer tid=from-client")
                .header(CONNECTION, "upgrade")
                .header("upgrade", "websocket")
                .body(Full::new(Bytes::new()))
                .unwrap();
            // Checked as a plain status first (not via the shared
            // `error_code` helper, which assumes a JSON error body): today
            // this request is not refused at all and is forwarded as an
            // ordinary request, so the response has no content-type header
            // for `error_code` to read.
            let response = client.request(websocket_upgrade).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "an Upgrade: websocket request on a non-CAPI route must be refused, not forwarded"
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["error"]["code"], "upgrade_not_supported");
            // Only the websocket token is refused; another Upgrade token on
            // the same route is stripped (as a hop-by-hop header) and
            // forwarded as an ordinary request, as before this feature.
            let other_upgrade = Request::builder()
                .method(Method::GET)
                .uri(format!("http://{proxy_address}/copilot-cli/v1/models"))
                .header(PAIRING_HEADER, PAIRING)
                .header(AUTHORIZATION, "Bearer tid=from-client")
                .header(CONNECTION, "upgrade")
                .header("upgrade", "h2c")
                .body(Full::new(Bytes::new()))
                .unwrap();
            let response = client.request(other_upgrade).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::OK,
                "a non-websocket Upgrade token must still be forwarded"
            );
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn capi_route_non_101_upstream_response_passes_through() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(|_request: Request<Incoming>| async move {
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::BAD_GATEWAY)
                                    .body(Full::new(Bytes::from_static(b"nope")))
                                    .unwrap(),
                            )
                        }),
                    )
                    .await
                    .unwrap();
            });
            let (dir, state) = capi_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let mut request = upgrade_request(format!(
                "http://{proxy_address}{CAPI_ROUTE}/{PAIRING}/responses"
            ));
            request
                .headers_mut()
                .insert(AUTHORIZATION, "Bearer tid=from-client".parse().unwrap());
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body, Bytes::from_static(b"nope"));
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn capi_route_websocket_tunnel_relays_the_101_and_one_frame_each_way_with_gateway_credentials()
     {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (mut socket, _) = upstream.accept().await.unwrap();
                let (method, path, headers) = read_http_head(&mut socket).await;
                assert_eq!(method, "GET");
                assert_eq!(path, "/responses");
                assert_eq!(
                    headers.get("authorization").map(String::as_str),
                    Some("Bearer identity"),
                    "the gateway identity must reach the upstream on the handshake"
                );
                assert_eq!(
                    headers.get("x-llm-token").map(String::as_str),
                    Some("tid=from-client"),
                    "the client's own token must reach the upstream on the handshake"
                );
                let accept =
                    ws_accept(headers.get("sec-websocket-key").map(String::as_str).unwrap_or(""));
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                let (opcode, payload) = ws_read_frame(&mut socket).await;
                assert_eq!(opcode, 0x1);
                assert_eq!(payload, b"hello from vs code");
                socket
                    .write_all(&ws_frame(b"hello from github", false))
                    .await
                    .unwrap();
            });
            let (dir, state) = capi_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let mut request = upgrade_request(format!(
                "http://{proxy_address}{CAPI_ROUTE}/{PAIRING}/responses"
            ));
            request
                .headers_mut()
                .insert(AUTHORIZATION, "Bearer tid=from-client".parse().unwrap());
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
            assert_eq!(response.headers()["upgrade"], "websocket");
            assert!(
                response.headers()["connection"]
                    .to_str()
                    .unwrap()
                    .eq_ignore_ascii_case("upgrade")
            );
            assert_eq!(
                response.headers()["sec-websocket-accept"],
                ws_accept(SEC_WEBSOCKET_KEY)
            );
            let upgraded = hyper::upgrade::on(response).await.unwrap();
            let mut io = TokioIo::new(upgraded);
            io.write_all(&ws_frame(b"hello from vs code", true))
                .await
                .unwrap();
            let mut header = [0u8; 2];
            io.read_exact(&mut header).await.unwrap();
            assert_eq!(header[0] & 0x0f, 0x1, "expected a text frame back");
            let len = (header[1] & 0x7f) as usize;
            assert!(header[1] & 0x80 == 0, "a server frame must not be masked");
            let mut payload = vec![0u8; len];
            io.read_exact(&mut payload).await.unwrap();
            assert_eq!(payload, b"hello from github");
            proxy.abort();
            let _ = proxy.await;
            drop(client);
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn dropping_the_proxy_closes_an_open_capi_tunnel() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (mut socket, _) = upstream.accept().await.unwrap();
                let (_, _, headers) = read_http_head(&mut socket).await;
                let accept =
                    ws_accept(headers.get("sec-websocket-key").map(String::as_str).unwrap_or(""));
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                // Held open until the client side sees it close.
                let mut buffer = [0u8; 1];
                let _ = socket.read(&mut buffer).await;
            });
            let (dir, state) = capi_state(address).await;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, default_config()));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let mut request = upgrade_request(format!(
                "http://{proxy_address}{CAPI_ROUTE}/{PAIRING}/responses"
            ));
            request
                .headers_mut()
                .insert(AUTHORIZATION, "Bearer tid=from-client".parse().unwrap());
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
            let upgraded = hyper::upgrade::on(response).await.unwrap();
            let mut io = TokioIo::new(upgraded);
            proxy.abort();
            let _ = proxy.await;
            // The tunnel task is owned by the connection's JoinSet: dropping
            // the server must close it, and the client sees EOF.
            let mut buffer = [0u8; 1];
            let read = tokio::time::timeout(Duration::from_secs(5), io.read(&mut buffer))
                .await
                .expect("dropping the proxy must close the open tunnel promptly")
                .unwrap();
            assert_eq!(read, 0, "expected EOF once the tunnel closes");
            upstream_task.abort();
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn capi_tunnel_closes_when_the_cached_credential_expires() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (mut socket, _) = upstream.accept().await.unwrap();
                let (_, _, headers) = read_http_head(&mut socket).await;
                let accept =
                    ws_accept(headers.get("sec-websocket-key").map(String::as_str).unwrap_or(""));
                socket
                    .write_all(
                        format!(
                            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                let mut buffer = [0u8; 1];
                let _ = socket.read(&mut buffer).await;
            });
            let (dir, state) = capi_controller_jwt_state(address).await;
            let cache = Arc::new(CredentialCache::default());
            let key = format!("vscode-copilot\u{0}http://{address}/\u{0}agentgateway");
            // Inserted directly (bypassing `insert`'s expiry-margin floor,
            // which would otherwise refuse to cache anything this close to
            // expiry). The cache reuses the entry for a minute, so the
            // request always gets it however slowly it arrives; the
            // credential itself expires in 300 ms, and that is the deadline
            // the tunnel must close at.
            cache.entries.lock().unwrap().insert(
                key,
                CachedCredential {
                    credential: "short-lived".to_owned(),
                    device_id: "device-test".to_owned(),
                    valid_until: std::time::Instant::now() + Duration::from_secs(60),
                    valid_until_unix: now_unix() + 60,
                    expires: std::time::Instant::now() + Duration::from_millis(300),
                    expires_unix: now_unix() + 60,
                },
            );
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve_with_cache(
                listener,
                state,
                default_config(),
                cache,
            ));
            let client: Client<HttpConnector, Full<Bytes>> =
                Client::builder(TokioExecutor::new()).build_http();
            let mut request = upgrade_request(format!(
                "http://{proxy_address}{CAPI_ROUTE}/{PAIRING}/responses"
            ));
            request
                .headers_mut()
                .insert(AUTHORIZATION, "Bearer tid=from-client".parse().unwrap());
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
            let upgraded = hyper::upgrade::on(response).await.unwrap();
            let mut io = TokioIo::new(upgraded);
            // Once the cached credential's deadline passes, the tunnel closes
            // on its own, with the proxy still running.
            let mut buffer = [0u8; 1];
            let read = tokio::time::timeout(Duration::from_secs(5), io.read(&mut buffer))
                .await
                .expect("the tunnel must close once its credential expires")
                .unwrap();
            assert_eq!(read, 0, "expected EOF once the tunnel closes");
            proxy.abort();
            let _ = proxy.await;
            upstream_task.abort();
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }
}
