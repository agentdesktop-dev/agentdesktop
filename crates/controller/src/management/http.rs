//! Deployment-level HTTP safeguards, independent of authentication providers.
//!
//! Cross-origin protection lives here rather than in providers: every browser-facing
//! credential (a session cookie, or simply being on loopback) is sent by the browser
//! no matter which site triggered the request, so every provider needs it.

use std::{sync::Arc, time::Duration};

use super::auth::Error;
use axum::{
    Router,
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};

#[derive(Clone)]
/// Selected by deployment composition, never inferred from an authentication provider.
pub enum Policy {
    Local,
    /// `origin` is the exact external origin browsers use, e.g. `https://admin.example.com`.
    Authenticated {
        origin: Arc<str>,
    },
}

impl Policy {
    /// Wrap all routes, including deliberately public endpoints.
    pub fn apply(&self, router: Router) -> Router {
        router.layer(middleware::from_fn_with_state(
            self.clone(),
            safeguard_request,
        ))
    }

    /// Rejects unsafe requests a browser sent on behalf of another site, using
    /// Fetch Metadata and `Origin` (no tokens). Requests with neither header are not
    /// from a modern browser and are allowed, unless they carry cookies.
    fn check_origin(&self, request: &Request) -> Result<(), &'static str> {
        if matches!(
            *request.method(),
            Method::GET | Method::HEAD | Method::OPTIONS
        ) {
            return Ok(());
        }
        let headers = request.headers();
        if let Some(site) = headers.get("sec-fetch-site") {
            return match site.as_bytes() {
                b"same-origin" | b"none" => Ok(()),
                _ => Err("cross_origin_request"),
            };
        }
        if let Some(origin) = headers.get(header::ORIGIN) {
            let expected = match self {
                Self::Authenticated { origin } => Some(origin.to_string()),
                // Local mode has no configured origin; the loopback Host is the origin.
                Self::Local => headers
                    .get(header::HOST)
                    .and_then(|host| host.to_str().ok())
                    .or_else(|| {
                        request
                            .uri()
                            .authority()
                            .map(|authority| authority.as_str())
                    })
                    .map(|host| format!("http://{host}")),
            };
            return match expected {
                Some(expected) if origin.as_bytes() == expected.as_bytes() => Ok(()),
                _ => Err("cross_origin_request"),
            };
        }
        // Only browsers too old for Fetch Metadata send cookies without either header.
        if headers.contains_key(header::COOKIE) {
            return Err("unverifiable_origin");
        }
        Ok(())
    }
}

/// Management/auth responses default to `no-store`: handler JSON, provider routes and
/// framework-generated errors must not be cached. Handlers that choose their own
/// caching (UI assets) set `Cache-Control` themselves, which this leaves alone.
pub fn private_routes(router: Router) -> Router {
    router.layer(middleware::from_fn(
        |request: Request, next: Next| async move {
            let mut response = next.run(request).await;
            response
                .headers_mut()
                .entry(header::CACHE_CONTROL)
                .or_insert(header::HeaderValue::from_static("no-store"));
            response
        },
    ))
}

/// Whether a `Host` header names this machine: `localhost` or any loopback address,
/// matching the loopback addresses local mode accepts for `adminListen`. Rejecting
/// other names stops DNS rebinding from reaching the unauthenticated listener.
pub fn is_loopback_host(host: &str) -> bool {
    let Ok(url) = url::Url::parse(&format!("http://{host}")) else {
        return false;
    };
    url.username().is_empty()
        && url.password().is_none()
        && url.path() == "/"
        && url.query().is_none()
        && url.fragment().is_none()
        && match url.host() {
            Some(url::Host::Domain(domain)) => domain == "localhost",
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => false,
        }
}

