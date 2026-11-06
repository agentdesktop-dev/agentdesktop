//! Management security boundary.
//!
//! Shared middleware asks the provider to verify credentials and attaches `Context`. Handlers alone call
//! `Context::require` with the operation and any concrete resource ID.
//! The built-in provider grants loopback-only local access; a compiled-in provider
//! can supply remote authentication and its own permission decisions.

use std::sync::Arc;

use async_trait::async_trait;
use axum::{
    Extension, Json, Router,
    extract::{FromRequestParts, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::MethodRouter,
};
use serde_json::json;

use authentication::Principal;
use authorization::AccessRequest;

pub mod authentication;
pub mod authorization;
pub mod providers;

#[derive(Debug)]
/// HTTP status and stable client-facing error code shared by providers and handlers.
pub struct Error(pub StatusCode, pub &'static str);

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let mut response = (self.0, Json(json!({ "error": self.1 }))).into_response();
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            header::HeaderValue::from_static("no-store"),
        );
        response
    }
}

/// Compiled-in management authentication integration. Implementations own login and
/// session handling, credential verification, and permission decisions. The router
/// enforces cross-origin protection, security headers and timeouts for every provider.
#[async_trait]
pub trait Provider: Send + Sync {
    /// The browser-facing session contract the controller UI is written against.
    /// These routes are merged outside ordinary API protection, so the provider
    /// enforces each endpoint's own requirements:
    ///
    /// - `GET /auth/session`: always 200 for a signed-in or anonymous browser (401
    ///   is also treated as anonymous) with JSON `{ mode: "local" | "oidc",
    ///   authenticated, subject, role: "viewer" | "operator" | null, permissions:
    ///   [String], assurance, recent_auth_required }`. `permissions` only drives what
    ///   the UI shows; handlers still check every operation.
    /// - `GET /auth/login`: starts sign-in (`?reauthenticate=true` forces a fresh
    ///   one) and eventually redirects back to `/`.
    /// - `POST /auth/logout`: ends the session; 204.
    /// - `POST /auth/activity`: foreground user activity that may extend an idle
    ///   session; 204, or 401 when the session has ended.
    ///
    /// Errors are JSON `{ "error": code }`; 401 means signed out, and 403 with
    /// `reauthentication_required` asks the UI to offer a fresh sign-in.
    fn routes(&self) -> Router;
    /// Verify credentials without resolving permissions or installing middleware.
    /// `None` means no authenticated identity; malformed/invalid credentials return
    /// an error. Browser mode must reject API-only service credentials.
    async fn authenticate(
        &self,
        headers: &HeaderMap,
        mode: RequestMode,
    ) -> Result<Option<Principal>, Error>;
    /// Full operation check called by handlers through Context::require. The provider
    /// evaluates permission policy and any credential-specific requirement such as
    /// recent authentication; requests have already passed the router's origin check.
    ///
    /// `principal` is who; `access.resource` is which instance (or collection/global
    /// scope); `access.resource_class` and `access.verb` name the operation.
    /// Mutation requirements derive from the verb, not a separate caller-supplied flag.
    async fn check(
        &self,
        principal: &Principal,
        headers: &HeaderMap,
        access: AccessRequest<'_>,
    ) -> Result<(), Error>;
}

/// Credential/error handling, independent of the operation being authorized.
/// APIs accept service credentials and return 401; browsers redirect to login.
#[derive(Clone, Copy)]
pub enum RequestMode {
    Api,
    Browser,
}
/// Verified principal plus the provider needed for handler-owned authorization.
/// Authentication supplies identity evidence, never a reusable operation grant.
#[derive(Clone)]
pub struct Context {
    provider: Arc<dyn Provider>,
    principal: Arc<Principal>,
    headers: Arc<HeaderMap>,
}

impl Context {
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Called by trusted authentication middleware, never constructed from request JSON.
    pub fn new(provider: Arc<dyn Provider>, principal: Arc<Principal>, headers: HeaderMap) -> Self {
        Self {
            provider,
            principal,
            headers: Arc::new(headers),
        }
    }

    /// Check before reading protected data or mutating state. `resource` is a domain
    /// identifier (e.g. a parsed device ID), not an HTTP path. `None` denotes a
    /// collection/global operation.
    pub async fn require(&self, access: AccessRequest<'_>) -> Result<(), Error> {
        let result = self
            .provider
            .check(&self.principal, &self.headers, access)
            .await;
        if let Err(error) = &result {
            tracing::warn!(subject = %self.principal.subject, issuer = %self.principal.issuer,
                credential_kind = ?self.principal.kind, resource_class = access.resource_class,
                verb = access.verb.as_str(), reason = error.1,
                "admin permission check denied");
        }
        result
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Context {
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        // A handler accidentally mounted without protection must not run anonymously.
        parts.extensions.get::<Self>().cloned().ok_or_else(|| {
            tracing::error!("admin handler called without authentication context");
            Error(StatusCode::UNAUTHORIZED, "missing_admin_context")
        })
    }
}

#[derive(Clone)]
/// Router-facing facade over the compiled-in provider; contains no OIDC implementation.
pub struct Auth {
    provider: Arc<dyn Provider>,
}

impl Auth {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }

    pub fn routes(&self) -> Router {
        self.provider.routes()
    }
    fn authenticate(&self, methods: MethodRouter) -> MethodRouter {
        methods.route_layer(middleware::from_fn_with_state(
            self.clone(),
            authenticate_request,
        ))
    }
    /// Authenticate API credentials and supply context for handler-owned authorization.
    pub fn authenticate_api(&self, methods: MethodRouter) -> MethodRouter {
        self.authenticate(methods)
            .route_layer(Extension(RequestMode::Api))
    }
    /// Authenticate browser sessions, redirecting unauthenticated navigation to login.
    /// The UI handler remains responsible for checking management read permission.
    pub fn authenticate_browser(&self, methods: MethodRouter) -> MethodRouter {
        self.authenticate(methods)
            .route_layer(Extension(RequestMode::Browser))
    }
}

/// Shared credential-to-context flow. Providers own verification; handlers own authorization.
async fn authenticate_request(
    State(auth): State<Auth>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(mode) = request.extensions().get::<RequestMode>().copied() else {
        tracing::error!("management method handler has no request mode");
        return Error(StatusCode::FORBIDDEN, "missing_request_mode").into_response();
    };
    let principal = match auth.provider.authenticate(request.headers(), mode).await {
        Ok(Some(principal)) => Arc::new(principal),
        Ok(None) if matches!(mode, RequestMode::Browser) => {
            return Redirect::to("/auth/login").into_response();
        }
        Ok(None) => return Error(StatusCode::UNAUTHORIZED, "no session").into_response(),
        Err(error) => return error.into_response(),
    };
    // TODO: authenticated management requests are not concurrency-limited; only the
    // 30s request timeout bounds them. A flooding client (e.g. a leaked service token)
    // queues on the database pool, which is shared with the fleet API, so device
    // traffic slows too. If that matters, give management its own small pool or a
    // limit no larger than its pool, rather than a global semaphore.
    let context = Context::new(auth.provider, principal.clone(), request.headers().clone());
    request.extensions_mut().insert(context);
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    if !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS) {
        tracing::info!(
            subject = %principal.subject,
            issuer = %principal.issuer,
            credential_kind = ?principal.kind,
            claims_verified_at = principal.claims_verified_at,
            %method,
            target = %path,
            status = response.status().as_u16(),
            "admin action"
        );
    }
    response
}

#[cfg(test)]
mod tests;
