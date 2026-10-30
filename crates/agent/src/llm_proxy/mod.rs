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
/// A cached controller credential is not used within this margin of its expiry.
const CREDENTIAL_EXPIRY_MARGIN: Duration = Duration::from_secs(30);
/// Longest a controller credential is served from the cache, counted from its
/// fetch. This bounds how long a revoked device keeps using the proxy:
/// revocation is enforced at issuance, the gateway validates tokens statelessly.
const CREDENTIAL_CACHE_TTL: Duration = Duration::from_secs(60);

/// Per-server cache of controller-issued gateway credentials, one per client
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
    /// Reuse the proxy's bounded cache for local credential-helper requests.
    pub(crate) async fn get_credential<F, Fut, E>(
        &self,
        key: &str,
        device_id: &str,
        fetch: F,
    ) -> Result<agentdesktop_core::model::LlmGatewayCredential, E>
    where
        F: FnOnce() -> Fut,
        Fut:
            std::future::Future<Output = Result<agentdesktop_core::model::LlmGatewayCredential, E>>,
    {
        let (credential, expiry, _) = self.get_or_fetch(key, device_id, fetch).await?;
        Ok(agentdesktop_core::model::LlmGatewayCredential {
            credential,
            expires_at_unix_seconds: expiry.unix,
        })
    }

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

/// Where a route takes the client id it presents to the gateway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RouteIdentity {
    /// The id is fixed by the route (the file the daemon owns decides it).
    Fixed(&'static str),
    /// The id comes from the registered session whose pairing the request carries.
    Session,
}

/// A fixed path prefix the proxy serves.
///
/// For a managed program the prefix is what its reconciler writes into the
/// client file, so the client id is decided by the file the daemon owns, never
/// by request headers; on the sandbox route the client id is the registered
/// session's. The prefix is stripped before forwarding.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProxyRoute {
    pub prefix: &'static str,
    pub identity: RouteIdentity,
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

/// The route for a program inside a sandbox: its pairing is a per-session
/// value minted over the local API (`sessions`), never the device pairing, and
/// the client id is the session's. The box needs nothing but this route, so
/// it holds no credential and needs nothing but loopback.
pub(crate) const SANDBOX_ROUTE: &str = "/sandbox";

/// Git relay route for allowed git hosts.
pub(crate) const GIT_ROUTE: &str = "/git";

/// Refusal route the generated gitconfig maps ssh-form remotes of allowlisted
/// hosts WITHOUT a `daemon.containment.git.ssh.hosts` entry to: it answers a
/// pkt-line `ERR` so the user sees a clear message instead of a deny-mode
/// timeout (hosts on the ssh allowlist use real ssh via `GIT_SSH_COMMAND`).
pub(crate) const GIT_SSH_ROUTE: &str = "/git-ssh";