async fn safeguard_request(State(policy): State<Policy>, request: Request, next: Next) -> Response {
    if matches!(policy, Policy::Local)
        && request
            .headers()
            .get(header::HOST)
            .is_some_and(|host| !host.to_str().ok().is_some_and(is_loopback_host))
    {
        tracing::warn!("local management request has non-loopback Host");
        return Error(StatusCode::FORBIDDEN, "non_loopback_host").into_response();
    }
    if let Err(reason) = policy.check_origin(&request) {
        tracing::warn!(reason, method = %request.method(), "cross-origin management request rejected");
        return Error(StatusCode::FORBIDDEN, reason).into_response();
    }
    let mut response = match tokio::time::timeout(Duration::from_secs(30), next.run(request)).await
    {
        Ok(response) => response,
        Err(_) => {
            tracing::warn!("management request exceeded timeout");
            Error(StatusCode::GATEWAY_TIMEOUT, "admin_request_timeout").into_response()
        }
    };
    let headers = response.headers_mut();
    for (name, value) in [
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
        (header::REFERRER_POLICY, "no-referrer"),
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    ] {
        headers.insert(name, header::HeaderValue::from_static(value));
    }
    if matches!(policy, Policy::Authenticated { .. }) {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            header::HeaderValue::from_static("max-age=31536000"),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request, routing::get};
    use tower::ServiceExt;

    #[tokio::test]
    async fn private_routes_default_to_no_store_and_keep_handler_choices() {
        let private = Router::new()
            .route("/auth/login", get(|| async { "login" }))
            .route(
                "/",
                get(|| async { ([(header::CACHE_CONTROL, "no-cache")], "index") }),
            )
            .route(
                "/assets/app.js",
                get(|| async {
                    (
                        [(
                            header::CACHE_CONTROL,
                            "private, max-age=31536000, immutable",
                        )],
                        "hashed asset",
                    )
                }),
            );
        let public = Router::new().route(
            "/relocated/keys",
            get(|| async { ([(header::CACHE_CONTROL, "public, max-age=300")], "keys") }),
        );
        for policy in [
            Policy::Local,
            Policy::Authenticated {
                origin: "https://admin.example".into(),
            },
        ] {
            let router = policy.apply(private_routes(private.clone()).merge(public.clone()));
            for (method, path, expected) in [
                (Method::GET, "/auth/login", "no-store"),
                // Framework-generated responses (here a 405) get the default too.
                (Method::PUT, "/auth/login", "no-store"),
                (Method::GET, "/", "no-cache"),
                (
                    Method::GET,
                    "/assets/app.js",
                    "private, max-age=31536000, immutable",
                ),
                (Method::GET, "/relocated/keys", "public, max-age=300"),
            ] {
                let response = router
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method.clone())
                            .uri(path)
                            .header("sec-fetch-site", "same-origin")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    response.headers()[header::CACHE_CONTROL],
                    expected,
                    "{method} {path}"
                );
                assert_eq!(
                    response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                    "nosniff"
                );
                assert_eq!(response.headers()[header::X_FRAME_OPTIONS], "DENY");
                assert!(
                    response
                        .headers()
                        .contains_key(header::CONTENT_SECURITY_POLICY)
                );
                assert_eq!(
                    response
                        .headers()
                        .contains_key(header::STRICT_TRANSPORT_SECURITY),
                    matches!(policy, Policy::Authenticated { .. })
                );
            }
        }
    }

    async fn status(policy: &Policy, method: Method, headers: &[(&str, &str)]) -> StatusCode {
        let router = policy.apply(Router::new().route(
            "/api",
            get(|| async { "read" }).delete(|| async { StatusCode::NO_CONTENT }),
        ));
        let mut request = Request::builder().method(method).uri("/api");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        router
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn unsafe_cross_origin_requests_are_rejected_for_every_policy() {
        let authenticated = Policy::Authenticated {
            origin: "https://admin.example".into(),
        };
        let host = ("host", "127.0.0.1:8080");
        for (policy, own, foreign) in [
            (
                &authenticated,
                "https://admin.example",
                "https://admin.example.evil",
            ),
            (
                &Policy::Local,
                "http://127.0.0.1:8080",
                "http://127.0.0.1:9999",
            ),
        ] {
            for (headers, expected) in [
                // Fetch Metadata decides when present, regardless of Origin.
                (
                    vec![("sec-fetch-site", "same-origin")],
                    StatusCode::NO_CONTENT,
                ),
                (vec![("sec-fetch-site", "none")], StatusCode::NO_CONTENT),
                (vec![("sec-fetch-site", "same-site")], StatusCode::FORBIDDEN),
                (
                    vec![("sec-fetch-site", "cross-site")],
                    StatusCode::FORBIDDEN,
                ),
                (
                    vec![("sec-fetch-site", "cross-site"), ("origin", own)],
                    StatusCode::FORBIDDEN,
                ),
                // Otherwise Origin must be exactly this listener's origin.
                (vec![("origin", own)], StatusCode::NO_CONTENT),
                (vec![("origin", foreign)], StatusCode::FORBIDDEN),
                (vec![("origin", "null")], StatusCode::FORBIDDEN),
                // Neither header: a non-browser client, unless it sends cookies.
                (vec![], StatusCode::NO_CONTENT),
                (
                    vec![("authorization", "Bearer token")],
                    StatusCode::NO_CONTENT,
                ),
                (vec![("cookie", "session=1")], StatusCode::FORBIDDEN),
            ] {
                let mut headers = headers;
                headers.push(host);
                assert_eq!(
                    status(policy, Method::DELETE, &headers).await,
                    expected,
                    "{headers:?}"
                );
            }
            // Reads are never blocked; browsers do not expose cross-origin responses.
            assert_eq!(
                status(
                    policy,
                    Method::GET,
                    &[("sec-fetch-site", "cross-site"), ("origin", foreign), host]
                )
                .await,
                StatusCode::OK
            );
        }
    }
}
