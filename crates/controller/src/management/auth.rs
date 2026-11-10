//! Management security boundary.
//!
//! Shared middleware asks the provider to verify credentials and attaches `Context`. Handlers alone call
//! `Context::require` with the operation and any concrete resource ID.
//! The built-in provider grants loopback-only local access; a compiled-in provider
//! can supply remote authentication and its own permission decisions.

use std::{
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use async_trait::async_trait;
use axum::{
    Extension, Json, Router,
    extract::{ConnectInfo, FromRequestParts, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::MethodRouter,
};
use serde_json::json;

use super::audit::{Actor, AuditEvent, AuditLog, EventKind, Outcome, RequestInfo, Target};
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
    /// - `GET /auth/session`: returns 200 with JSON `{ mode: "local" | "authenticated",
    ///   authenticated, subject, role: "viewer" | "operator" | null, permissions:
    ///   [String], assurance, recent_auth_required }` for both signed-in and anonymous
    ///   browsers. Anonymous sessions use `authenticated: false` and null identity,
    ///   role, and assurance fields. This endpoint does not return 401 for sign-out.
    ///   `permissions` only drives what the UI shows; handlers still check every
    ///   operation.
    /// - `GET /auth/login`: starts sign-in (`?reauthenticate=true` forces a fresh
    ///   one) and eventually redirects back to `/`.
    /// - `POST /auth/logout`: ends the session; 204.
    /// - `POST /auth/activity`: foreground user activity that may extend an idle
    ///   session; 204, or 401 when the session has ended.
    ///
    /// Errors are JSON `{ "error": code }`; 401 on protected API requests means
    /// signed out, and 403 with `reauthentication_required` asks the UI to offer a
    /// fresh sign-in.
    fn routes(&self) -> Router;
    /// Verify credentials and return identity evidence, without deciding whether that
    /// identity may perform any particular operation. `Context::require` delegates
    /// those checks to `check`.
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
    audit: AuditLog,
    request: Arc<RequestAudit>,
}

/// Per-request audit state shared by `Context::require` and the middleware.
struct RequestAudit {
    info: RequestInfo,
    /// Mutations authorized (and recorded as attempted) during this request.
    authorized: Mutex<Vec<(String, Target)>>,
    /// Whether any mutation permission was checked, whatever the result.
    mutation_checked: AtomicBool,
}

impl Context {
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// Audit sink, for extension handlers that record events of their own.
    pub fn audit(&self) -> &AuditLog {
        &self.audit
    }

    /// The audited request, to attach to extension-recorded events.
    pub fn request_info(&self) -> &RequestInfo {
        &self.request.info
    }

    /// Called by trusted authentication middleware, never constructed from request JSON.
    pub fn new(
        provider: Arc<dyn Provider>,
        principal: Arc<Principal>,
        headers: HeaderMap,
        audit: AuditLog,
        request: RequestInfo,
    ) -> Self {
        Self {
            provider,
            principal,
            headers: Arc::new(headers),
            audit,
            request: Arc::new(RequestAudit {
                info: request,
                authorized: Mutex::default(),
                mutation_checked: AtomicBool::new(false),
            }),
        }
    }

    /// Check before reading protected data or mutating state. `resource` is a domain
    /// identifier (e.g. a parsed device ID), not an HTTP path. `None` denotes a
    /// collection/global operation.
    ///
    /// Denials are audited. An allowed mutation is audited as attempted before this
    /// returns; if that record cannot be written the mutation is refused (503).
    pub async fn require(&self, access: AccessRequest<'_>) -> Result<(), Error> {
        let result = self
            .provider
            .check(&self.principal, &self.headers, access)
            .await;
        let mutation = access.verb.is_mutation();
        if mutation {
            self.request.mutation_checked.store(true, Ordering::Relaxed);
        }
        let event = |kind: EventKind, outcome| {
            AuditEvent::new(kind, Actor::from(&*self.principal), outcome)
                .action(access.permission())
                .target(access.resource_class, access.resource)
                .request(self.request.info.clone())
        };
        match &result {
            Err(error) => {
                let denied = event(EventKind::AuthorizationDenied, Outcome::Denied).reason(error.1);
                if let Err(audit_error) = self.audit.record(denied).await {
                    tracing::error!(error = format!("{audit_error:#}"), subject = %self.principal.subject,
                        permission = access.permission(), reason = error.1,
                        "failed to record admin authorization denial");
                }
            }
            Ok(()) if mutation => {
                if let Err(audit_error) = self
                    .audit
                    .record(event(EventKind::ActionAttempted, Outcome::Authorized))
                    .await
                {
                    tracing::error!(error = format!("{audit_error:#}"), subject = %self.principal.subject,
                        permission = access.permission(),
                        "refusing admin action that could not be audited");
                    return Err(Error(StatusCode::SERVICE_UNAVAILABLE, "audit_unavailable"));
                }
                self.request.authorized.lock().unwrap().push((
                    access.permission(),
                    Target {
                        class: access.resource_class.to_owned(),
                        id: access.resource.map(str::to_owned),
                    },
                ));
            }
            Ok(()) => {}
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
/// Router-facing facade over the compiled-in authentication provider.
pub struct Auth {
    provider: Arc<dyn Provider>,
    audit: AuditLog,
}

impl Auth {
    pub fn new(provider: Arc<dyn Provider>, audit: AuditLog) -> Self {
        Self { provider, audit }
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
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(peer)| *peer);
    let info = RequestInfo::new(request.method(), request.uri(), request.headers(), peer);
    let context = Context::new(
        auth.provider,
        principal,
        request.headers().clone(),
        auth.audit,
        info,
    );
    let safe = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    request.extensions_mut().insert(context.clone());
    if safe {
        return next.run(request).await;
    }
    // Mutating requests run to completion in their own task, so a timeout or client
    // disconnect cannot drop the handler between the mutation and its completion
    // record.
    let task = tokio::spawn({
        let context = context.clone();
        async move {
            let response = next.run(request).await;
            record_completion(&context, response.status()).await;
            response
        }
    });
    match task.await {
        Ok(response) => response,
        Err(error) => {
            tracing::error!(%error, path = %context.request.info.path, "admin handler panicked");
            record_completion(&context, StatusCode::INTERNAL_SERVER_ERROR).await;
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Records the outcome of each mutation authorized during the request, or flags a
/// mutating request that never checked a mutation permission. The mutation has
/// already happened, so a failure to record is logged rather than returned.
async fn record_completion(context: &Context, status: StatusCode) {
    let authorized = std::mem::take(&mut *context.request.authorized.lock().unwrap());
    let actor = || Actor::from(context.principal());
    let mut completions = Vec::new();
    if authorized.is_empty() {
        // 4xx without a check: routing or extraction rejected the request (e.g. 405)
        // before any handler logic ran.
        if context.request.mutation_checked.load(Ordering::Relaxed) || status.is_client_error() {
            return;
        }
        tracing::error!(method = %context.request.info.method, path = %context.request.info.path,
            status = status.as_u16(),
            "mutating admin request finished without a mutation permission check");
        completions.push(AuditEvent::new(
            EventKind::ActionCompleted,
            actor(),
            Outcome::Unaudited,
        ));
    } else {
        let outcome = if status.is_success() || status.is_redirection() {
            Outcome::Succeeded
        } else if status == StatusCode::NOT_FOUND {
            Outcome::NotFound
        } else {
            Outcome::Failed
        };
        for (action, target) in authorized {
            let mut event =
                AuditEvent::new(EventKind::ActionCompleted, actor(), outcome).action(action);
            event.target = Some(target);
            completions.push(event);
        }
    }
    for event in completions {
        let event = event
            .status(status.as_u16())
            .request(context.request.info.clone());
        if let Err(error) = context.audit.record(event).await {
            tracing::error!(error = format!("{error:#}"), subject = %context.principal.subject,
                path = %context.request.info.path, status = status.as_u16(),
                "failed to record admin action completion");
        }
    }
}

#[cfg(test)]
mod tests;
