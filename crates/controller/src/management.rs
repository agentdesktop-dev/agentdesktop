//! Management UI/API composition for viewers, operators and compiled-in extensions.
//! Handler authorization is explicit; authentication and policy implementations are
//! supplied through `auth`, independently of the fleet enrollment API.

use std::{borrow::Cow, net::SocketAddr, path::Path, sync::Arc};

use anyhow::Context as _;
use axum::{
    Router,
    http::{StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{MethodRouter, get},
};
use rust_embed::RustEmbed;
use serde::Serialize;
use tracing::info;

use agentdesktop_core::config::{ControllerAdminAccess, ControllerAdminTransport};

use crate::{
    daemon_config::DaemonConfigStore,
    database::Database,
    gateway_jwt::{self, GatewayJwks},
};

pub mod audit;
pub mod auth;
mod handlers;
pub mod http;

use audit::AuditLog;
use auth::{
    Auth,
    authorization::{self, AccessRequest, Verb},
};
use handlers::{
    api_not_found, audit_events, daemon_config, delete_device, device, devices, overview, settings,
    usage_report,
};

/// Embedded UI file lookup by path, e.g. `|path| Assets::get(path).map(|file| file.data)`.
/// The router supplies the index.html fallback, content type and cache headers.
pub type AssetLookup = fn(&str) -> Option<Cow<'static, [u8]>>;

/// Compiled-in management extensions. Routes are authenticated before being merged.
#[derive(Default)]
pub struct Extensions {
    /// Required for authenticated access, and rejected for local access, which always
    /// uses the built-in loopback provider.
    pub authentication: Option<Arc<dyn auth::Provider>>,
    /// Extension-owned APIs, protected separately from public auth/JWKS routes.
    pub routes: Vec<ProtectedRoutes>,
    /// Optional complete UI bundle that replaces the built-in one, e.g. to add pages.
    pub assets: Option<AssetLookup>,
}

/// Extension route handlers. Each handler owns its operation/resource authorization.
pub struct ProtectedRoutes {
    pub path: &'static str,
    pub methods: MethodRouter,
}

#[derive(Clone)]
pub struct State {
    database: Database,
    daemon_config: DaemonConfigStore,
    settings: Settings,
    audit: AuditLog,
}

#[derive(Clone, Serialize)]
pub struct Settings {
    pub fleet_listen: String,
    pub admin_listen: String,
    pub oidc_enabled: bool,
    pub tls_enabled: bool,
    pub gateway_jwt_enabled: bool,
    pub usage_enabled: bool,
}

impl State {
    pub fn new(database: Database, daemon_config: DaemonConfigStore, settings: Settings) -> Self {
        Self {
            audit: AuditLog::new(database.clone()),
            database,
            daemon_config,
            settings,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_audit(mut self, audit: AuditLog) -> Self {
        self.audit = audit;
        self
    }
}

#[derive(RustEmbed)]
#[folder = "../../frontend/controller/dist/"]
struct Assets;

/// Selects the management authentication provider for the configured access mode.
/// Local access always uses the built-in loopback provider; remote (authenticated)
/// access requires a compiled-in provider. Mismatches fail at startup rather than
/// running a provider under the other mode's HTTP policy.
pub fn auth(
    access: &ControllerAdminAccess,
    extensions: &Extensions,
    audit: AuditLog,
) -> anyhow::Result<Auth> {
    match (access, &extensions.authentication) {
        (ControllerAdminAccess::Local, None) => Ok(Auth::new(
            Arc::new(auth::providers::local::LocalAccess),
            audit,
        )),
        (ControllerAdminAccess::Authenticated(_), Some(provider)) => {
            Ok(Auth::new(provider.clone(), audit))
        }
        (ControllerAdminAccess::Authenticated(_), None) => anyhow::bail!(
            "adminAccess.mode authenticated requires a compiled-in authentication provider; this controller supports only local admin access"
        ),
        (ControllerAdminAccess::Local, Some(_)) => anyhow::bail!(
            "this controller has a compiled-in authentication provider, which requires adminAccess.mode authenticated; local mode would bypass it"
        ),
    }
}

pub async fn serve(
    address: SocketAddr,
    state: State,
    gateway_jwks: Option<GatewayJwks>,
    access: ControllerAdminAccess,
    extensions: Extensions,
) -> anyhow::Result<()> {
    let app = router(state, gateway_jwks, &access, extensions)?;
    let listener = tokio::net::TcpListener::bind(address).await?;
    serve_router(listener, app, access).await
}

pub async fn serve_router(
    listener: tokio::net::TcpListener,
    app: Router,
    access: ControllerAdminAccess,
) -> anyhow::Result<()> {
    let address = listener.local_addr()?;
    if let ControllerAdminAccess::Authenticated(config) = &access
        && let ControllerAdminTransport::Tls { certificate, key } = &config.transport
    {
        let tls = axum_server::tls_rustls::RustlsConfig::from_config(Arc::new(admin_tls_config(
            certificate,
            key,
        )?));
        info!(listen = %address, "controller authenticated admin HTTPS listening");
        axum_server::from_tcp_rustls(listener.into_std()?, tls)?
            .serve(app.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .context("serve admin HTTPS")?;
    } else {
        info!(listen = %address, authenticated = matches!(access, ControllerAdminAccess::Authenticated(_)), "controller admin HTTP listening");
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await?;
    }
    Ok(())
}

/// Admin TLS identity with an explicit crypto provider, so the listener does not
/// depend on which rustls backends the final binary happens to enable.
fn admin_tls_config(certificate: &Path, key: &Path) -> anyhow::Result<rustls::ServerConfig> {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
    let certificates = CertificateDer::pem_file_iter(certificate)
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .with_context(|| format!("read admin TLS certificate {}", certificate.display()))?;
    anyhow::ensure!(
        !certificates.is_empty(),
        "admin TLS certificate {} contains no certificates",
        certificate.display()
    );
    let key = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("read admin TLS key {}", key.display()))?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certificates, key)
    .context("load admin TLS identity")?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

/// Compose authenticated APIs/UI and deliberately public auth/JWKS endpoints.
/// The provider and HTTP safeguards both derive from `access`, so they cannot disagree.
/// Permissions are checked by handlers; providers must not infer them from paths.
pub fn router(
    state: State,
    gateway_jwks: Option<GatewayJwks>,
    access: &ControllerAdminAccess,
    extensions: Extensions,
) -> anyhow::Result<Router> {
    let auth = auth(access, &extensions, state.audit.clone())?;
    let policy = match access {
        ControllerAdminAccess::Local => http::Policy::Local,
        ControllerAdminAccess::Authenticated(admin) => http::Policy::Authenticated {
            origin: url::Url::parse(&admin.public_url)
                .context("parse adminAccess.publicUrl")?
                .origin()
                .ascii_serialization()
                .into(),
        },
    };
    let assets = extensions
        .assets
        .unwrap_or(|path| Assets::get(path).map(|file| file.data));
    let api =
        |methods: MethodRouter<State>| auth.authenticate_api(methods.with_state(state.clone()));
    let mut app = Router::new()
        .route("/api/v1/overview", api(get(overview)))
        .route("/api/v1/devices", api(get(devices)))
        .route("/api/v1/daemon-config", api(get(daemon_config)))
        .route(
            "/api/v1/devices/{device_id}",
            api(get(device).delete(delete_device)),
        )
        .route("/api/v1/settings", api(get(settings)))
        .route("/api/v1/usage", api(get(usage_report)))
        .route("/api/v1/audit", api(get(audit_events)))
        .route("/api", auth.authenticate_api(get(api_not_found)))
        .route("/api/{*path}", auth.authenticate_api(get(api_not_found)))
        .fallback(auth.authenticate_browser(get(
            move |uri: Uri, context: auth::Context| async move {
                context
                    .require(AccessRequest {
                        verb: Verb::Read,
                        resource_class: authorization::ADMIN,
                        resource: None,
                    })
                    .await?;
                Ok::<_, auth::Error>(asset(assets, &uri))
            },
        )))
        .merge(auth.routes());
    for routes in extensions.routes {
        app = app.route(routes.path, auth.authenticate_api(routes.methods));
    }
    // Private routes default to no-store; public JWKS keeps its own caching. Common
    // HTTP safeguards still wrap all routes.
    Ok(policy.apply(http::private_routes(app).merge(gateway_jwt::routes(gateway_jwks))))
}

fn asset(assets: AssetLookup, uri: &Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let found = assets(path);
    // Client-side routes fall back to the app, but a missing build file must not:
    // a tab left open across an upgrade would get HTML where it expects JS.
    if found.is_none() && path.starts_with("assets/") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let (path, file) = found
        .map(|file| (path, file))
        .or_else(|| assets("index.html").map(|file| ("index.html", file)))
        .expect("embedded controller UI must contain index.html");
    let content_type = mime_guess::from_path(path).first_or_octet_stream();
    (
        [
            (header::CONTENT_TYPE, content_type.as_ref()),
            (
                header::CACHE_CONTROL,
                // Vite content-hashes everything under `assets/`, so a new build
                // changes those names; anything else (index.html, unhashed files)
                // must be revalidated. `private`: these sit behind sign-in.
                if path.starts_with("assets/") {
                    "private, max-age=31536000, immutable"
                } else {
                    "no-cache"
                },
            ),
        ],
        file,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(path: &str) -> Option<std::borrow::Cow<'static, [u8]>> {
        match path {
            "index.html" | "assets/index-abc123.js" | "favicon.ico" => {
                Some(path.as_bytes().to_vec().into())
            }
            _ => None,
        }
    }

    #[test]
    fn missing_build_files_are_not_served_as_the_app() {
        let response = asset(lookup, &"/assets/index-old999.js".parse().unwrap());
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn only_hashed_assets_are_cached_long_term() {
        for (uri, served, cache) in [
            ("/", "index.html", "no-cache"),
            ("/devices/abc", "index.html", "no-cache"),
            (
                "/assets/index-abc123.js",
                "assets/index-abc123.js",
                "private, max-age=31536000, immutable",
            ),
            ("/favicon.ico", "favicon.ico", "no-cache"),
        ] {
            let response = asset(lookup, &uri.parse().unwrap());
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], cache, "{uri}");
            let content_type = mime_guess::from_path(served).first_or_octet_stream();
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                content_type.as_ref(),
                "{uri}"
            );
        }
    }
}