/// The prefixed routes: the Copilot programs and the sandbox route. A prefix
/// matches only at a `/` boundary, so
/// `/vscode-copilot-passthrough/...` never matches `/vscode-copilot`.
pub(crate) const ROUTES: &[ProxyRoute] = &[
    ProxyRoute {
        prefix: "/vscode-copilot-passthrough",
        identity: RouteIdentity::Fixed("vscode-copilot"),
        credential: RouteCredential::Passthrough,
        upstream: Upstream::ProxyUrl,
        pairing_in_path: false,
        tunnels_upgrades: false,
        credential_less_ping: false,
    },
    ProxyRoute {
        prefix: CAPI_ROUTE,
        identity: RouteIdentity::Fixed("vscode-copilot"),
        credential: RouteCredential::Passthrough,
        upstream: Upstream::ProxyUrl,
        pairing_in_path: true,
        tunnels_upgrades: true,
        credential_less_ping: true,
    },
    ProxyRoute {
        prefix: "/vscode-copilot",
        identity: RouteIdentity::Fixed("vscode-copilot"),
        credential: RouteCredential::Gateway,
        upstream: Upstream::Url,
        pairing_in_path: false,
        tunnels_upgrades: false,
        credential_less_ping: false,
    },
    ProxyRoute {
        prefix: "/copilot-cli",
        identity: RouteIdentity::Fixed("copilot-cli"),
        credential: RouteCredential::Gateway,
        upstream: Upstream::Url,
        pairing_in_path: false,
        tunnels_upgrades: false,
        credential_less_ping: false,
    },
    ProxyRoute {
        prefix: SANDBOX_ROUTE,
        identity: RouteIdentity::Session,
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
                    inventory_interval: Duration::from_secs(60), controller: None, llm_proxy: None,
                    usage_receiver: None, containment: None },
                discovery, enrollment: EnrollmentState::new(false), state_dir: dir.path().to_owned(),
                oidc_callback_listen: None, telemetry: None, logout: None, usage: None,
                programs: None,
                violation_ledger: None,
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
            sessions: SessionRegistry::default(),
            git: None,
            egress_upstream: None,
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
        assert_eq!(route.identity, RouteIdentity::Fixed("vscode-copilot"));
        assert_eq!(route.credential, RouteCredential::Passthrough);
        assert_eq!(rest, "/chat/completions");
        let (route, rest) = match_route("/vscode-copilot/v1/chat/completions").unwrap();
        assert_eq!(route.credential, RouteCredential::Gateway);
        assert_eq!(rest, "/v1/chat/completions");
        assert_eq!(match_route("/copilot-cli").unwrap().1, "/");
        assert!(match_route("/copilot-client/v1").is_none());
        assert!(match_route("/v1/chat/completions").is_none());
        let (route, rest) = match_route("/sandbox/v1/messages").unwrap();
        assert_eq!(route.identity, RouteIdentity::Session);
        assert_eq!(rest, "/v1/messages");
        assert_eq!(match_route("/sandbox").unwrap().1, "/");
        assert!(match_route("/sandboxx/v1/messages").is_none());
        assert!(match_route("/sandbox-x/v1/messages").is_none());
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
                RouteIdentity::Fixed("copilot-cli"),
                RouteCredential::Gateway,
                Upstream::Url,
            ),
            (
                "/vscode-copilot/v1/x",
                RouteIdentity::Fixed("vscode-copilot"),
                RouteCredential::Gateway,
                Upstream::Url,
            ),
            (
                "/vscode-copilot-passthrough/chat/completions",
                RouteIdentity::Fixed("vscode-copilot"),
                RouteCredential::Passthrough,
                Upstream::ProxyUrl,
            ),
            (
                "/vscode-copilot-capi/PAIRING/responses",
                RouteIdentity::Fixed("vscode-copilot"),
                RouteCredential::Passthrough,
                Upstream::ProxyUrl,
            ),
            (
                "/sandbox/v1/messages",
                RouteIdentity::Session,
                RouteCredential::Gateway,
                Upstream::Url,
            ),
        ];
        for (path, identity, credential, upstream) in expected {
            let (route, _) = match_route(path).unwrap_or_else(|| panic!("{path}"));
            assert_eq!(route.identity, identity, "{path}");
            assert_eq!(route.credential, credential, "{path}");
            assert_eq!(route.upstream, upstream, "{path}");
        }
        let (sandbox, _) = match_route("/sandbox/v1/messages").unwrap();
        assert!(!sandbox.pairing_in_path);
        assert!(!sandbox.tunnels_upgrades);
        assert!(!sandbox.credential_less_ping);
        assert_eq!(ROUTES.len(), 5);
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
                    usage_receiver: None,
                    containment: None,
                },
                discovery,
                enrollment: EnrollmentState::new(true),
                state_dir: dir.path().to_owned(),
                oidc_callback_listen: None,
                telemetry: None,
                logout: None,
                usage: None,
                programs: None,
                violation_ledger: None,
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
                usage_receiver: None,
                containment: None,
            },
            discovery,
            enrollment: EnrollmentState::new(false),
            state_dir: dir.path().to_owned(),
            oidc_callback_listen: None,
            telemetry: None,
            logout: None,
            usage: None,
            programs: None,
            violation_ledger: None,
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
                usage_receiver: None,
                containment: None,
            },
            discovery,
            enrollment: EnrollmentState::new(false),
            state_dir: dir.path().to_owned(),
            oidc_callback_listen: None,
            telemetry: None,
            logout: None,
            usage: None,
            programs: None,
            violation_ledger: None,
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
                usage_receiver: None,
                containment: None,
            },
            discovery,
            enrollment: EnrollmentState::new(false),
            state_dir: dir.path().to_owned(),
            oidc_callback_listen: None,
            telemetry: None,
            logout: None,
            usage: None,
            programs: None,
            violation_ledger: None,
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

    // --- /sandbox route, per-session pairing and the sandbox request log.

    const SESSION_TTL: Duration = Duration::from_secs(3600);

    fn new_session(sessions: &SessionRegistry, client_id: &str) -> sessions::Session {
        sessions
            .register(client_id, "test session", SESSION_TTL, now_unix())
            .unwrap_or_else(|error| panic!("register a session: {error:?}"))
    }

    fn http_client() -> Client<HttpConnector, Full<Bytes>> {
        Client::builder(TokioExecutor::new()).build_http()
    }

    /// A POST to `path_and_query` on the proxy, carrying `pairing` in the pairing header.
    fn post_with(
        proxy: std::net::SocketAddr,
        path_and_query: &str,
        pairing: Option<&str>,
        body: &'static [u8],
    ) -> Request<Full<Bytes>> {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(format!("http://{proxy}{path_and_query}"));
        if let Some(pairing) = pairing {
            builder = builder.header(PAIRING_HEADER, pairing);
        }
        builder.body(Full::new(Bytes::from_static(body))).unwrap()
    }

    /// Status, code and message of a refusal.
    async fn refusal(response: Response<Incoming>) -> (StatusCode, String, String) {
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        (
            status,
            json["error"]["code"].as_str().unwrap().to_owned(),
            json["error"]["message"].as_str().unwrap().to_owned(),
        )
    }

    #[derive(Clone)]
    struct Upstreamed {
        uri: String,
        headers: HeaderMap,
        body: Bytes,
    }

    type SeenRequests = Arc<std::sync::Mutex<Vec<Upstreamed>>>;

    /// An upstream that answers every request with 200 "ok" and records it.
    async fn recording_upstream() -> (
        std::net::SocketAddr,
        SeenRequests,
        tokio::task::JoinHandle<()>,
    ) {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let seen: SeenRequests = Arc::default();
        let record = seen.clone();
        let task = tokio::spawn(async move {
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
                                    let (parts, body) = request.into_parts();
                                    let body = body.collect().await.unwrap().to_bytes();
                                    record.lock().unwrap().push(Upstreamed {
                                        uri: parts.uri.to_string(),
                                        headers: parts.headers,
                                        body,
                                    });
                                    Ok::<_, Infallible>(Response::new(Full::new(
                                        Bytes::from_static(b"ok"),
                                    )))
                                }
                            }),
                        )
                        .await;
                });
            }
        });
        (address, seen, task)
    }

    /// An upstream that answers with the scripted statuses in order (200 "ok"
    /// once the script runs out) and counts the requests it saw.
    async fn scripted_upstream(
        script: Vec<u16>,
    ) -> (
        std::net::SocketAddr,
        Arc<std::sync::atomic::AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        let task = tokio::spawn(async move {
            loop {
                let (socket, _) = upstream.accept().await.unwrap();
                let counter = counter.clone();
                let script = script.clone();
                tokio::spawn(async move {
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(
                            TokioIo::new(socket),
                            service_fn(move |request: Request<Incoming>| {
                                let counter = counter.clone();
                                let script = script.clone();
                                async move {
                                    let _ = request.into_body().collect().await;
                                    let index =
                                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    let status = script.get(index).copied().unwrap_or(200);
                                    Ok::<_, Infallible>(
                                        Response::builder()
                                            .status(status)
                                            .body(Full::new(Bytes::from_static(b"ok")))
                                            .unwrap(),
                                    )
                                }
                            }),
                        )
                        .await;
                });
            }
        });
        (address, hits, task)
    }

    /// Sends raw request bytes over a fresh TCP connection (hyper's client
    /// cannot send CONNECT or absolute-form request targets) and returns the
    /// response status plus the full response text once the Content-Length
    /// body arrived.
    async fn raw_http(proxy: std::net::SocketAddr, request: String) -> (u16, String) {
        let mut stream = TcpStream::connect(proxy).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buffer = Vec::new();
        loop {
            let mut chunk = [0u8; 4096];
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            let text = String::from_utf8_lossy(&buffer);
            if let Some(header_end) = text.find("\r\n\r\n") {
                let length = text[..header_end]
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if buffer.len() >= header_end + 4 + length {
                    break;
                }
            }
        }
        let text = String::from_utf8_lossy(&buffer).into_owned();
        let status = text
            .split(' ')
            .nth(1)
            .and_then(|status| status.parse().ok())
            .unwrap_or_else(|| panic!("no status line in {text:?}"));
        (status, text)
    }

    /// The `error.code` of a raw refusal (the JSON body after the headers).
    fn raw_error_code(text: &str) -> String {
        let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
        let json: serde_json::Value =
            serde_json::from_str(body).unwrap_or_else(|_| panic!("no JSON body in {text:?}"));
        json["error"]["code"].as_str().unwrap_or("").to_owned()
    }

    // The sandbox route is the copilot-cli shape with a session pairing: prefix
    // stripped, every client credential and the pairing removed, the gateway
    // identity added, other headers kept, and nothing forwarded when refused.
    #[tokio::test]
    async fn sandbox_route_accepts_a_session_pairing_and_forwards_with_the_gateway_identity() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let (address, seen, upstream_task) = recording_upstream().await;
            let (dir, state) = request_source_state(address).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();

            // Refusals first: nothing may reach the gateway.
            let unpaired = post_with(proxy_address, "/sandbox/v1/messages", None, b"{}");
            let (status, code, message) = refusal(client.request(unpaired).await.unwrap()).await;
            assert_eq!((status, code.as_str()), (StatusCode::FORBIDDEN, "pairing_invalid"));
            assert_eq!(
                message,
                "agentdesktop proxy: session pairing missing, expired or revoked; start a new session"
            );
            let wrong = format!("{}X", &session.pairing[..session.pairing.len() - 1]);
            let wrong = post_with(proxy_address, "/sandbox/v1/messages", Some(&wrong), b"{}");
            assert_eq!(
                refusal(client.request(wrong).await.unwrap()).await.1,
                "pairing_invalid"
            );
            // The pairing is only accepted in its own header, never as the bearer.
            let as_bearer = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{proxy_address}/sandbox/v1/messages"))
                .header(AUTHORIZATION, format!("Bearer {}", session.pairing))
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                refusal(client.request(as_bearer).await.unwrap()).await.1,
                "pairing_invalid"
            );
            let escape = post_with(
                proxy_address,
                "/sandbox/../admin",
                Some(&session.pairing),
                b"{}",
            );
            assert_eq!(
                refusal(client.request(escape).await.unwrap()).await.1,
                "path_invalid"
            );
            let browser = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{proxy_address}/sandbox/v1/messages"))
                .header(PAIRING_HEADER, &*session.pairing)
                .header(ORIGIN, "https://evil.example")
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                refusal(client.request(browser).await.unwrap()).await.1,
                "browser_not_allowed"
            );
            let options = Request::builder()
                .method(Method::OPTIONS)
                .uri(format!("http://{proxy_address}/sandbox/v1/messages"))
                .header(PAIRING_HEADER, &*session.pairing)
                .body(Full::new(Bytes::new()))
                .unwrap();
            assert_eq!(
                refusal(client.request(options).await.unwrap()).await.1,
                "method_not_allowed"
            );
            assert!(seen.lock().unwrap().is_empty(), "a refused request was forwarded");

            // The device routes keep their message.
            let device_unpaired = post_with(proxy_address, "/copilot-cli/v1/x", None, b"{}");
            let (_, _, message) = refusal(client.request(device_unpaired).await.unwrap()).await;
            assert!(message.contains("pairing value missing or wrong"), "{message}");

            // The accepted request.
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{proxy_address}/sandbox/v1/messages?beta=true"))
                .header(PAIRING_HEADER, &*session.pairing)
                .header(AUTHORIZATION, "Bearer a-real-key-by-mistake")
                .header("x-api-key", "placeholder")
                .header("api-key", "placeholder")
                .header("x-llm-token", "must-not-forward")
                .header("anthropic-version", "2023-06-01")
                .body(Full::new(Bytes::from_static(b"{\"n\":1}")))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "ok"
            );
            {
                let seen = seen.lock().unwrap();
                assert_eq!(seen.len(), 1);
                let forwarded = &seen[0];
                assert_eq!(forwarded.uri, "/v1/messages?beta=true");
                assert_eq!(forwarded.body, Bytes::from_static(b"{\"n\":1}"));
                assert_eq!(forwarded.headers[AUTHORIZATION], "Bearer identity");
                assert_eq!(forwarded.headers["anthropic-version"], "2023-06-01");
                assert_eq!(forwarded.headers[HOST], address.to_string());
                for removed in ["x-api-key", "api-key", "x-llm-token", PAIRING_HEADER] {
                    assert!(!forwarded.headers.contains_key(removed), "{removed}");
                }
            }
            proxy.abort();
            let _ = proxy.await;
            upstream_task.abort();
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    // The device pairing never opens the sandbox route (the box must not be able
    // to turn a device-pairing file into a session).
    #[tokio::test]
    async fn sandbox_route_refuses_the_device_pairing() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let (address, seen, upstream_task) = recording_upstream().await;
            let (dir, state) = request_source_state(address).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            new_session(&sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();
            let request = post_with(proxy_address, "/sandbox/v1/messages", Some(PAIRING), b"{}");
            assert_eq!(
                refusal(client.request(request).await.unwrap()).await.1,
                "pairing_invalid"
            );
            assert!(seen.lock().unwrap().is_empty());
            proxy.abort();
            let _ = proxy.await;
            upstream_task.abort();
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    // A session pairing is a bearer for /sandbox only: it never equals the
    // device pairing, so every other route (and the prefix-less one) refuses it.
    #[tokio::test]
    async fn session_pairing_is_refused_on_every_other_route() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let (address, seen, upstream_task) = recording_upstream().await;
            let (dir, state) = capi_state(address).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();
            let capi = format!("/vscode-copilot-capi/{}/responses", session.pairing);
            for path in [
                "/copilot-cli/v1/messages",
                "/vscode-copilot/v1/messages",
                "/vscode-copilot-passthrough/chat/completions",
                "/v1/messages",
                capi.as_str(),
            ] {
                let request = post_with(proxy_address, path, Some(&session.pairing), b"{}");
                let (status, code, _) = refusal(client.request(request).await.unwrap()).await;
                assert_eq!(
                    (status, code.as_str()),
                    (StatusCode::FORBIDDEN, "pairing_invalid"),
                    "{path}"
                );
            }
            assert!(seen.lock().unwrap().is_empty());
            proxy.abort();
            let _ = proxy.await;
            upstream_task.abort();
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn expired_and_revoked_sessions_are_refused() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let (address, seen, upstream_task) = recording_upstream().await;
            let (dir, state) = request_source_state(address).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            // Registered an hour ago with a one-minute idle timeout.
            let expired = sessions
                .register(
                    "claude-code",
                    "old",
                    Duration::from_secs(60),
                    now_unix() - 3600,
                )
                .ok()
                .unwrap();
            let revoked = new_session(&sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();
            let request = post_with(
                proxy_address,
                "/sandbox/v1/messages",
                Some(&expired.pairing),
                b"{}",
            );
            let (status, code, _) = refusal(client.request(request).await.unwrap()).await;
            assert_eq!(
                (status, code.as_str()),
                (StatusCode::FORBIDDEN, "pairing_invalid")
            );

            let request = post_with(
                proxy_address,
                "/sandbox/v1/messages",
                Some(&revoked.pairing),
                b"{}",
            );
            assert_eq!(
                client.request(request).await.unwrap().status(),
                StatusCode::OK
            );
            assert!(sessions.revoke(&revoked.id));
            let request = post_with(
                proxy_address,
                "/sandbox/v1/messages",
                Some(&revoked.pairing),
                b"{}",
            );
            let (status, code, _) = refusal(client.request(request).await.unwrap()).await;
            assert_eq!(
                (status, code.as_str()),
                (StatusCode::FORBIDDEN, "pairing_invalid")
            );
            assert_eq!(
                seen.lock().unwrap().len(),
                1,
                "only the live request is forwarded"
            );
            proxy.abort();
            let _ = proxy.await;
            upstream_task.abort();
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    // A first segment that merely starts like the sandbox prefix is a mistyped
    // client file, not the prefix-less route. The device pairing gets the route
    // error; a session pairing is not the device pairing and gets the pairing error.
    #[tokio::test]
    async fn sandbox_lookalike_prefixes_are_unknown_routes_or_refused() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let (address, seen, upstream_task) = recording_upstream().await;
            let (dir, state) = request_source_state(address).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();
            for path in [
                "/sandboxx/v1/messages",
                "/sandbox2/v1/messages",
                "/Sandbox/v1/messages",
            ] {
                let request = post_with(proxy_address, path, Some(PAIRING), b"{}");
                let (status, code, message) = refusal(client.request(request).await.unwrap()).await;
                assert_eq!(
                    (status, code.as_str()),
                    (StatusCode::NOT_FOUND, "route_unknown"),
                    "{path}"
                );
                assert!(message.contains("/sandbox"), "{message}");
                let request = post_with(proxy_address, path, Some(&session.pairing), b"{}");
                let (status, code, _) = refusal(client.request(request).await.unwrap()).await;
                assert_eq!(
                    (status, code.as_str()),
                    (StatusCode::FORBIDDEN, "pairing_invalid"),
                    "{path}"
                );
            }
            assert!(seen.lock().unwrap().is_empty());
            proxy.abort();
            let _ = proxy.await;
            upstream_task.abort();
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[test]
    fn sandbox_lookalikes_resemble_a_route() {
        for path in ["/sandboxx/v1", "/sandbox2/v1", "/Sandbox/v1", "/sandbox-x"] {
            assert!(resembles_route(path), "{path}");
        }
        assert!(match_route("/sandbox/v1/messages").is_some());
    }

    // The identity is the session's client id, so credentials are cached per
    // client id and shared by every session of that id.
    #[tokio::test]
    async fn sandbox_sessions_use_their_client_id_and_share_the_cached_credential() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let (address, seen, upstream_task) = recording_upstream().await;
            let (dir, state) = controller_jwt_state(address, false).await;
            let cache = Arc::new(CredentialCache::default());
            let issued = |value: &str| agentdesktop_core::model::LlmGatewayCredential {
                credential: value.to_owned(),
                expires_at_unix_seconds: now_unix() + 600,
            };
            for (client_id, jwt) in [("claude-code", "jwt-claude"), ("my_tool", "jwt-tool")] {
                cache.insert(
                    &format!("{client_id}\u{0}http://{address}/\u{0}agentgateway"),
                    "device-test",
                    &issued(jwt),
                );
            }
            let config = default_config();
            let sessions = config.sessions.clone();
            let first = new_session(&sessions, "claude-code");
            let second = new_session(&sessions, "claude-code");
            let other = new_session(&sessions, "my_tool");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve_with_cache(listener, state, config, cache));
            let client = http_client();
            for session in [&first, &second, &other] {
                let request = post_with(
                    proxy_address,
                    "/sandbox/v1/messages",
                    Some(&session.pairing),
                    b"{}",
                );
                assert_eq!(
                    client.request(request).await.unwrap().status(),
                    StatusCode::OK
                );
            }
            let authorizations: Vec<String> = seen
                .lock()
                .unwrap()
                .iter()
                .map(|request| request.headers[AUTHORIZATION].to_str().unwrap().to_owned())
                .collect();
            assert_eq!(
                authorizations,
                ["Bearer jwt-claude", "Bearer jwt-claude", "Bearer jwt-tool"]
            );
            proxy.abort();
            let _ = proxy.await;
            upstream_task.abort();
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    // Like `cached_credential_is_used_and_dropped_on_401`, on the sandbox route.
    #[tokio::test]
    async fn sandbox_route_retries_once_on_401_and_drops_the_rejected_cached_credential() {
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
            let key = format!("claude-code\u{0}http://{address}/\u{0}agentgateway");
            let issued = |value: &str| agentdesktop_core::model::LlmGatewayCredential {
                credential: value.to_owned(),
                expires_at_unix_seconds: now_unix() + 600,
            };
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve_with_cache(listener, state, config, cache.clone()));
            let client = http_client();
            let request = |body: &'static [u8]| {
                post_with(
                    proxy_address,
                    "/sandbox/v1/messages",
                    Some(&session.pairing),
                    body,
                )
            };
            cache.insert(&key, "device-test", &issued("good"));
            let response = client.request(request(b"{\"n\":1}")).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(cache.get(&key, "device-test").as_deref(), Some("good"));
            // Rejected: entry dropped, one retry that needs a controller.
            cache.insert(&key, "device-test", &issued("stale"));
            let (status, code, _) =
                refusal(client.request(request(b"{\"n\":2}")).await.unwrap()).await;
            assert_eq!(code, "agentdesktop_credential", "{status}");
            assert_eq!(cache.get(&key, "device-test"), None);
            assert_eq!(
                seen.lock().unwrap().clone(),
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

    // AC 5: revoking a session leaves the response it is already receiving
    // intact; the next request is refused.
    #[tokio::test]
    async fn revoking_a_session_does_not_cut_a_response_in_flight() {
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let (release_tx, release_rx) = oneshot::channel::<()>();
            let release = Arc::new(tokio::sync::Mutex::new(Some(release_rx)));
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(move |request: Request<Incoming>| {
                            let release = release.clone();
                            async move {
                                let _ = request.into_body().collect().await;
                                let gate = release.lock().await.take();
                                let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(2);
                                tokio::spawn(async move {
                                    tx.send(Ok(Frame::data(Bytes::from_static(b"first "))))
                                        .await
                                        .unwrap();
                                    if let Some(gate) = gate {
                                        gate.await.unwrap();
                                    }
                                    let _ =
                                        tx.send(Ok(Frame::data(Bytes::from_static(b"last")))).await;
                                });
                                Ok::<_, Infallible>(
                                    Response::builder()
                                        .header("content-type", "text/event-stream")
                                        .body(StreamBody::new(ReceiverStream::new(rx)))
                                        .unwrap(),
                                )
                            }
                        }),
                    )
                    .await;
            });
            let (dir, state) = request_source_state(address).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();
            let request = post_with(
                proxy_address,
                "/sandbox/v1/messages",
                Some(&session.pairing),
                b"{}",
            );
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let mut body = response.into_body();
            let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
            assert_eq!(first, "first ");
            assert!(sessions.revoke(&session.id));
            release_tx.send(()).unwrap();
            let rest = body.collect().await.unwrap().to_bytes();
            assert_eq!(rest, "last");
            let request = post_with(
                proxy_address,
                "/sandbox/v1/messages",
                Some(&session.pairing),
                b"{}",
            );
            assert_eq!(
                refusal(client.request(request).await.unwrap()).await.1,
                "pairing_invalid"
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

    // --- AC 3: the sandbox request log.

    #[derive(Clone, Default)]
    struct LogCapture(Arc<std::sync::Mutex<Vec<u8>>>);

    struct LogWriter(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = LogWriter;

        fn make_writer(&'a self) -> LogWriter {
            LogWriter(self.0.clone())
        }
    }

    const SANDBOX_LOG_MESSAGE: &str = "LLM proxy sandbox request";

    impl LogCapture {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }

        fn sandbox_lines(&self) -> Vec<String> {
            self.text()
                .lines()
                .filter(|line| line.contains(SANDBOX_LOG_MESSAGE))
                .map(str::to_owned)
                .collect()
        }

        /// Waits (up to 5 s) until at least `count` sandbox lines were emitted;
        /// the line of a streamed response appears when its body ends.
        async fn wait_for_lines(&self, count: usize) -> Vec<String> {
            for _ in 0..100 {
                let lines = self.sandbox_lines();
                if lines.len() >= count {
                    return lines;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let lines = self.sandbox_lines();
            assert!(
                lines.len() >= count,
                "expected {count} sandbox log lines, got {lines:?}"
            );
            lines
        }
    }

    fn capture_logs() -> (LogCapture, tracing::subscriber::DefaultGuard) {
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        (capture, tracing::subscriber::set_default(subscriber))
    }

    /// The value of `key=value` in a formatted event line, without quotes.
    fn log_field(line: &str, key: &str) -> String {
        let marker = format!(" {key}=");
        let start = line
            .find(&marker)
            .unwrap_or_else(|| panic!("no field {key} in {line}"))
            + marker.len();
        line[start..]
            .split(' ')
            .next()
            .unwrap()
            .trim_matches('"')
            .to_owned()
    }

    /// hyper never polls a body past its declared end, so a `Content-Length`
    /// response and an empty one (204) finish through `Drop`: still `complete`.
    #[tokio::test]
    async fn sandbox_request_log_records_sized_and_empty_responses_as_complete() {
        let (logs, _guard) = capture_logs();
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(move |request: Request<Incoming>| async move {
                            let path = request.uri().path().to_owned();
                            let _ = request.into_body().collect().await;
                            let response = if path.ends_with("/v1/models") {
                                Response::builder()
                                    .status(StatusCode::NO_CONTENT)
                                    .body(Full::new(Bytes::new()))
                                    .unwrap()
                            } else {
                                Response::builder()
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from_static(b"{\"ok\":true}")))
                                    .unwrap()
                            };
                            Ok::<_, Infallible>(response)
                        }),
                    )
                    .await;
            });
            let (dir, state) = request_source_state(address).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();

            let sized = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{proxy_address}/sandbox/v1/messages"))
                .header(PAIRING_HEADER, &*session.pairing)
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            let response = client.request(sized).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "{\"ok\":true}"
            );
            let lines = logs.wait_for_lines(1).await;
            assert_eq!(log_field(&lines[0], "status"), "200");
            assert_eq!(log_field(&lines[0], "bytes_out"), "11");
            assert_eq!(log_field(&lines[0], "outcome"), "complete", "{}", lines[0]);

            let empty = Request::builder()
                .method(Method::GET)
                .uri(format!("http://{proxy_address}/sandbox/v1/models"))
                .header(PAIRING_HEADER, &*session.pairing)
                .body(Full::new(Bytes::new()))
                .unwrap();
            let response = client.request(empty).await.unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            let lines = logs.wait_for_lines(2).await;
            assert_eq!(log_field(&lines[1], "status"), "204");
            assert_eq!(log_field(&lines[1], "bytes_out"), "0");
            assert_eq!(log_field(&lines[1], "outcome"), "complete", "{}", lines[1]);
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(logs.sandbox_lines().len(), 2, "one event per request");

            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sandbox_request_log_records_a_streamed_200_once_when_the_body_ends() {
        let (logs, _guard) = capture_logs();
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(move |request: Request<Incoming>| async move {
                            let _ = request.into_body().collect().await;
                            let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(2);
                            tokio::spawn(async move {
                                tx.send(Ok(Frame::data(Bytes::from_static(b"hello"))))
                                    .await
                                    .unwrap();
                                tokio::time::sleep(Duration::from_millis(50)).await;
                                let _ = tx
                                    .send(Ok(Frame::data(Bytes::from_static(b"world!"))))
                                    .await;
                            });
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .header("content-type", "text/event-stream")
                                    .body(StreamBody::new(ReceiverStream::new(rx)))
                                    .unwrap(),
                            )
                        }),
                    )
                    .await;
            });
            let (dir, state) = request_source_state(address).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}/sandbox/v1/messages?beta=true"
                ))
                .header(PAIRING_HEADER, &*session.pairing)
                .header(AUTHORIZATION, "Bearer real-key-by-mistake")
                .header("x-api-key", "placeholder-key")
                .body(Full::new(Bytes::from_static(b"{\"n\":1}")))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "helloworld!"
            );
            let lines = logs.wait_for_lines(1).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            let lines_later = logs.sandbox_lines();
            assert_eq!(
                lines_later.len(),
                1,
                "exactly one event per request: {lines_later:?}"
            );
            let line = &lines[0];
            assert!(line.contains("INFO"), "{line}");
            assert_eq!(log_field(line, "session"), session.id);
            assert_eq!(log_field(line, "client_id"), "claude-code");
            assert_eq!(log_field(line, "method"), "POST");
            assert_eq!(log_field(line, "path"), "/v1/messages");
            assert_eq!(log_field(line, "status"), "200");
            assert_eq!(log_field(line, "code"), "-");
            assert_eq!(log_field(line, "bytes_in"), "7");
            assert_eq!(log_field(line, "bytes_out"), "11");
            assert_eq!(log_field(line, "outcome"), "complete");
            log_field(line, "duration_ms").parse::<u64>().unwrap();
            // Never headers, bodies, queries or pairings.
            let everything = logs.text();
            for secret in [
                &*session.pairing,
                PAIRING,
                "beta=true",
                "real-key-by-mistake",
                "placeholder-key",
                "{\"n\":1}",
            ] {
                assert!(!everything.contains(secret), "log leaks {secret}");
            }
            proxy.abort();
            let _ = proxy.await;
            upstream_task.abort();
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sandbox_request_log_records_refusals_and_fixed_routes_log_nothing() {
        let (logs, _guard) = capture_logs();
        tokio::time::timeout(Duration::from_secs(15), async {
            let (address, _seen, upstream_task) = recording_upstream().await;
            let (dir, state) = request_source_state(address).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();

            // No session matches: session and client_id are "-".
            let wrong = post_with(
                proxy_address,
                "/sandbox/v1/messages?x=1",
                Some(PAIRING),
                b"{}",
            );
            assert_eq!(
                client.request(wrong).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
            let lines = logs.wait_for_lines(1).await;
            let line = &lines[0];
            assert_eq!(log_field(line, "session"), "-");
            assert_eq!(log_field(line, "client_id"), "-");
            assert_eq!(log_field(line, "method"), "POST");
            assert_eq!(log_field(line, "path"), "/v1/messages");
            assert_eq!(log_field(line, "status"), "403");
            assert_eq!(log_field(line, "code"), "pairing_invalid");
            assert_eq!(log_field(line, "bytes_in"), "0");

            // Refusals before the pairing check are logged too (the path split
            // moved above the Host/Origin/OPTIONS checks).
            let rebound = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{proxy_address}/sandbox/v1/messages"))
                .header(HOST, "attacker.example")
                .header(PAIRING_HEADER, &*session.pairing)
                .body(Full::new(Bytes::from_static(b"{}")))
                .unwrap();
            assert_eq!(
                client.request(rebound).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );
            let options = Request::builder()
                .method(Method::OPTIONS)
                .uri(format!("http://{proxy_address}/sandbox/v1/messages"))
                .body(Full::new(Bytes::new()))
                .unwrap();
            assert_eq!(
                client.request(options).await.unwrap().status(),
                StatusCode::METHOD_NOT_ALLOWED
            );
            // A matched session is recorded on later refusals.
            let escape = post_with(
                proxy_address,
                "/sandbox/../admin",
                Some(&session.pairing),
                b"{}",
            );
            assert_eq!(
                client.request(escape).await.unwrap().status(),
                StatusCode::BAD_REQUEST
            );
            // The fixed routes emit nothing new.
            let fixed = post_with(proxy_address, "/copilot-cli/v1/messages", None, b"{}");
            assert_eq!(
                client.request(fixed).await.unwrap().status(),
                StatusCode::FORBIDDEN
            );

            let lines = logs.wait_for_lines(4).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            let lines_later = logs.sandbox_lines();
            assert_eq!(lines_later.len(), 4, "{lines_later:?}");
            assert_eq!(log_field(&lines[1], "session"), "-");
            assert_eq!(log_field(&lines[1], "status"), "403");
            assert_eq!(log_field(&lines[1], "code"), "host_not_allowed");
            assert_eq!(log_field(&lines[1], "path"), "/v1/messages");
            assert_eq!(log_field(&lines[2], "session"), "-");
            assert_eq!(log_field(&lines[2], "status"), "405");
            assert_eq!(log_field(&lines[2], "code"), "method_not_allowed");
            assert_eq!(log_field(&lines[2], "method"), "OPTIONS");
            assert_eq!(log_field(&lines[3], "session"), session.id);
            assert_eq!(log_field(&lines[3], "client_id"), "claude-code");
            assert_eq!(log_field(&lines[3], "status"), "400");
            assert_eq!(log_field(&lines[3], "code"), "path_invalid");
            assert_eq!(log_field(&lines[3], "path"), "/../admin");
            let everything = logs.text();
            assert!(!everything.contains(&*session.pairing));
            assert!(!everything.contains("x=1"));
            proxy.abort();
            let _ = proxy.await;
            upstream_task.abort();
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sandbox_request_log_marks_a_dropped_response_as_aborted() {
        let (logs, _guard) = capture_logs();
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(move |request: Request<Incoming>| async move {
                            let _ = request.into_body().collect().await;
                            let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(2);
                            tokio::spawn(async move {
                                if tx
                                    .send(Ok(Frame::data(Bytes::from_static(b"hello"))))
                                    .await
                                    .is_err()
                                {
                                    return;
                                }
                                // The stream goes on for seconds unless the proxy
                                // drops it when the client is gone.
                                for _ in 0..100 {
                                    tokio::time::sleep(Duration::from_millis(50)).await;
                                    if tx
                                        .send(Ok(Frame::data(Bytes::from_static(b"."))))
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                            });
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .header("content-type", "text/event-stream")
                                    .body(StreamBody::new(ReceiverStream::new(rx)))
                                    .unwrap(),
                            )
                        }),
                    )
                    .await;
            });
            let (dir, state) = request_source_state(address).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();
            let request = post_with(
                proxy_address,
                "/sandbox/v1/messages",
                Some(&session.pairing),
                b"{}",
            );
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let mut body = response.into_body();
            let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
            assert_eq!(first, "hello");
            // The client goes away after the first chunk.
            drop(body);
            drop(client);
            let lines = logs.wait_for_lines(1).await;
            let line = &lines[0];
            assert_eq!(log_field(line, "session"), session.id);
            assert_eq!(log_field(line, "status"), "200");
            assert_eq!(log_field(line, "outcome"), "aborted");
            assert!(
                log_field(line, "bytes_out").parse::<u64>().unwrap() >= 5,
                "{line}"
            );
            proxy.abort();
            let _ = proxy.await;
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn sandbox_request_canceled_before_upstream_headers_still_logs() {
        let (logs, _guard) = capture_logs();
        tokio::time::timeout(Duration::from_secs(15), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(move |request: Request<Incoming>| async move {
                            let _ = request.into_body().collect().await;
                            // Never respond: the client will be aborted first.
                            tokio::time::sleep(Duration::from_secs(600)).await;
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .body(Full::new(Bytes::from_static(b"{}")))
                                    .unwrap(),
                            )
                        }),
                    )
                    .await;
            });
            let (dir, state) = request_source_state(address).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();

            // Client drops before the upstream responds: the handler future
            // is dropped and RequestLog's Drop emits with outcome=canceled.
            let canceled = tokio::spawn({
                let client = client.clone();
                let pairing = session.pairing.clone();
                async move {
                    let request =
                        post_with(proxy_address, "/sandbox/v1/messages", Some(&pairing), b"{}");
                    let _ = client.request(request).await;
                }
            });
            // Give the request time to reach the proxy and upstream.
            tokio::time::sleep(Duration::from_millis(100)).await;
            canceled.abort();
            let lines = logs.wait_for_lines(1).await;
            let line = &lines[0];
            assert_eq!(log_field(line, "session"), session.id);
            assert_eq!(log_field(line, "client_id"), "claude-code");
            assert_eq!(log_field(line, "method"), "POST");
            assert_eq!(log_field(line, "path"), "/v1/messages");
            assert_eq!(log_field(line, "status"), "0");
            assert_eq!(log_field(line, "outcome"), "canceled");
            assert_eq!(log_field(line, "bytes_out"), "0");
            // Wait a bit to ensure no duplicate line is emitted.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let lines_later = logs.sandbox_lines();
            assert_eq!(
                lines_later.len(),
                1,
                "canceled request should log exactly once"
            );

            proxy.abort();
            let _ = proxy.await;
            upstream_task.abort();
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    // When deviceAuthorization: false and no stored token exists, a sandbox
    // request with a valid pairing returns 401 (not 502) because the sandbox
    // route transforms the config to non-interactive before credential
    // acquisition. Without the transform, discovery would attempt to contact
    // the unreachable issuer and return 502. This test verifies mod.rs:1386.
    #[tokio::test]
    async fn sandbox_route_returns_401_when_oidc_device_authorization_is_false_and_no_stored_token()
    {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir().unwrap();
            // No stored token (unlike request_source_state which stores one).
            // Use an unreachable loopback HTTPS address.
            let unreachable_issuer = "https://127.0.0.1:1/";
            let config = parse_daemon(&format!(
                "llmGateway:\n  url: http://127.0.0.1:9999\n  authentication:\n    type: oidc\n    issuer: {unreachable_issuer}\n    clientId: test-client\n    deviceAuthorization: false\n    allowInsecure: true\n"
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
                    usage_receiver: None,
                    containment: None,
                },
                discovery,
                enrollment: EnrollmentState::new(false),
                state_dir: dir.path().to_owned(),
                oidc_callback_listen: None,
                telemetry: None,
                logout: None,
                usage: None,
                programs: None,
                violation_ledger: None,
            };
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();

            let request = post_with(
                proxy_address,
                "/sandbox/v1/messages",
                Some(&session.pairing),
                b"{}",
            );
            let response = client.request(request).await.unwrap();
            let (status, code, _) = refusal(response).await;
            // Must be 401 (sign-in required), not 502 (bad gateway/discovery error).
            assert_eq!(status, StatusCode::UNAUTHORIZED, "non-interactive acquisition returns 401");
            assert_eq!(code, "agentdesktop_credential", "the error is credential-related");

            proxy.abort();
            let _ = proxy.await;
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }
    // PR 4 tests: forward proxy, git relay, deny default

    #[test]
    fn git_endpoint_classification_fetch_discovery() {
        let result = classify_git_endpoint("/repo.git/info/refs", Some("service=git-upload-pack"));
        assert_eq!(result, Some(GitEndpoint::FetchDiscovery));
    }

    #[test]
    fn git_endpoint_classification_push_discovery() {
        let result = classify_git_endpoint("/repo.git/info/refs", Some("service=git-receive-pack"));
        assert_eq!(result, Some(GitEndpoint::PushDiscovery));
    }

    #[test]
    fn git_endpoint_classification_fetch_rpc() {
        let result = classify_git_endpoint("/repo.git/git-upload-pack", None);
        assert_eq!(result, Some(GitEndpoint::FetchRpc));
    }

    #[test]
    fn git_endpoint_classification_push_rpc() {
        let result = classify_git_endpoint("/repo.git/git-receive-pack", None);
        assert_eq!(result, Some(GitEndpoint::PushRpc));
    }

    #[test]
    fn git_endpoint_classification_dumb_protocol_refused() {
        let result = classify_git_endpoint("/repo.git/info/refs", None);
        assert_eq!(result, None);
    }

    #[test]
    fn git_endpoint_classification_objects_refused() {
        let result = classify_git_endpoint("/repo.git/objects/ab/cd1234", None);
        assert_eq!(result, None);
    }

    #[test]
    fn pkt_line_err_discovery_format() {
        let response = pkt_line_err("push not allowed", false);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/x-git-upload-pack-advertisement"
        );
    }

    #[test]
    fn pkt_line_err_rpc_format() {
        let response = pkt_line_err("host not allowed", true);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/x-git-upload-pack-result"
        );
    }

    #[tokio::test]
    async fn git_route_requires_pairing() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (dir, state) = request_source_state(SocketAddr::from(([127, 0, 0, 1], 0))).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            let _session = new_session(&sessions, "test-client");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();

            // Request without pairing header should fail
            let request = Request::builder()
                .method(Method::GET)
                .uri(format!(
                    "http://{}/git/github.com/org/repo.git/info/refs?service=git-upload-pack",
                    proxy_address
                ))
                .body(Full::new(Bytes::new()))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let (_, code, _) = refusal(response).await;
            assert_eq!(code, "pairing_invalid");

            proxy.abort();
            let _ = proxy.await;
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn git_route_host_not_allowed() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (dir, state) = request_source_state(SocketAddr::from(([127, 0, 0, 1], 0))).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "test-client");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();

            let mut request = Request::builder()
                .method(Method::GET)
                .uri(format!(
                    "http://{}/git/evil.com/repo.git/info/refs?service=git-upload-pack",
                    proxy_address
                ))
                .body(Full::new(Bytes::new()))
                .unwrap();
            request.headers_mut().insert(
                PAIRING_HEADER,
                HeaderValue::from_str(&session.pairing).unwrap(),
            );

            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            let (_, code, _) = refusal(response).await;
            assert_eq!(code, "git_host_not_allowed");

            proxy.abort();
            let _ = proxy.await;
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn git_leg_chains_to_the_egress_upstream() {
        tokio::time::timeout(Duration::from_secs(10), async {
            // The stub plays the central L7 egress endpoint (AC 2).
            let (upstream_address, seen, upstream_task) = recording_upstream().await;
            let (dir, state) = request_source_state(SocketAddr::from(([127, 0, 0, 1], 0))).await;
            let mut config = default_config();
            config.git = Some(agentdesktop_core::config::GitConfig {
                allow: vec!["https://github.com/Org/".to_owned()],
                push_allow: vec![],
                credentials: vec![agentdesktop_core::config::GitCredentialEntry {
                    url_prefix: "https://github.com/".to_owned(),
                    username: "git-user".to_owned(),
                    secret: "git-pat".to_owned(),
                }],
                ssh: None,
            });
            config.egress_upstream =
                Some(url::Url::parse(&format!("http://{upstream_address}/")).unwrap());
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "test-client");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();

            let mut request = Request::builder()
                .method(Method::GET)
                .uri(format!(
                    "http://{proxy_address}/git/github.com/Org/MixedCase.git/info/refs?service=git-upload-pack"
                ))
                .body(Full::new(Bytes::new()))
                .unwrap();
            request.headers_mut().insert(
                PAIRING_HEADER,
                HeaderValue::from_str(&session.pairing).unwrap(),
            );
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let seen = std::mem::take(&mut *seen.lock().unwrap());
            assert_eq!(seen.len(), 1);
            // Path case preserved, no /git prefix, no destination host in the path.
            assert_eq!(
                seen[0].uri,
                "/Org/MixedCase.git/info/refs?service=git-upload-pack"
            );
            // Host = the egress host, not the destination.
            assert_eq!(
                seen[0].headers[HOST],
                upstream_address.to_string().as_str()
            );
            // Authorization carries ONLY the git credential: no gateway-hop
            // auth header in v1 (AC 2, F-B deferral).
            let expected = format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode("git-user:git-pat")
            );
            let authorization: Vec<_> = seen[0].headers.get_all(AUTHORIZATION).iter().collect();
            assert_eq!(authorization, [&HeaderValue::from_str(&expected).unwrap()]);
            assert!(!seen[0].headers.contains_key(PAIRING_HEADER));
            assert!(!seen[0].headers.contains_key("proxy-authorization"));

            proxy.abort();
            let _ = proxy.await;
            upstream_task.abort();
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn git_leg_retries_503_exactly_once() {
        tokio::time::timeout(Duration::from_secs(10), async {
            // Statuses the stub egress upstream answers with, in order: the
            // first request is retried once to a 200; the second request
            // gets two 503s and the second one surfaces to the box.
            let (upstream_address, hits, upstream_task) =
                scripted_upstream(vec![503, 200, 503, 503]).await;
            let (dir, state) = request_source_state(SocketAddr::from(([127, 0, 0, 1], 0))).await;
            let mut config = default_config();
            config.git = Some(agentdesktop_core::config::GitConfig {
                allow: vec!["https://github.com/org/".to_owned()],
                push_allow: vec![],
                credentials: vec![],
                ssh: None,
            });
            config.egress_upstream =
                Some(url::Url::parse(&format!("http://{upstream_address}/")).unwrap());
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "test-client");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();

            let fetch = |pairing: String| {
                move || {
                    let mut request = Request::builder()
                        .method(Method::GET)
                        .uri(format!(
                            "http://{proxy_address}/git/github.com/org/repo.git/info/refs?service=git-upload-pack"
                        ))
                        .body(Full::new(Bytes::new()))
                        .unwrap();
                    request
                        .headers_mut()
                        .insert(PAIRING_HEADER, HeaderValue::from_str(&pairing).unwrap());
                    request
                }
            };
            let request = fetch(session.pairing.to_string());

            // First fetch: 503 then 200 — the retry is invisible to the box.
            let response = client.request(request()).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 2);

            // Second fetch: 503 twice — retried exactly once, then surfaced.
            let response = client.request(request()).await.unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 4);

            proxy.abort();
            let _ = proxy.await;
            upstream_task.abort();
            drop(client);
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn connect_requires_pairing_in_proxy_authorization() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (dir, state) = request_source_state(SocketAddr::from(([127, 0, 0, 1], 0))).await;
            let config = default_config();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));

            // CONNECT without Proxy-Authorization fails as pairing_invalid
            // (raw socket: hyper's client cannot send CONNECT).
            let (status, text) = raw_http(
                proxy_address,
                "CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\n\r\n".to_owned(),
            )
            .await;
            assert_eq!(status, 403, "{text}");
            assert_eq!(raw_error_code(&text), "pairing_invalid");

            // A value that is not a registered session pairing fails the
            // same way.
            let bogus = base64::engine::general_purpose::STANDARD.encode("not-a-pairing:");
            let (status, text) = raw_http(
                proxy_address,
                format!(
                    "CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\nProxy-Authorization: Basic {bogus}\r\n\r\n"
                ),
            )
            .await;
            assert_eq!(status, 403, "{text}");
            assert_eq!(raw_error_code(&text), "pairing_invalid");

            proxy.abort();
            let _ = proxy.await;
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn connect_always_refused_egress_not_supported() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (dir, state) = request_source_state(SocketAddr::from(([127, 0, 0, 1], 0))).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "test-client");

            // Encode pairing as Basic auth (pairing:)
            let pairing_auth = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:", session.pairing));

            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));

            // A paired CONNECT gets 403 egress_not_supported via the
            // ProxyError envelope — never a tunnel (raw socket: hyper's
            // client cannot send CONNECT).
            let (status, text) = raw_http(
                proxy_address,
                format!(
                    "CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\nProxy-Authorization: Basic {pairing_auth}\r\n\r\n"
                ),
            )
            .await;
            assert_eq!(status, 403, "{text}");
            assert_eq!(raw_error_code(&text), "egress_not_supported");

            proxy.abort();
            let _ = proxy.await;
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn absolute_form_always_refused_like_connect() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (dir, state) = request_source_state(SocketAddr::from(([127, 0, 0, 1], 0))).await;
            let config = default_config();
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "test-client");
            let pairing_auth = base64::engine::general_purpose::STANDARD
                .encode(format!("{}:", session.pairing));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));

            // Absolute-form (the plain-proxy request target) carries the
            // pairing like CONNECT and is always refused the same way.
            let (status, text) = raw_http(
                proxy_address,
                format!(
                    "GET http://example.com/file HTTP/1.1\r\nHost: example.com\r\nProxy-Authorization: Basic {pairing_auth}\r\n\r\n"
                ),
            )
            .await;
            assert_eq!(status, 403, "{text}");
            assert_eq!(raw_error_code(&text), "egress_not_supported");

            // Unpaired absolute-form: pairing_invalid.
            let (status, text) = raw_http(
                proxy_address,
                "GET http://example.com/file HTTP/1.1\r\nHost: example.com\r\n\r\n".to_owned(),
            )
            .await;
            assert_eq!(status, 403, "{text}");
            assert_eq!(raw_error_code(&text), "pairing_invalid");

            proxy.abort();
            let _ = proxy.await;
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[test]
    fn egress_default_is_deny() {
        use agentdesktop_core::config::ContainmentEgress;
        assert_eq!(ContainmentEgress::default(), ContainmentEgress::Deny);
    }

    #[test]
    fn validate_containment_egress_upstream_url() {
        use agentdesktop_core::config::{
            ContainmentStartupConfig, EgressUpstreamConfig, validate_containment_startup,
        };
        use std::path::PathBuf;
        use std::str::FromStr;
        use url::Url;

        let mut cfg = ContainmentStartupConfig {
            executor: PathBuf::from("C:\\wxc\\wxc-exec.exe"),
            egress: Default::default(),
            engine: Default::default(),
            egress_upstream: Some(EgressUpstreamConfig {
                url: Url::from_str("https://gateway.example.com/forward").unwrap(),
                authentication: None,
            }),
            git: None,
        };

        // Valid URL should pass
        assert!(validate_containment_startup(&cfg, true).is_ok());

        // URL with credentials should fail
        cfg.egress_upstream = Some(EgressUpstreamConfig {
            url: Url::from_str("https://user:pass@gateway.example.com/forward").unwrap(),
            authentication: None,
        });
        assert!(validate_containment_startup(&cfg, true).is_err());

        // URL with query should fail
        cfg.egress_upstream = Some(EgressUpstreamConfig {
            url: Url::from_str("https://gateway.example.com/forward?key=value").unwrap(),
            authentication: None,
        });
        assert!(validate_containment_startup(&cfg, true).is_err());

        // URL without host should fail
        cfg.egress_upstream = Some(EgressUpstreamConfig {
            url: Url::from_str("file:///path").unwrap(),
            authentication: None,
        });
        assert!(validate_containment_startup(&cfg, true).is_err());
    }

    #[test]
    fn validate_containment_git_urls_must_be_https() {
        use agentdesktop_core::config::{
            ContainmentStartupConfig, GitConfig, validate_containment_startup,
        };
        use std::path::PathBuf;

        let mut cfg = ContainmentStartupConfig {
            executor: PathBuf::from("C:\\wxc\\wxc-exec.exe"),
            egress: Default::default(),
            engine: Default::default(),
            egress_upstream: None,
            git: Some(GitConfig {
                allow: vec!["https://github.com/org/".to_string()],
                push_allow: vec!["https://github.com/org/".to_string()],
                credentials: vec![],
                ssh: None,
            }),
        };

        // HTTPS URLs should pass
        assert!(validate_containment_startup(&cfg, true).is_ok());

        // HTTP URLs in allow should fail
        cfg.git = Some(GitConfig {
            allow: vec!["http://github.com/org/".to_string()],
            push_allow: vec![],
            credentials: vec![],
            ssh: None,
        });
        assert!(validate_containment_startup(&cfg, true).is_err());

        // SSH URLs in allow should fail
        cfg.git = Some(GitConfig {
            allow: vec!["git@github.com:org/repo.git".to_string()],
            push_allow: vec![],
            credentials: vec![],
            ssh: None,
        });
        assert!(validate_containment_startup(&cfg, true).is_err());

        // HTTP URLs in pushAllow should fail
        cfg.git = Some(GitConfig {
            allow: vec!["https://github.com/org/".to_string()],
            push_allow: vec!["http://github.com/org/".to_string()],
            credentials: vec![],
            ssh: None,
        });
        assert!(validate_containment_startup(&cfg, true).is_err());
    }

    /// A launch plan for the PR 4 gitconfig tests: fixed Windows paths, the
    /// given paths existing, the given egress.
    fn pr4_plan(
        egress: agentdesktop_core::config::ContainmentEgress,
        existing: &[&str],
    ) -> crate::launch::plan::LaunchPlan {
        let existing: Vec<String> = existing.iter().map(|path| (*path).to_owned()).collect();
        let exists = |path: &std::path::Path| {
            existing
                .iter()
                .any(|entry| std::path::Path::new(entry) == path)
        };
        let is_dir = |_: &std::path::Path| false;
        let input = crate::launch::LaunchInput {
            launch_id: "0123456789abcdef".into(),
            executable: std::path::PathBuf::from(r"C:\Users\dev\.local\bin\claude.exe"),
            client_exe_dir: std::path::PathBuf::from(r"C:\lab\agentdesktop"),
            cwd: std::path::PathBuf::from(r"C:\ws"),
            args: vec![],
            home: std::path::PathBuf::from(r"C:\Users\dev"),
            state_dir: std::path::PathBuf::from(r"C:\Users\dev\state"),
            launch_dir: std::path::PathBuf::from(r"C:\Users\dev\state\launches\0123456789abcdef"),
            git_dir: None,
            program_files_claude: None,
            egress,
            relay_base_url: "http://127.0.0.1:4000/sandbox".into(),
            pairing: "test-pairing".into(),
            base_env: std::collections::BTreeMap::new(),
            ssh_enabled: false,
            exists: &exists,
            is_dir: &is_dir,
            console: crate::launch::plan::ConsolePlan::Window,
        };
        crate::launch::plan(&input, &crate::launch::profile::CLAUDE_CODE)
    }

    /// An upstream answering by path: `/org/hop<K>/...` redirects to
    /// `hop<K+1>` until K reaches `target` (then 200 "ok"); with `escape`
    /// the first redirect points outside the `/org/` prefix. Records paths.
    async fn redirect_upstream(
        target: usize,
        escape: bool,
    ) -> (
        std::net::SocketAddr,
        Arc<std::sync::Mutex<Vec<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let record = seen.clone();
        let task = tokio::spawn(async move {
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
                                    let path = request.uri().path().to_owned();
                                    record.lock().unwrap().push(path.clone());
                                    let hop: usize = path
                                        .split('/')
                                        .find_map(|segment| segment.strip_prefix("hop"))
                                        .and_then(|n| n.parse().ok())
                                        .unwrap_or(0);
                                    let response = if hop < target {
                                        let location = if escape {
                                            "/elsewhere/repo.git/info/refs?service=git-upload-pack"
                                                .to_owned()
                                        } else {
                                            format!(
                                                "/org/hop{}/info/refs?service=git-upload-pack",
                                                hop + 1
                                            )
                                        };
                                        Response::builder()
                                            .status(StatusCode::FOUND)
                                            .header("location", location)
                                            .body(Full::new(Bytes::new()))
                                            .unwrap()
                                    } else {
                                        Response::new(Full::new(Bytes::from_static(b"ok")))
                                    };
                                    Ok::<_, Infallible>(response)
                                }
                            }),
                        )
                        .await;
                });
            }
        });
        (address, seen, task)
    }

    /// A proxy whose git allowlist points at a local plain-HTTP stub: the
    /// allow entry's scheme decides the connection scheme (production entries
    /// are pinned to https by startup validation).
    async fn git_proxy(
        allow: Vec<String>,
        egress_upstream: Option<url::Url>,
    ) -> (
        tempfile::TempDir,
        std::net::SocketAddr,
        SessionRegistry,
        sessions::Session,
        tokio::task::JoinHandle<anyhow::Result<()>>,
    ) {
        let (dir, state) = request_source_state(SocketAddr::from(([127, 0, 0, 1], 0))).await;
        let mut config = default_config();
        config.git = Some(agentdesktop_core::config::GitConfig {
            allow,
            push_allow: vec![],
            credentials: vec![],
            ssh: None,
        });
        config.egress_upstream = egress_upstream;
        let sessions = config.sessions.clone();
        let session = new_session(&sessions, "claude-code");
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let proxy = tokio::spawn(serve(listener, state, config));
        (dir, proxy_address, sessions, session, proxy)
    }

    fn git_get(proxy: std::net::SocketAddr, path: &str, pairing: &str) -> Request<Full<Bytes>> {
        Request::builder()
            .method(Method::GET)
            .uri(format!("http://{proxy}{GIT_ROUTE}/{path}"))
            .header(PAIRING_HEADER, pairing)
            .body(Full::new(Bytes::new()))
            .unwrap()
    }

    #[tokio::test]
    async fn revocation_cuts_a_streaming_git_response() {
        let (logs, _guard) = capture_logs();
        tokio::time::timeout(Duration::from_secs(10), async {
            // The upstream sends the headers and a first pack chunk, then
            // holds the body open (a long transfer).
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let upstream_address = upstream.local_addr().unwrap();
            let (hold_tx, mut hold_rx) = mpsc::channel::<()>(1);
            let upstream_task = tokio::spawn(async move {
                let (socket, _) = upstream.accept().await.unwrap();
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(socket),
                        service_fn(move |_request: Request<Incoming>| {
                            let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(2);
                            async move {
                                tokio::spawn(async move {
                                    tx.send(Ok(Frame::data(Bytes::from_static(b"PACK-first")))).await.unwrap();
                                    // Never sends more; keeps the stream open.
                                    std::future::pending::<()>().await;
                                    drop(tx);
                                });
                                Ok::<_, Infallible>(
                                    Response::builder()
                                        .header("content-type", "application/x-git-upload-pack-result")
                                        .body(StreamBody::new(ReceiverStream::new(rx)))
                                        .unwrap(),
                                )
                            }
                        }),
                    )
                    .await;
                let _ = hold_rx.recv().await;
            });
            let (dir, proxy_address, sessions, session, proxy) =
                git_proxy(vec![format!("http://{upstream_address}/org/")], None).await;
            let client = http_client();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{proxy_address}{GIT_ROUTE}/{upstream_address}/org/repo.git/git-upload-pack"))
                .header(PAIRING_HEADER, &*session.pairing)
                .body(Full::new(Bytes::from_static(b"0000")))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let mut body = response.into_body();
            let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
            assert_eq!(first, "PACK-first");
            assert!(sessions.revoke(&session.id));
            // The body ends (cleanly or with an error) instead of hanging.
            let _ = body.collect().await;
            let lines = logs.wait_for_lines(1).await;
            let line = lines.last().unwrap();
            assert_eq!(log_field(line, "outcome"), "canceled", "{line}");
            assert_eq!(log_field(line, "status"), "200", "{line}");
            proxy.abort();
            drop(hold_tx);
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // Credential injection tests
    #[test]
    fn credential_injection_longest_prefix_match() {
        use agentdesktop_core::config::GitCredentialEntry;

        let credentials = vec![
            GitCredentialEntry {
                url_prefix: "https://github.com/".to_string(),
                username: "user1".to_string(),
                secret: "secret1".to_string(),
            },
            GitCredentialEntry {
                url_prefix: "https://github.com/org/".to_string(),
                username: "user2".to_string(),
                secret: "secret2".to_string(),
            },
        ];

        let url = |text: &str| url::Url::parse(text).unwrap();
        // The more specific entry wins under its prefix ...
        let picked = select_git_credential(
            &credentials,
            &url("https://github.com/org/repo.git/info/refs"),
        );
        assert_eq!(picked.map(|entry| entry.username.as_str()), Some("user2"));
        // ... the broader one everywhere else on the host ...
        let picked = select_git_credential(
            &credentials,
            &url("https://github.com/other/repo.git/info/refs"),
        );
        assert_eq!(picked.map(|entry| entry.username.as_str()), Some("user1"));
        // ... a partial path element is not the longer prefix ...
        let picked = select_git_credential(
            &credentials,
            &url("https://github.com/organization/repo.git"),
        );
        assert_eq!(picked.map(|entry| entry.username.as_str()), Some("user1"));
        // ... and the order of the entries does not matter.
        let reversed: Vec<_> = credentials.iter().rev().cloned().collect();
        let picked = select_git_credential(&reversed, &url("https://github.com/org/repo.git"));
        assert_eq!(picked.map(|entry| entry.username.as_str()), Some("user2"));
    }

    #[test]
    fn credential_injection_only_for_allowed_hosts() {
        use agentdesktop_core::config::{GitConfig, GitCredentialEntry};

        let git_config = GitConfig {
            allow: vec!["https://github.com/org/".to_string()],
            push_allow: vec![],
            credentials: vec![GitCredentialEntry {
                url_prefix: "https://github.com/".to_string(),
                username: "user".to_string(),
                secret: "secret".to_string(),
            }],
            ssh: None,
        };

        let url = |text: &str| url::Url::parse(text).unwrap();
        // Allowed destination: the (broader) credential entry applies.
        let picked = git_credential_for(
            &git_config,
            &url("https://github.com/org/repo.git/info/refs"),
        );
        assert_eq!(picked.map(|entry| entry.username.as_str()), Some("user"));
        // Same host, outside the allow prefix: the credential entry covers
        // it, but the allowlist gate comes first.
        assert!(
            git_credential_for(
                &git_config,
                &url("https://github.com/other/repo.git/info/refs")
            )
            .is_none()
        );
    }

    #[test]
    fn credential_never_injected_for_non_allowed_host() {
        use agentdesktop_core::config::{GitConfig, GitCredentialEntry};

        let git_config = GitConfig {
            allow: vec!["https://allowed.example.com/".to_string()],
            push_allow: vec![],
            credentials: vec![GitCredentialEntry {
                url_prefix: "https://evil.example.com/".to_string(),
                username: "user".to_string(),
                secret: "secret".to_string(),
            }],
            ssh: None,
        };

        let url = |text: &str| url::Url::parse(text).unwrap();
        // The credential's own host is not allowlisted: never injected.
        assert!(
            git_credential_for(
                &git_config,
                &url("https://evil.example.com/repo.git/info/refs")
            )
            .is_none()
        );
        // The allowed host has no matching credential entry: nothing injected.
        assert!(
            git_credential_for(
                &git_config,
                &url("https://allowed.example.com/repo.git/info/refs")
            )
            .is_none()
        );
        // The same holds with an egress upstream configured: the gate is the
        // destination, not the leg (handle_git_request uses this selector on
        // both legs; git_leg_chains_to_the_egress_upstream pins the chained
        // header shape).
    }

    #[test]
    fn git_credential_secret_never_serialized() {
        use agentdesktop_core::config::GitCredentialEntry;

        let entry = GitCredentialEntry {
            url_prefix: "https://github.com/".to_string(),
            username: "user".to_string(),
            secret: "supersecret".to_string(),
        };

        let json = serde_json::to_value(&entry).unwrap();
        assert!(
            json.get("secret").is_none(),
            "secret field must not be serialized"
        );
        assert_eq!(json.get("username").and_then(|v| v.as_str()), Some("user"));
    }

    // Gitconfig generation tests
    #[test]
    fn gitconfig_insteadof_rewrites_to_git_route() {
        let allow = vec![
            "https://github.com/org/".to_owned(),
            "https://git.example.com:8443/team".to_owned(),
        ];
        let contents = crate::launch::gitconfig_contents("pairing-x", 4000, &allow, &[], &[]);
        // One rewrite per allow entry, onto GIT_ROUTE on the loopback
        // listener; the authority is verbatim (port only when configured),
        // the value in git's quoted form.
        assert!(contents.contains(&format!(
            "[url \"http://127.0.0.1:4000{GIT_ROUTE}/github.com/org/\"]\n\tinsteadOf = \"https://github.com/org/\"\n"
        )), "{contents}");
        assert!(contents.contains(&format!(
            "[url \"http://127.0.0.1:4000{GIT_ROUTE}/git.example.com:8443/team\"]\n\tinsteadOf = \"https://git.example.com:8443/team\"\n"
        )), "{contents}");
        // ssh-form remotes of allowlisted hosts map to the refusal route.
        assert!(contents.contains(&format!(
            "[url \"http://127.0.0.1:4000{GIT_SSH_ROUTE}/github.com/\"]\n\tinsteadOf = ssh://github.com/\n\tinsteadOf = git@github.com:\n"
        )), "{contents}");
        // No allow entries: no rewrites at all.
        let empty = crate::launch::gitconfig_contents("pairing-x", 4000, &[], &[], &[]);
        assert!(!empty.contains("insteadOf"), "{empty}");
    }

    #[test]
    fn gitconfig_pairing_scoped_to_loopback() {
        let pairing = "secret-session-pairing-value";
        let allow = vec!["https://github.com/org/".to_owned()];
        let contents = crate::launch::gitconfig_contents(pairing, 4123, &allow, &[], &[]);
        // The pairing appears exactly once, as the extraHeader of the
        // url-matched loopback section; never in a global http section
        // (which git would send to every remote).
        assert_eq!(contents.matches(pairing).count(), 1, "{contents}");
        let section = "[http \"http://127.0.0.1:4123/\"]\n";
        let start = contents.find(section).expect("loopback http section");
        let body = &contents[start + section.len()..];
        let body = &body[..body.find("\n[").unwrap_or(body.len())];
        assert!(
            body.contains(&format!("\textraHeader = {PAIRING_HEADER}: {pairing}\n")),
            "{contents}"
        );
        assert!(!contents.contains("[http]"), "{contents}");
    }

    #[test]
    fn gitconfig_proxy_bypass() {
        let contents = crate::launch::gitconfig_contents(
            "p",
            4000,
            &["https://github.com/org/".to_owned()],
            &[],
            &[],
        );
        let section = "[http \"http://127.0.0.1:4000/\"]\n";
        let start = contents.find(section).expect("loopback http section");
        let body = &contents[start + section.len()..];
        let body = &body[..body.find("\n[").unwrap_or(body.len())];
        assert!(body.lines().any(|line| line == "\tproxy ="), "{contents}");
        // Url-scoped only: no global proxy override is written.
        assert_eq!(contents.matches("proxy =").count(), 1, "{contents}");
    }

    #[test]
    fn gitconfig_credential_helper_emptied() {
        let contents = crate::launch::gitconfig_contents(
            "p",
            4000,
            &["https://github.com/org/".to_owned()],
            &[],
            &[],
        );
        // The empty helper entry is the first (and only) credential.helper
        // value, resetting Git for Windows' system `helper=manager`.
        let section = contents
            .find("[credential]\n\thelper =\n")
            .expect("empty credential.helper");
        assert_eq!(contents.matches("helper").count(), 1, "{contents}");
        assert!(
            contents.find("[url").is_none_or(|url| section < url),
            "{contents}"
        );
    }

    #[test]
    fn gitconfig_identity_allowlist() {
        // The allowlist and which key is read with --get-all.
        assert_eq!(
            crate::launch::GIT_IDENTITY_KEYS,
            &[
                ("user.name", false),
                ("user.email", false),
                ("user.signingkey", false),
                ("commit.gpgsign", false),
                ("core.longpaths", false),
                ("safe.directory", true),
            ]
        );
        let identity = vec![
            ("user.name".to_owned(), vec!["Dev User".to_owned()]),
            ("user.email".to_owned(), vec!["dev@example.com".to_owned()]),
            ("commit.gpgsign".to_owned(), vec!["true".to_owned()]),
            (
                "safe.directory".to_owned(),
                vec!["C:/ws/a".to_owned(), "C:/ws/b".to_owned()],
            ),
        ];
        let contents = crate::launch::gitconfig_contents("p", 4000, &[], &identity, &[]);
        assert!(
            contents.contains("[user]\n\tname = \"Dev User\"\n"),
            "{contents}"
        );
        assert!(
            contents.contains("[user]\n\temail = \"dev@example.com\"\n"),
            "{contents}"
        );
        assert!(
            contents.contains("[commit]\n\tgpgsign = \"true\"\n"),
            "{contents}"
        );
        // Every value of the multi-valued key is kept.
        assert!(
            contents.contains("[safe]\n\tdirectory = \"C:/ws/a\"\n\tdirectory = \"C:/ws/b\"\n"),
            "{contents}"
        );
        // A value that could break out of its line is dropped, not written.
        let hostile = vec![(
            "user.name".to_owned(),
            vec!["x\n[alias]\n\tco = !evil".to_owned()],
        )];
        let contents = crate::launch::gitconfig_contents("p", 4000, &[], &hostile, &[]);
        assert!(!contents.contains("[alias]"), "{contents}");
    }

    #[test]
    fn gitconfig_file_lifecycle() {
        let state_dir = tempfile::tempdir().unwrap();
        let launch_dir = state_dir.path().join("launches").join("0123456789abcdef");
        crate::launch::create_launch_dir(&launch_dir).unwrap();
        let path = crate::launch::generate_gitconfig(
            &launch_dir,
            "pairing-x",
            4000,
            &["https://github.com/org/".to_owned()],
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(path, launch_dir.join("git_config"));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("pairing-x")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        // The sweep removes it secrets-first even while the box still runs
        // (the launch directory itself is kept for a live pid).
        std::fs::write(launch_dir.join("pid"), "1 2").unwrap();
        crate::launch::sweep(state_dir.path(), &|_, _| true).unwrap();
        assert!(launch_dir.is_dir());
        assert!(
            !path.exists(),
            "git_config is in the secrets-first sweep list"
        );
    }

    #[test]
    fn gitconfig_env_injection() {
        for egress in [
            agentdesktop_core::config::ContainmentEgress::Deny,
            agentdesktop_core::config::ContainmentEgress::Allow,
        ] {
            let plan = pr4_plan(egress, &[]);
            assert_eq!(
                plan.env.get("GIT_CONFIG_GLOBAL").map(String::as_str),
                Some(r"C:\Users\dev\state\launches\0123456789abcdef\git_config")
            );
            assert_eq!(
                plan.env.get("GIT_TERMINAL_PROMPT").map(String::as_str),
                Some("0")
            );
            // AC 2a (v5): no proxy environment variables.
            for key in [
                "HTTP_PROXY",
                "HTTPS_PROXY",
                "NO_PROXY",
                "http_proxy",
                "https_proxy",
                "no_proxy",
            ] {
                assert!(!plan.env.contains_key(key), "{key}");
            }
        }
    }

    #[test]
    fn gitconfig_real_paths_denied() {
        // `~/.gitconfig` is always denied (parent $HOME always exists); never granted.
        // XDG config is only denied when its parent exists (MXC parent rule).
        for existing in [
            vec![],
            vec![
                r"C:\Users\dev\.gitconfig",
                r"C:\Users\dev\.config\git\config",
            ],
        ] {
            let plan = pr4_plan(
                agentdesktop_core::config::ContainmentEgress::Deny,
                &existing,
            );
            // `~/.gitconfig` is always denied.
            assert!(
                plan.denied
                    .contains(&std::path::PathBuf::from(r"C:\Users\dev\.gitconfig")),
                "~/.gitconfig denied"
            );
            assert!(
                !plan
                    .readonly
                    .contains(&std::path::PathBuf::from(r"C:\Users\dev\.gitconfig")),
                "~/.gitconfig not granted"
            );
            // XDG config is not denied when parent doesn't exist (pr4_plan has
            // is_dir returning false, so `~/.config/git` never exists).
            assert!(
                !plan.denied.contains(&std::path::PathBuf::from(
                    r"C:\Users\dev\.config\git\config"
                )),
                "XDG config not denied when parent absent"
            );
            assert!(
                !plan.readonly.contains(&std::path::PathBuf::from(
                    r"C:\Users\dev\.config\git\config"
                )),
                "XDG config not granted"
            );
            // The generated file is granted read-only: exactly the file.
            let generated = std::path::PathBuf::from(
                r"C:\Users\dev\state\launches\0123456789abcdef\git_config",
            );
            assert!(plan.readonly.contains(&generated));
            assert!(!plan.readonly.iter().any(|path| path
                == &std::path::PathBuf::from(r"C:\Users\dev\state\launches\0123456789abcdef")));
        }
    }

    // Redirect policy tests
    #[tokio::test]
    async fn redirect_follows_in_prefix_to_3_hops() {
        // Three in-prefix redirects are followed; the 4th request answers 200.
        tokio::time::timeout(Duration::from_secs(10), async {
            let (upstream, seen, upstream_task) = redirect_upstream(3, false).await;
            let (dir, proxy_address, _sessions, session, proxy) =
                git_proxy(vec![format!("http://{upstream}/org/")], None).await;
            let client = http_client();
            let response = client
                .request(git_get(
                    proxy_address,
                    &format!("{upstream}/org/hop0/info/refs?service=git-upload-pack"),
                    &session.pairing,
                ))
                .await
                .unwrap();
            assert!(
                !response.status().is_redirection(),
                "a 3xx is never relayed"
            );
            assert!(!response.headers().contains_key("location"));
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "ok"
            );
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 4, "{seen:?}");
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn redirect_refuses_4th_hop() {
        // A 4th redirect is refused, not followed (4 upstream requests, no 5th).
        tokio::time::timeout(Duration::from_secs(10), async {
            let (upstream, seen, upstream_task) = redirect_upstream(4, false).await;
            let (dir, proxy_address, _sessions, session, proxy) =
                git_proxy(vec![format!("http://{upstream}/org/")], None).await;
            let client = http_client();
            let response = client
                .request(git_get(
                    proxy_address,
                    &format!("{upstream}/org/hop0/info/refs?service=git-upload-pack"),
                    &session.pairing,
                ))
                .await
                .unwrap();
            assert!(
                !response.status().is_redirection(),
                "a 3xx is never relayed"
            );
            assert!(!response.headers().contains_key("location"));
            let (status, code, _) = refusal(response).await;
            assert_eq!(
                (status, code.as_str()),
                (StatusCode::FORBIDDEN, "git_host_not_allowed")
            );
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 4, "{seen:?}");
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn redirect_refuses_out_of_prefix() {
        // A redirect leaving the allow entry's prefix is refused, never followed.
        tokio::time::timeout(Duration::from_secs(10), async {
            let (upstream, seen, upstream_task) = redirect_upstream(1, true).await;
            let (dir, proxy_address, _sessions, session, proxy) =
                git_proxy(vec![format!("http://{upstream}/org/")], None).await;
            let client = http_client();
            let response = client
                .request(git_get(
                    proxy_address,
                    &format!("{upstream}/org/hop0/info/refs?service=git-upload-pack"),
                    &session.pairing,
                ))
                .await
                .unwrap();
            assert!(
                !response.status().is_redirection(),
                "a 3xx is never relayed"
            );
            assert!(!response.headers().contains_key("location"));
            let (status, code, _) = refusal(response).await;
            assert_eq!(
                (status, code.as_str()),
                (StatusCode::FORBIDDEN, "git_host_not_allowed")
            );
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "{seen:?}");
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn redirect_never_relayed_to_box() {
        // One redirect: the box sees only the followed 200.
        tokio::time::timeout(Duration::from_secs(10), async {
            let (upstream, seen, upstream_task) = redirect_upstream(1, false).await;
            let (dir, proxy_address, _sessions, session, proxy) =
                git_proxy(vec![format!("http://{upstream}/org/")], None).await;
            let client = http_client();
            let response = client
                .request(git_get(
                    proxy_address,
                    &format!("{upstream}/org/hop0/info/refs?service=git-upload-pack"),
                    &session.pairing,
                ))
                .await
                .unwrap();
            assert!(
                !response.status().is_redirection(),
                "a 3xx is never relayed"
            );
            assert!(!response.headers().contains_key("location"));
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                "ok"
            );
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 2, "{seen:?}");
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // Revocation tests
    #[tokio::test]
    async fn revocation_aborts_inflight_git_request() {
        let (logs, _guard) = capture_logs();
        tokio::time::timeout(Duration::from_secs(10), async {
            // An upstream that accepts and never answers: the git request is
            // in flight until revocation aborts it.
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let upstream_address = upstream.local_addr().unwrap();
            let upstream_task = tokio::spawn(async move {
                let mut held = Vec::new();
                loop {
                    let (socket, _) = upstream.accept().await.unwrap();
                    held.push(socket);
                }
            });
            let (dir, proxy_address, sessions, session, proxy) =
                git_proxy(vec![format!("http://{upstream_address}/org/")], None).await;
            let client = http_client();
            let pending = tokio::spawn({
                let client = client.clone();
                let request = Request::builder()
                    .method(Method::POST)
                    .uri(format!("http://{proxy_address}{GIT_ROUTE}/{upstream_address}/org/repo.git/git-upload-pack"))
                    .header(PAIRING_HEADER, &*session.pairing)
                    .body(Full::new(Bytes::from_static(b"0000")))
                    .unwrap();
                async move { client.request(request).await.unwrap() }
            });
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(!pending.is_finished(), "the request is in flight");
            assert!(sessions.revoke(&session.id));
            let response = pending.await.unwrap();
            let (status, code, _) = refusal(response).await;
            assert_eq!((status, code.as_str()), (StatusCode::FORBIDDEN, "pairing_invalid"));
            let lines = logs.wait_for_lines(1).await;
            let line = lines.last().unwrap();
            assert_eq!(log_field(line, "outcome"), "canceled", "{line}");
            assert_eq!(log_field(line, "session"), session.id, "{line}");
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(logs.sandbox_lines().len(), 1, "exactly one line for the aborted request");
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // Log shape tests
    #[tokio::test]
    async fn git_request_log_has_host_and_decision() {
        let (logs, _guard) = capture_logs();
        tokio::time::timeout(Duration::from_secs(10), async {
            let client = http_client();
            // allow (direct leg) and refuse, on one proxy.
            let (direct, _seen, direct_task) = recording_upstream().await;
            let (dir, proxy_address, _sessions, session, proxy) =
                git_proxy(vec![format!("http://{direct}/org/")], None).await;
            let path = format!("{direct}/org/repo.git/info/refs?service=git-upload-pack");
            let response = client
                .request(git_get(proxy_address, &path, &session.pairing))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let _ = response.into_body().collect().await;
            let lines = logs.wait_for_lines(1).await;
            assert_eq!(
                log_field(&lines[0], "host"),
                direct.to_string(),
                "{}",
                lines[0]
            );
            assert_eq!(log_field(&lines[0], "decision"), "allow", "{}", lines[0]);
            assert_eq!(log_field(&lines[0], "status"), "200", "{}", lines[0]);
            assert_eq!(log_field(&lines[0], "session"), session.id, "{}", lines[0]);
            assert_eq!(
                lines[0].matches(" session=").count(),
                1,
                "session id once: {}",
                lines[0]
            );
            let path = "evil.com/repo.git/info/refs?service=git-upload-pack";
            let response = client
                .request(git_get(proxy_address, path, &session.pairing))
                .await
                .unwrap();
            assert_eq!(refusal(response).await.1, "git_host_not_allowed");
            let lines = logs.wait_for_lines(2).await;
            assert_eq!(log_field(&lines[1], "host"), "evil.com", "{}", lines[1]);
            assert_eq!(log_field(&lines[1], "decision"), "refuse", "{}", lines[1]);
            assert_eq!(
                log_field(&lines[1], "code"),
                "git_host_not_allowed",
                "{}",
                lines[1]
            );
            // The query string never reaches the log.
            assert!(!lines[1].contains("service="), "{}", lines[1]);
            proxy.abort();
            direct_task.abort();
            drop(dir);

            // upstream (chained leg).
            let (egress, _seen, egress_task) = recording_upstream().await;
            let (dir, proxy_address, _sessions, session, proxy) = git_proxy(
                vec!["https://github.com/org/".to_owned()],
                Some(url::Url::parse(&format!("http://{egress}/")).unwrap()),
            )
            .await;
            let path = "github.com/org/repo.git/info/refs?service=git-upload-pack";
            let response = client
                .request(git_get(proxy_address, path, &session.pairing))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let _ = response.into_body().collect().await;
            let lines = logs.wait_for_lines(3).await;
            assert_eq!(log_field(&lines[2], "host"), "github.com", "{}", lines[2]);
            assert_eq!(log_field(&lines[2], "decision"), "upstream", "{}", lines[2]);
            proxy.abort();
            egress_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn connect_request_log_has_host_and_decision() {
        let (logs, _guard) = capture_logs();
        tokio::time::timeout(Duration::from_secs(10), async {
            let (dir, state) = request_source_state(SocketAddr::from(([127, 0, 0, 1], 0))).await;
            let config = default_config();
            let session = new_session(&config.sessions, "claude-code");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let auth = base64::engine::general_purpose::STANDARD.encode(format!("{}:", session.pairing));
            let (status, _) = raw_http(
                proxy_address,
                format!("CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\nProxy-Authorization: Basic {auth}\r\n\r\n"),
            )
            .await;
            assert_eq!(status, 403);
            let lines = logs.wait_for_lines(1).await;
            let line = &lines[0];
            assert_eq!(log_field(line, "method"), "CONNECT", "{line}");
            assert_eq!(log_field(line, "host"), "github.com:443", "{line}");
            assert_eq!(log_field(line, "decision"), "refuse", "{line}");
            assert_eq!(log_field(line, "code"), "egress_not_supported", "{line}");
            assert_eq!(log_field(line, "session"), session.id, "{line}");
            // The pairing never reaches the log.
            assert!(!logs.text().contains(&*session.pairing));
            assert!(!logs.text().contains(&auth));
            proxy.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    /// PR 5 AC 6: the refusal route keeps serving ssh-form remotes of
    /// `git.allow` hosts that are not ssh hosts, with the new message and no
    /// stale PR reference, for fetch and push alike.
    #[tokio::test]
    async fn ssh_refusal_route_names_the_ssh_hosts_setting() {
        const NEW_MESSAGE: &str = "ERR ssh remotes are not enabled for this host; use the https remote or add the host to daemon.containment.git.ssh.hosts";
        tokio::time::timeout(Duration::from_secs(10), async {
            let client = http_client();
            let (direct, _seen, direct_task) = recording_upstream().await;
            let (dir, proxy_address, _sessions, session, proxy) =
                git_proxy(vec![format!("http://{direct}/org/")], None).await;
            for service in ["git-upload-pack", "git-receive-pack"] {
                let ssh = Request::builder()
                    .method(Method::GET)
                    .uri(format!("http://{proxy_address}{GIT_SSH_ROUTE}/github.com/org/repo.git/info/refs?service={service}"))
                    .header(PAIRING_HEADER, &*session.pairing)
                    .body(Full::new(Bytes::new()))
                    .unwrap();
                let response = client.request(ssh).await.unwrap();
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let body = String::from_utf8_lossy(&body);
                assert!(body.contains(NEW_MESSAGE), "{service}: {body}");
                assert!(!body.contains("PR 5"), "{service}: {body}");
            }
            proxy.abort();
            direct_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn log_outcome_values_unchanged() {
        let (logs, _guard) = capture_logs();
        tokio::time::timeout(Duration::from_secs(10), async {
            let client = http_client();
            let (direct, _seen, direct_task) = recording_upstream().await;
            let (dir, proxy_address, _sessions, session, proxy) =
                git_proxy(vec![format!("http://{direct}/org/")], None).await;
            // allowed fetch, refused host, refused dumb protocol, refused push
            // (pkt-line ERR), ssh refusal route, CONNECT refusal.
            for path in [
                format!("{direct}/org/repo.git/info/refs?service=git-upload-pack"),
                "evil.com/repo.git/info/refs?service=git-upload-pack".to_owned(),
                format!("{direct}/org/repo.git/info/refs"),
                format!("{direct}/org/repo.git/info/refs?service=git-receive-pack"),
            ] {
                let response = client.request(git_get(proxy_address, &path, &session.pairing)).await.unwrap();
                let _ = response.into_body().collect().await;
            }
            let ssh = Request::builder()
                .method(Method::GET)
                .uri(format!("http://{proxy_address}{GIT_SSH_ROUTE}/github.com/org/repo.git/info/refs?service=git-upload-pack"))
                .header(PAIRING_HEADER, &*session.pairing)
                .body(Full::new(Bytes::new()))
                .unwrap();
            let response = client.request(ssh).await.unwrap();
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(String::from_utf8_lossy(&body).contains("ERR ssh remotes are not enabled for this host; use the https remote or add the host to daemon.containment.git.ssh.hosts"));
            let auth = base64::engine::general_purpose::STANDARD.encode(format!("{}:", session.pairing));
            raw_http(
                proxy_address,
                format!("CONNECT github.com:443 HTTP/1.1\r\nHost: github.com:443\r\nProxy-Authorization: Basic {auth}\r\n\r\n"),
            )
            .await;
            let lines = logs.wait_for_lines(6).await;
            for line in &lines {
                let outcome = log_field(line, "outcome");
                assert!(["complete", "aborted", "canceled"].contains(&outcome.as_str()), "{line}");
                let decision = log_field(line, "decision");
                assert!(["allow", "refuse", "upstream"].contains(&decision.as_str()), "{line}");
            }
            proxy.abort();
            direct_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // Default flip warn test
    #[tokio::test]
    async fn explicit_egress_allow_warns_at_launch() {
        let (logs, _guard) = capture_logs();
        // The launch path calls this once per launch (start_launch); deny,
        // the default, is silent.
        crate::launch::warn_on_open_egress(agentdesktop_core::config::ContainmentEgress::Deny);
        assert!(!logs.text().contains("egress allow"), "{}", logs.text());
        crate::launch::warn_on_open_egress(agentdesktop_core::config::ContainmentEgress::Allow);
        let text = logs.text();
        let line = text
            .lines()
            .find(|line| line.contains("egress allow: the box has unrestricted outbound"))
            .unwrap_or_else(|| panic!("no warn line in {text}"));
        assert!(line.contains("WARN"), "{line}");
        assert_eq!(text.matches("egress allow:").count(), 1);
    }

    // Prefix edge cases from AC 5
    #[test]
    fn prefix_matching_exact_on_configured() {
        let url = |text: &str| url::Url::parse(text).unwrap();
        let prefix = "https://github.com/org/";
        assert!(git_prefix_matches(
            prefix,
            &url("https://github.com/org/repo.git/info/refs")
        ));
        // A partial path element is not a match ...
        assert!(!git_prefix_matches(
            prefix,
            &url("https://github.com/organization/repo.git")
        ));
        // ... nor is the prefix appearing deeper in the path (a repo segment).
        assert!(!git_prefix_matches(
            prefix,
            &url("https://github.com/other/org/repo.git")
        ));
        // Path matching is case-sensitive, like the gateway's PathPrefix.
        assert!(!git_prefix_matches(
            prefix,
            &url("https://github.com/Org/repo.git")
        ));
        // Another host never matches.
        assert!(!git_prefix_matches(
            prefix,
            &url("https://github.com.evil.example/org/repo.git")
        ));
    }

    #[test]
    fn prefix_matching_case_insensitive_host() {
        let url = |text: &str| url::Url::parse(text).unwrap();
        assert!(git_prefix_matches(
            "https://GitHub.COM/org/",
            &url("https://github.com/org/repo.git")
        ));
        assert!(git_prefix_matches(
            "https://github.com/org/",
            &url("https://GITHUB.com/org/repo.git")
        ));
    }

    #[test]
    fn prefix_matching_default_port_handled() {
        let url = |text: &str| url::Url::parse(text).unwrap();
        assert!(git_prefix_matches(
            "https://github.com:443/org/",
            &url("https://github.com/org/repo.git")
        ));
        assert!(git_prefix_matches(
            "https://github.com/org/",
            &url("https://github.com:443/org/repo.git")
        ));
        assert!(!git_prefix_matches(
            "https://github.com:8443/org/",
            &url("https://github.com/org/repo.git")
        ));
        assert!(!git_prefix_matches(
            "https://github.com/org/",
            &url("https://github.com:8443/org/repo.git")
        ));
        assert!(git_prefix_matches(
            "https://git.example.com:8443/team/",
            &url("https://git.example.com:8443/team/x.git")
        ));
    }

    #[test]
    fn prefix_matching_trailing_slash_normalized() {
        let url = |text: &str| url::Url::parse(text).unwrap();
        for prefix in ["https://github.com/org", "https://github.com/org/"] {
            assert!(
                git_prefix_matches(prefix, &url("https://github.com/org/repo.git")),
                "{prefix}"
            );
            assert!(
                git_prefix_matches(prefix, &url("https://github.com/org")),
                "{prefix}"
            );
            assert!(
                git_prefix_matches(prefix, &url("https://github.com/org/")),
                "{prefix}"
            );
            assert!(
                !git_prefix_matches(prefix, &url("https://github.com/orgx/repo.git")),
                "{prefix}"
            );
        }
    }
    // Delta fix-round tests (2026-10-21)

    // T1a (F1): a redirect may not change the allow entry's scheme.
    #[test]
    fn git_redirect_stays_in_requires_same_scheme() {
        let url = |text: &str| url::Url::parse(text).unwrap();
        assert!(!git_redirect_stays_in(
            "https://github.com/org/",
            &url("http://github.com:443/org/r.git/info/refs")
        ));
        assert!(!git_redirect_stays_in(
            "https://git.corp:8443/org/",
            &url("http://git.corp:8443/org/r")
        ));
        assert!(git_redirect_stays_in(
            "https://github.com/org/",
            &url("https://github.com/org/r.git/info/refs")
        ));
        assert!(!git_redirect_stays_in(
            "https://github.com/org/",
            &url("https://github.com/other/r.git")
        ));
    }

    // T1b (F1): a scheme-changing redirect is refused end to end, never
    // followed (at 833e878c it was followed: 502 with the https hop attempted).
    #[tokio::test]
    async fn git_redirect_scheme_change_refused() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let stub = upstream.local_addr().unwrap();
            let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter = hits.clone();
            let upstream_task = tokio::spawn(async move {
                loop {
                    let (socket, _) = upstream.accept().await.unwrap();
                    let counter = counter.clone();
                    tokio::spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                TokioIo::new(socket),
                                service_fn(move |_request: Request<Incoming>| {
                                    let counter = counter.clone();
                                    async move {
                                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                        Ok::<_, Infallible>(
                                            Response::builder()
                                                .status(StatusCode::FOUND)
                                                .header(
                                                    "location",
                                                    format!("https://{stub}/org/r.git/info/refs?service=git-upload-pack"),
                                                )
                                                .body(Full::new(Bytes::new()))
                                                .unwrap(),
                                        )
                                    }
                                }),
                            )
                            .await;
                    });
                }
            });
            let (dir, proxy_address, _sessions, session, proxy) =
                git_proxy(vec![format!("http://{stub}/org/")], None).await;
            let client = http_client();
            let response = client
                .request(git_get(
                    proxy_address,
                    &format!("{stub}/org/r.git/info/refs?service=git-upload-pack"),
                    &session.pairing,
                ))
                .await
                .unwrap();
            let (status, code, _) = refusal(response).await;
            assert_eq!(
                (status, code.as_str()),
                (StatusCode::FORBIDDEN, "git_host_not_allowed")
            );
            assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // T2a (F3, adv M3): a push to a fetch-only remote answers git's own
    // pkt-line ERR envelope on both legs, and nothing reaches the upstream.
    #[tokio::test]
    async fn push_to_fetch_only_remote_answers_pkt_line_err() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (up, seen, upstream_task) = recording_upstream().await;
            let (dir, proxy_address, _sessions, session, proxy) =
                git_proxy(vec![format!("http://{up}/org/")], None).await;
            let client = http_client();
            let line =
                format!("ERR push to {up} is not allowed by daemon.containment.git.pushAllow\n");
            let expected = format!("{:04x}{line}", line.len() + 4);
            // Discovery leg.
            let response = client
                .request(git_get(
                    proxy_address,
                    &format!("{up}/org/r.git/info/refs?service=git-receive-pack"),
                    &session.pairing,
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[CONTENT_TYPE],
                "application/x-git-receive-pack-advertisement"
            );
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                expected.as_bytes()
            );
            // RPC leg.
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}{GIT_ROUTE}/{up}/org/r.git/git-receive-pack"
                ))
                .header(PAIRING_HEADER, &*session.pairing)
                .body(Full::new(Bytes::from_static(b"0000")))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers()[CONTENT_TYPE],
                "application/x-git-receive-pack-result"
            );
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                expected.as_bytes()
            );
            assert_eq!(seen.lock().unwrap().len(), 0);
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // T2b (F3): a pushAllow-listed remote's push RPC reaches the upstream.
    #[tokio::test]
    async fn push_allowed_remote_reaches_upstream() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (up, seen, upstream_task) = recording_upstream().await;
            let (dir, state) = request_source_state(SocketAddr::from(([127, 0, 0, 1], 0))).await;
            let mut config = default_config();
            config.git = Some(agentdesktop_core::config::GitConfig {
                allow: vec![format!("http://{up}/org/")],
                push_allow: vec![format!("http://{up}/org/")],
                credentials: vec![],
                ssh: None,
            });
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "test-client");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}{GIT_ROUTE}/{up}/org/r.git/git-receive-pack"
                ))
                .header(PAIRING_HEADER, &*session.pairing)
                .body(Full::new(Bytes::from_static(b"0000")))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let seen = seen.lock().unwrap().clone();
            assert_eq!(
                seen.len(),
                1,
                "{:?}",
                seen.iter().map(|s| &s.uri).collect::<Vec<_>>()
            );
            assert_eq!(seen[0].uri, "/org/r.git/git-receive-pack");
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // T3 (F2, adv M1): gitconfig values are written in git's quoted form, so
    // Windows paths, `;` and `"` survive git's value parser.
    #[test]
    fn gitconfig_values_are_quoted() {
        let identity = vec![
            (
                "user.signingkey".to_owned(),
                vec![r"C:\Users\dev\.ssh\id.pub".to_owned()],
            ),
            ("user.name".to_owned(), vec!["Max ; Mustermann".to_owned()]),
            ("user.email".to_owned(), vec![r#"a"b@x"#.to_owned()]),
        ];
        let contents = crate::launch::gitconfig_contents("p", 4000, &[], &identity, &[]);
        assert!(
            contents.contains(r#"signingkey = "C:\\Users\\dev\\.ssh\\id.pub""#),
            "{contents}"
        );
        assert!(
            contents.contains(r#"name = "Max ; Mustermann""#),
            "{contents}"
        );
        assert!(contents.contains(r#"email = "a\"b@x""#), "{contents}");
    }

    // T4 (F4, adv minor 8): a host-only allow entry maps without a `//` path.
    #[test]
    fn gitconfig_host_only_entry_has_no_double_slash() {
        let allow = vec!["https://github.com".to_owned()];
        let contents = crate::launch::gitconfig_contents("p", 4000, &allow, &[], &[]);
        assert!(
            contents.contains(&format!(
                "[url \"http://127.0.0.1:4000{GIT_ROUTE}/github.com\"]\n\tinsteadOf = \"https://github.com\"\n"
            )),
            "{contents}"
        );
        assert!(
            !contents.contains(&format!("{GIT_ROUTE}/github.com/\"")),
            "{contents}"
        );
    }

    // T7 (M5b): the longest-prefix credential is selected per destination,
    // end to end on the chained leg.
    #[tokio::test]
    async fn credential_selected_per_prefix_end_to_end() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (up, seen, upstream_task) = recording_upstream().await;
            let (dir, state) = request_source_state(SocketAddr::from(([127, 0, 0, 1], 0))).await;
            let mut config = default_config();
            config.git = Some(agentdesktop_core::config::GitConfig {
                allow: vec![
                    "https://github.com/org/".to_owned(),
                    "https://github.com/team/".to_owned(),
                ],
                push_allow: vec![],
                credentials: vec![
                    agentdesktop_core::config::GitCredentialEntry {
                        url_prefix: "https://github.com/org/".to_owned(),
                        username: "u1".to_owned(),
                        secret: "s1".to_owned(),
                    },
                    agentdesktop_core::config::GitCredentialEntry {
                        url_prefix: "https://github.com/org/deep/".to_owned(),
                        username: "u2".to_owned(),
                        secret: "s2".to_owned(),
                    },
                ],
                ssh: None,
            });
            config.egress_upstream = Some(url::Url::parse(&format!("http://{up}/")).unwrap());
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "test-client");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();
            for path in [
                "github.com/team/r.git/info/refs?service=git-upload-pack",
                "github.com/org/r.git/info/refs?service=git-upload-pack",
                "github.com/org/deep/r.git/info/refs?service=git-upload-pack",
            ] {
                let response = client
                    .request(git_get(proxy_address, path, &session.pairing))
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{path}");
            }
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 3);
            let basic = |user: &str, secret: &str| {
                format!(
                    "Basic {}",
                    base64::engine::general_purpose::STANDARD.encode(format!("{user}:{secret}"))
                )
            };
            assert!(
                !seen[0].headers.contains_key(AUTHORIZATION),
                "no entry matches /team/"
            );
            assert_eq!(seen[1].headers[AUTHORIZATION], basic("u1", "s1").as_str());
            assert_eq!(seen[2].headers[AUTHORIZATION], basic("u2", "s2").as_str());
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // T8 (M7b): a redirect into ANOTHER allow entry's prefix is still
    // refused — the gate is the matched entry, not the whole allowlist.
    #[tokio::test]
    async fn redirect_refuses_other_allow_entry() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (up, seen, upstream_task) = redirect_upstream(1, true).await;
            let (dir, proxy_address, _sessions, session, proxy) = git_proxy(
                vec![
                    format!("http://{up}/org/"),
                    format!("http://{up}/elsewhere/"),
                ],
                None,
            )
            .await;
            let client = http_client();
            let response = client
                .request(git_get(
                    proxy_address,
                    &format!("{up}/org/hop0/info/refs?service=git-upload-pack"),
                    &session.pairing,
                ))
                .await
                .unwrap();
            let (status, code, _) = refusal(response).await;
            assert_eq!(
                (status, code.as_str()),
                (StatusCode::FORBIDDEN, "git_host_not_allowed")
            );
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1, "{seen:?}");
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // T9 (M14): an Authorization header the box sends is never forwarded.
    #[tokio::test]
    async fn box_authorization_never_forwarded() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (up, seen, upstream_task) = recording_upstream().await;
            let (dir, proxy_address, _sessions, session, proxy) =
                git_proxy(vec![format!("http://{up}/org/")], None).await;
            let client = http_client();
            let mut request = git_get(
                proxy_address,
                &format!("{up}/org/r.git/info/refs?service=git-upload-pack"),
                &session.pairing,
            );
            request.headers_mut().insert(
                AUTHORIZATION,
                HeaderValue::from_static("Basic Ym94OmNyZWQ="),
            );
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1);
            assert!(
                !seen[0].headers.contains_key(AUTHORIZATION),
                "{:?}",
                seen[0].headers
            );
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // T10 (M27): the loopback Host gate still guards the git route.
    #[tokio::test]
    async fn git_route_keeps_host_loopback_gate() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (up, seen, upstream_task) = recording_upstream().await;
            let (dir, proxy_address, _sessions, session, proxy) =
                git_proxy(vec![format!("http://{up}/org/")], None).await;
            let client = http_client();
            let mut request = git_get(
                proxy_address,
                &format!("{up}/org/r.git/info/refs?service=git-upload-pack"),
                &session.pairing,
            );
            request
                .headers_mut()
                .insert(HOST, HeaderValue::from_static("evil.example"));
            let response = client.request(request).await.unwrap();
            let (status, code, _) = refusal(response).await;
            assert_eq!(
                (status, code.as_str()),
                (StatusCode::FORBIDDEN, "host_not_allowed")
            );
            assert_eq!(seen.lock().unwrap().len(), 0);
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // T11 (M28b): a git request body over the cap is refused before any
    // upstream contact.
    #[tokio::test]
    async fn git_body_over_cap_refused() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (up, seen, upstream_task) = recording_upstream().await;
            let (dir, proxy_address, _sessions, session, proxy) =
                git_proxy(vec![format!("http://{up}/org/")], None).await;
            let client = http_client();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}{GIT_ROUTE}/{up}/org/r.git/git-upload-pack"
                ))
                .header(PAIRING_HEADER, &*session.pairing)
                .body(Full::new(Bytes::from(vec![b'0'; MAX_REQUEST_BODY + 1])))
                .unwrap();
            let response = client.request(request).await.unwrap();
            let (status, code, _) = refusal(response).await;
            assert_eq!(
                (status, code.as_str()),
                (StatusCode::PAYLOAD_TOO_LARGE, "body_too_large")
            );
            assert_eq!(seen.lock().unwrap().len(), 0);
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // T12 (M23): DV-6 — a smart-HTTP path with the wrong method is refused.
    #[tokio::test]
    async fn git_wrong_method_refused() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (up, seen, upstream_task) = recording_upstream().await;
            let (dir, proxy_address, _sessions, session, proxy) =
                git_proxy(vec![format!("http://{up}/org/")], None).await;
            let client = http_client();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}{GIT_ROUTE}/{up}/org/r.git/info/refs?service=git-upload-pack"
                ))
                .header(PAIRING_HEADER, &*session.pairing)
                .body(Full::new(Bytes::from_static(b"0000")))
                .unwrap();
            let response = client.request(request).await.unwrap();
            let (status, code, _) = refusal(response).await;
            assert_eq!(
                (status, code.as_str()),
                (StatusCode::FORBIDDEN, "git_host_not_allowed")
            );
            assert_eq!(seen.lock().unwrap().len(), 0);
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // T13 (F7, DV-7): a push RPC's 503 is never retried — it surfaces on the
    // first answer and the upstream saw exactly one request.
    #[tokio::test]
    async fn push_rpc_503_is_not_retried() {
        tokio::time::timeout(Duration::from_secs(10), async {
            let (up, hits, upstream_task) = scripted_upstream(vec![503, 200]).await;
            let (dir, state) = request_source_state(SocketAddr::from(([127, 0, 0, 1], 0))).await;
            let mut config = default_config();
            config.git = Some(agentdesktop_core::config::GitConfig {
                allow: vec!["https://github.com/org/".to_owned()],
                push_allow: vec!["https://github.com/org/".to_owned()],
                credentials: vec![],
                ssh: None,
            });
            config.egress_upstream = Some(url::Url::parse(&format!("http://{up}/")).unwrap());
            let sessions = config.sessions.clone();
            let session = new_session(&sessions, "test-client");
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let proxy_address = listener.local_addr().unwrap();
            let proxy = tokio::spawn(serve(listener, state, config));
            let client = http_client();
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}{GIT_ROUTE}/github.com/org/r.git/git-receive-pack"
                ))
                .header(PAIRING_HEADER, &*session.pairing)
                .body(Full::new(Bytes::from_static(b"0000")))
                .unwrap();
            let response = client.request(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }

    // T14 (F6, adv minor 10): a session revoked while the request body is
    // still being collected sends nothing upstream. This pins the revocation
    // watch during body collect; the `biased` ordering itself is not
    // observable from the current-thread test runtime.
    /// A request body streamed from an mpsc channel (T14).
    type ChannelBody = StreamBody<ReceiverStream<Result<Frame<Bytes>, Infallible>>>;

    #[tokio::test]
    async fn revocation_during_body_upload_sends_nothing_upstream() {
        let (logs, _guard) = capture_logs();
        tokio::time::timeout(Duration::from_secs(10), async {
            let (up, seen, upstream_task) = recording_upstream().await;
            let (dir, proxy_address, sessions, session, proxy) =
                git_proxy(vec![format!("http://{up}/org/")], None).await;
            let client: Client<HttpConnector, ChannelBody> =
                Client::builder(TokioExecutor::new()).build_http();
            let (tx, rx) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(2);
            let request = Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "http://{proxy_address}{GIT_ROUTE}/{up}/org/r.git/git-upload-pack"
                ))
                .header(PAIRING_HEADER, &*session.pairing)
                .body(StreamBody::new(ReceiverStream::new(rx)))
                .unwrap();
            let pending = tokio::spawn({
                let client = client.clone();
                async move { client.request(request).await.unwrap() }
            });
            tx.send(Ok(Frame::data(Bytes::from_static(b"0032want "))))
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert!(sessions.revoke(&session.id));
            let _ = tx.send(Ok(Frame::data(Bytes::from_static(b"0000")))).await;
            drop(tx);
            let response = pending.await.unwrap();
            let (status, code, _) = refusal(response).await;
            assert_eq!(
                (status, code.as_str()),
                (StatusCode::FORBIDDEN, "pairing_invalid")
            );
            assert_eq!(
                seen.lock().unwrap().len(),
                0,
                "nothing reached the upstream"
            );
            let lines = logs.wait_for_lines(1).await;
            let line = lines.last().unwrap();
            assert_eq!(log_field(line, "outcome"), "canceled", "{line}");
            proxy.abort();
            upstream_task.abort();
            drop(dir);
        })
        .await
        .unwrap();
    }
}
