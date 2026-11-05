mod cache;
mod routes;

pub(crate) use routes::{CAPI_ROUTE, CODEX_ROUTE, PAIRING_HEADER};
use routes::{
    RouteCredential, Upstream, bearer_or_header, constant_time_eq, host_is_loopback, match_route,
    path_escapes_base, resembles_route, split_path_pairing, upstream_base,
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
                        let effective = effective.clone();
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
mod tests;
