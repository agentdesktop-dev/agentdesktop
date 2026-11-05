use agentdesktop_core::config::LlmGatewayConfig;
use hyper::{
    HeaderMap,
    header::{AUTHORIZATION, HOST},
};

/// Header that carries the per-device pairing value on every route.
pub(crate) const PAIRING_HEADER: &str = "x-agentdesktop-pairing";

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
pub(crate) const CODEX_ROUTE: &str = "/codex";

/// Routes for the managed programs. A prefix matches only at a `/` boundary, so
/// `/vscode-copilot-passthrough/...` never matches `/vscode-copilot`.
pub(crate) const ROUTES: &[ProxyRoute] = &[
    ProxyRoute {
        prefix: CODEX_ROUTE,
        client_id: "codex",
        credential: RouteCredential::Passthrough,
        upstream: Upstream::ProxyUrl,
        pairing_in_path: false,
        tunnels_upgrades: false,
        credential_less_ping: false,
    },
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

/// Only loopback hosts: a page on another origin that resolves a name to
/// 127.0.0.1 (DNS rebinding) sends that name in `Host`. Accepts any loopback
/// IP (the listen validation allows all of 127.0.0.0/8 and ::1), bracketed or
/// bare IPv6, `localhost` in any case with an optional trailing dot, each with
/// or without a port. A request without `Host` (HTTP/1.0) is refused.
pub(super) fn host_is_loopback(headers: &HeaderMap) -> bool {
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
pub(super) fn path_escapes_base(path: &str) -> bool {
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
pub(super) fn resembles_route(path: &str) -> bool {
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

pub(super) fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The route for a request path, and the path with the prefix removed.
pub(super) fn match_route(path: &str) -> Option<(&'static ProxyRoute, &str)> {
    ROUTES.iter().find_map(|route| {
        let rest = path.strip_prefix(route.prefix)?;
        (rest.is_empty() || rest.starts_with('/'))
            .then_some((route, if rest.is_empty() { "/" } else { rest }))
    })
}

pub(super) fn bearer_or_header(headers: &HeaderMap, header: &str) -> Option<String> {
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

pub(super) fn upstream_base(gateway: &LlmGatewayConfig, upstream: Upstream) -> &url::Url {
    match upstream {
        Upstream::Url => &gateway.url,
        Upstream::ProxyUrl => gateway.proxy_url.as_ref().unwrap_or(&gateway.url),
    }
}
