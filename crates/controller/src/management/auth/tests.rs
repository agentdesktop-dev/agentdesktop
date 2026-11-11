use super::providers::local::LocalAccess;
use super::*;
use crate::audit::{AuditLog, RequestInfo};
use crate::database::Database;
use crate::management::http::is_loopback_host;
use agentdesktop_core::config::ControllerAdminAccess;
use axum::{
    body::Body,
    http::{HeaderValue, Request},
    routing::get,
};
use http_body_util::BodyExt;
use tower::ServiceExt;

#[test]
fn service_kind_does_not_replace_the_verified_subject() {
    use authentication::PrincipalKind;

    let mut principal = Principal::local();
    assert!(!principal.is_service());
    principal.kind = PrincipalKind::Human;
    assert!(!principal.is_service());
    principal.subject = "service-subject".into();
    principal.kind = PrincipalKind::Service {
        client_id: "inventory-client".into(),
    };
    assert!(principal.is_service());
    assert_eq!(principal.subject, "service-subject");
}

#[test]
fn local_claims_are_an_empty_json_object() {
    let principal = Principal::local();
    assert!(principal.claims.is_empty());
    assert_eq!(serde_json::to_value(principal.claims).unwrap(), json!({}));
}

#[test]
fn verbs_define_safety_and_named_permissions() {
    use authorization::Verb;

    for (verb, name, mutation) in [
        (Verb::Read, "read", false),
        (Verb::List, "list", false),
        (Verb::Create, "create", true),
        (Verb::Update, "update", true),
        (Verb::Delete, "delete", true),
    ] {
        let permission = format!("rbac.policy.{name}");
        let access = AccessRequest {
            verb,
            resource_class: "rbac.policy",
            resource: Some("policy-1"),
        };
        assert_eq!(access.verb, verb);
        assert_eq!(access.resource_class, "rbac.policy");
        assert_eq!(access.resource, Some("policy-1"));
        assert_eq!(access.permission(), permission);
        assert_eq!(access.verb.is_mutation(), mutation);
    }
}

fn authenticated_access() -> ControllerAdminAccess {
    serde_json::from_value(json!({
        "mode":"authenticated",
        "publicUrl":"https://admin.example",
        "transport":{"mode":"proxy"},
        "provider":{}
    }))
    .unwrap()
}

fn state(database: Database) -> super::super::State {
    let (audit, _) = AuditLog::in_memory(database.clone());
    super::super::State::new(
        database,
        crate::daemon_config::DaemonConfigStore::new(None),
        super::super::Settings {
            fleet_listen: "127.0.0.1:8443".into(),
            admin_listen: "127.0.0.1:8080".into(),
            oidc_enabled: false,
            tls_enabled: false,
            gateway_jwt_enabled: false,
            usage_enabled: false,
        },
    )
    .with_audit(audit)
}

/// Audit log on its own temporary database, streaming to memory.
struct TestAudit {
    log: AuditLog,
    lines: Arc<std::sync::Mutex<Vec<String>>>,
    path: std::path::PathBuf,
}

impl TestAudit {
    async fn new() -> Self {
        let (database, path) = database().await;
        let (log, lines) = AuditLog::in_memory(database);
        Self { log, lines, path }
    }

    fn events(&self) -> Vec<serde_json::Value> {
        stream_events(&self.lines)
    }
}

impl Drop for TestAudit {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn stream_events(lines: &std::sync::Mutex<Vec<String>>) -> Vec<serde_json::Value> {
    lines
        .lock()
        .unwrap()
        .iter()
        .map(|line| {
            assert!(line.ends_with('\n'));
            serde_json::from_str(line).unwrap()
        })
        .collect()
}

async fn database() -> (Database, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!("local-admin-{}.db", uuid::Uuid::new_v4()));
    let database = Database::connect(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .unwrap();
    (database, path)
}

#[tokio::test]
async fn handlers_own_operations_and_axum_selects_the_matching_handler() {
    use authorization::Verb;
    async fn read(auth: Context) -> Result<String, Error> {
        let access = AccessRequest {
            verb: Verb::Read,
            resource_class: "admin",
            resource: None,
        };
        auth.require(access).await?;
        Ok(access.permission())
    }
    async fn delete(auth: Context) -> Result<String, Error> {
        let access = AccessRequest {
            verb: Verb::Delete,
            resource_class: "devices",
            resource: None,
        };
        auth.require(access).await?;
        Ok(access.permission())
    }
    let audit = TestAudit::new().await;
    let auth = Auth::new(Arc::new(LocalAccess), audit.log.clone());
    let app = Router::new().route(
        "/device",
        auth.authenticate_api(get(read))
            .merge(auth.authenticate_api(axum::routing::delete(delete))),
    );
    for (method, verb) in [
        (Method::GET, Verb::Read),
        (Method::HEAD, Verb::Read),
        (Method::DELETE, Verb::Delete),
    ] {
        let response = request(app.clone(), "/device", method.clone(), HeaderMap::new()).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        if method != Method::HEAD {
            assert_eq!(
                body.as_ref(),
                if verb == Verb::Read {
                    b"admin.read".as_slice()
                } else {
                    b"devices.delete".as_slice()
                }
            );
        } else {
            assert!(body.is_empty());
        }
    }
    for method in [Method::POST, Method::PUT, Method::PATCH, Method::OPTIONS] {
        assert_eq!(
            request(app.clone(), "/device", method, HeaderMap::new())
                .await
                .status(),
            StatusCode::METHOD_NOT_ALLOWED
        );
    }
}

fn app(auth: &Auth) -> Router {
    crate::management::http::Policy::Local.apply(
        Router::new()
            .route(
                "/api/v1/test",
                auth.authenticate_api(get(|auth: Context| async move {
                    auth.require(AccessRequest {
                        verb: authorization::Verb::Read,
                        resource_class: "admin",
                        resource: None,
                    })
                    .await?;
                    Ok::<_, Error>("private")
                }))
                .merge(
                    auth.authenticate_api(axum::routing::delete(|auth: Context| async move {
                        auth.require(AccessRequest {
                            verb: authorization::Verb::Delete,
                            resource_class: "devices",
                            resource: None,
                        })
                        .await?;
                        Ok::<_, Error>(StatusCode::NO_CONTENT)
                    })),
                ),
            )
            .merge(auth.routes()),
    )
}

async fn request(app: Router, path: &str, method: Method, headers: HeaderMap) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .body(Body::empty())
        .unwrap();
    *request.headers_mut() = headers;
    app.oneshot(request).await.unwrap()
}

#[tokio::test]
async fn local_mode_keeps_ui_api_and_bootstrap_without_session_tables() {
    let (database, path) = database().await;
    let audit = TestAudit::new().await;
    let auth = Auth::new(Arc::new(LocalAccess), audit.log.clone());
    let response = request(app(&auth), "/auth/session", Method::GET, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let info: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(info["mode"], "local");
    assert_eq!(info["authenticated"], true);
    assert_eq!(info["role"], "operator");
    assert_eq!(
        info["permissions"],
        json!(authorization::permission_names())
    );
    assert_eq!(
        request(app(&auth), "/api/v1/test", Method::GET, HeaderMap::new())
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        request(app(&auth), "/api/v1/test", Method::DELETE, HeaderMap::new())
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        request(app(&auth), "/auth/activity", Method::POST, HeaderMap::new())
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    for (action, method) in [("/auth/login", Method::GET), ("/auth/logout", Method::POST)] {
        assert_eq!(
            request(app(&auth), action, method, HeaderMap::new())
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    let (sessions,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='admin_sessions'",
    )
    .fetch_one(database.pool())
    .await
    .unwrap();
    assert_eq!(sessions, 0);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn local_mode_rejects_rebinding_and_cross_site_mutations() {
    let audit = TestAudit::new().await;
    let auth = Auth::new(Arc::new(LocalAccess), audit.log.clone());
    // Every loopback address `adminListen` accepts in local mode, not just .1.
    for host in [
        "localhost:8080",
        "LOCALHOST:8080",
        "127.0.0.1:8080",
        "127.12.34.56:8080",
        "127.255.255.254",
        "[::1]:8080",
    ] {
        assert!(is_loopback_host(host), "{host}");
    }
    for host in [
        "evil.example",
        "localhost.evil.example",
        "evil@localhost",
        "localhost/path",
        "128.0.0.1:8080",
        "10.0.0.1:8080",
        "[::ffff:127.0.0.1]:8080",
        "[::2]:8080",
        "",
    ] {
        assert!(!is_loopback_host(host), "{host}");
    }
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("evil.example"));
    assert_eq!(
        request(app(&auth), "/api/v1/test", Method::GET, headers.clone())
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    headers.insert(header::HOST, HeaderValue::from_static("localhost:8080"));
    for origin in [
        "https://evil.example",
        "http://localhost:9999",
        "https://localhost:8080",
        "null",
    ] {
        headers.insert(header::ORIGIN, HeaderValue::from_static(origin));
        assert_eq!(
            request(app(&auth), "/api/v1/test", Method::DELETE, headers.clone())
                .await
                .status(),
            StatusCode::FORBIDDEN,
            "{origin}"
        );
        // Reads stay available; browsers do not expose cross-origin responses.
        assert_eq!(
            request(app(&auth), "/api/v1/test", Method::GET, headers.clone())
                .await
                .status(),
            StatusCode::OK,
            "{origin}"
        );
    }
    headers.insert(
        header::ORIGIN,
        HeaderValue::from_static("http://localhost:8080"),
    );
    assert_eq!(
        request(app(&auth), "/api/v1/test", Method::DELETE, headers.clone())
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    headers.insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
    assert_eq!(
        request(app(&auth), "/api/v1/test", Method::DELETE, headers)
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    let routes = Router::new().route(
        "/api/extension/policy",
        auth.authenticate_api(get(|auth: Context| async move {
            auth.require(AccessRequest {
                verb: authorization::Verb::Read,
                resource_class: "rbac.policy",
                resource: None,
            })
            .await?;
            Ok::<_, Error>("secret")
        })),
    );
    assert_eq!(
        request(
            routes,
            "/api/extension/policy",
            Method::GET,
            HeaderMap::new()
        )
        .await
        .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn authenticated_configuration_is_rejected_without_a_provider() {
    let (database, path) = database().await;
    let error = match super::super::serve(
        "127.0.0.1:0".parse().unwrap(),
        state(database),
        None,
        authenticated_access(),
        super::super::Extensions::default(),
    )
    .await
    {
        Ok(_) => panic!("a build without a provider must not enable remote authentication"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("requires a compiled-in authentication provider")
    );
    std::fs::remove_file(path).unwrap();
}

#[test]
fn admin_read_covers_every_management_read_but_no_mutation() {
    use authorization::Verb;

    for (verb, resource_class, covered) in [
        (Verb::Read, "admin", true),
        (Verb::List, "devices", true),
        (Verb::Read, "devices", true),
        (Verb::Read, "daemon-config", true),
        (Verb::Read, "settings", true),
        (Verb::Read, "usage", true),
        (Verb::Read, "audit", true),
        (Verb::Delete, "devices", false),
        // Reads outside the catalog, such as an extension's policy, are not covered.
        (Verb::Read, "rbac.policy", false),
        (Verb::List, "settings", false),
    ] {
        let access = AccessRequest {
            verb,
            resource_class,
            resource: Some("device-1"),
        };
        assert_eq!(
            access.covered_by_admin_read(),
            covered,
            "{}",
            access.permission()
        );
    }
}

/// Authenticates everyone and records each operation the handlers check.
struct RecordingProvider(Arc<std::sync::Mutex<Vec<String>>>);

#[async_trait]
impl Provider for RecordingProvider {
    fn routes(&self) -> Router {
        Router::new()
    }

    async fn authenticate(
        &self,
        _headers: &HeaderMap,
        _mode: RequestMode,
    ) -> Result<Option<Principal>, Error> {
        Ok(Some(Principal::local()))
    }

    async fn check(
        &self,
        _principal: &Principal,
        _headers: &HeaderMap,
        access: AccessRequest<'_>,
    ) -> Result<(), Error> {
        self.0.lock().unwrap().push(match access.resource {
            Some(resource) => format!("{} {resource}", access.permission()),
            None => access.permission(),
        });
        Ok(())
    }
}

#[tokio::test]
async fn management_routes_check_precise_operations() {
    let (database, path) = database().await;
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let router = super::super::router(
        state(database),
        None,
        &authenticated_access(),
        super::super::Extensions {
            authentication: Some(Arc::new(RecordingProvider(calls.clone()))),
            ..Default::default()
        },
    )
    .unwrap();
    for (method, path, expected) in [
        (Method::GET, "/api/v1/overview", "devices.list"),
        (Method::GET, "/api/v1/devices", "devices.list"),
        (
            Method::GET,
            "/api/v1/devices/device-1",
            "devices.read device-1",
        ),
        (
            Method::DELETE,
            "/api/v1/devices/device-1",
            "devices.delete device-1",
        ),
        (Method::GET, "/api/v1/daemon-config", "daemon-config.read"),
        (Method::GET, "/api/v1/settings", "settings.read"),
        (Method::GET, "/api/v1/usage", "usage.read"),
        (Method::GET, "/devices", "admin.read"),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        request(router.clone(), path, method.clone(), headers).await;
        assert_eq!(
            calls.lock().unwrap().drain(..).collect::<Vec<_>>(),
            [expected],
            "{method} {path}"
        );
    }
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn local_configuration_is_rejected_with_a_provider() {
    let (database, path) = database().await;
    let error = match super::super::router(
        state(database),
        None,
        &ControllerAdminAccess::Local,
        super::super::Extensions {
            authentication: Some(Arc::new(TokenProvider)),
            ..Default::default()
        },
    ) {
        Ok(_) => panic!("a compiled-in provider must not run under the local policy"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("requires adminAccess.mode authenticated")
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn middleware_supplies_context_and_handlers_own_authorization() {
    async fn without_context(_auth: Context) -> StatusCode {
        panic!("a handler without authenticated context must not execute");
    }
    let audit = TestAudit::new().await;
    let auth = Auth::new(Arc::new(LocalAccess), audit.log.clone());
    let authenticated = Router::new().route(
        "/new-api",
        auth.authenticate_api(get(
            |auth: Context, extensions: axum::http::Extensions| async move {
                assert!(extensions.get::<Arc<Principal>>().is_none());
                assert_eq!(auth.principal().subject, Principal::local().subject);
                "authenticated"
            },
        )),
    );
    let response = request(authenticated, "/new-api", Method::GET, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let unprotected = Router::new().route("/new-api", get(without_context));
    assert_eq!(
        request(unprotected, "/new-api", Method::GET, HeaderMap::new())
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let unknown = Router::new().route(
        "/new-api",
        auth.authenticate_api(get(|auth: Context| async move {
            auth.require(AccessRequest {
                verb: authorization::Verb::Read,
                resource_class: "unknown",
                resource: None,
            })
            .await?;
            Ok::<_, Error>("private")
        })),
    );
    assert_eq!(
        request(unknown, "/new-api", Method::GET, HeaderMap::new())
            .await
            .status(),
        StatusCode::OK
    );
}

#[tokio::test]
async fn request_context_shares_evidence_and_checks_borrowed_operation_and_resource() {
    let principal = Arc::new(Principal::local());
    let audit = TestAudit::new().await;
    let context = Context::new(
        Arc::new(LocalAccess),
        principal.clone(),
        HeaderMap::new(),
        audit.log.clone(),
        RequestInfo::new(&Method::GET, &"/".parse().unwrap(), &HeaderMap::new(), None),
    );
    let extracted = context.clone();
    assert!(Arc::ptr_eq(&context.principal, &extracted.principal));
    assert!(Arc::ptr_eq(&context.headers, &extracted.headers));
    assert!(Arc::ptr_eq(&extracted.principal, &principal));

    // Neither input has a static lifetime or is transferred into the provider.
    let resource_class = String::from("admin");
    let resource = String::from("device-123");
    extracted
        .require(AccessRequest {
            verb: authorization::Verb::Read,
            resource_class: &resource_class,
            resource: Some(&resource),
        })
        .await
        .unwrap();
    assert_eq!(resource_class, "admin");
    assert_eq!(resource, "device-123");
}

#[tokio::test]
async fn read_only_routes_explicitly_deny_writes() {
    let audit = TestAudit::new().await;
    let auth = Auth::new(Arc::new(LocalAccess), audit.log.clone());
    let router = Router::new().route(
        "/read-only",
        auth.authenticate_api(get(|| async { "data" })),
    );
    assert_eq!(
        request(router.clone(), "/read-only", Method::GET, HeaderMap::new())
            .await
            .status(),
        StatusCode::OK
    );
    let response = request(router, "/read-only", Method::POST, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

/// Extension-style provider: a bearer token names the subject, and only "admin" may
/// delete devices.
struct TokenProvider;

#[async_trait]
impl Provider for TokenProvider {
    fn routes(&self) -> Router {
        Router::new().route("/auth/session", get(|| async { "session" }))
    }

    async fn authenticate(
        &self,
        headers: &HeaderMap,
        _mode: RequestMode,
    ) -> Result<Option<Principal>, Error> {
        let Some(value) = headers.get(header::AUTHORIZATION) else {
            return Ok(None);
        };
        let subject = value
            .to_str()
            .ok()
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or(Error(StatusCode::UNAUTHORIZED, "invalid_token"))?;
        Ok(Some(Principal {
            issuer: "https://id.example".into(),
            subject: subject.into(),
            kind: authentication::PrincipalKind::Human,
            claims: Default::default(),
            auth_time: None,
            claims_verified_at: 0,
        }))
    }

    async fn check(
        &self,
        principal: &Principal,
        _headers: &HeaderMap,
        access: AccessRequest<'_>,
    ) -> Result<(), Error> {
        if (access.verb.is_mutation() || access.resource_class == authorization::AUDIT)
            && principal.subject != "admin"
        {
            return Err(Error(StatusCode::FORBIDDEN, "forbidden"));
        }
        Ok(())
    }
}

fn bearer(subject: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {subject}")).unwrap(),
    );
    headers
}

#[tokio::test]
async fn extension_providers_authenticate_and_authorize_the_management_router() {
    let (database, path) = database().await;
    let router = super::super::router(
        state(database),
        None,
        &authenticated_access(),
        super::super::Extensions {
            authentication: Some(Arc::new(TokenProvider)),
            routes: vec![super::super::ProtectedRoutes {
                path: "/api/extension",
                methods: get(|auth: Context| async move {
                    auth.require(AccessRequest {
                        verb: authorization::Verb::Read,
                        resource_class: "extension",
                        resource: None,
                    })
                    .await?;
                    Ok::<_, Error>(auth.principal().subject.clone())
                }),
            }],
            assets: Some(|_| None),
        },
    )
    .unwrap();
    let response = request(router.clone(), "/devices", Method::GET, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    assert_eq!(response.headers()[header::LOCATION], "/auth/login");
    assert_eq!(
        request(
            router.clone(),
            "/api/v1/devices",
            Method::GET,
            HeaderMap::new()
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let mut invalid = HeaderMap::new();
    invalid.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic x"));
    assert_eq!(
        request(router.clone(), "/api/v1/devices", Method::GET, invalid)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = request(
        router.clone(),
        "/api/v1/devices",
        Method::GET,
        bearer("viewer"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    assert!(
        response
            .headers()
            .contains_key(header::STRICT_TRANSPORT_SECURITY)
    );
    let body = request(
        router.clone(),
        "/api/extension",
        Method::GET,
        bearer("viewer"),
    )
    .await
    .into_body()
    .collect()
    .await
    .unwrap()
    .to_bytes();
    assert_eq!(body.as_ref(), b"viewer");
    assert_eq!(
        request(
            router.clone(),
            "/api/v1/devices/device-1",
            Method::DELETE,
            bearer("viewer")
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        request(
            router.clone(),
            "/api/v1/devices/device-1",
            Method::DELETE,
            bearer("admin")
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        request(router, "/auth/session", Method::GET, HeaderMap::new())
            .await
            .status(),
        StatusCode::OK
    );
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn direct_tls_transport_serves_the_authenticated_router_over_https_only() {
    let (database, path) = database().await;
    let directory = std::env::temp_dir().join(format!("admin-tls-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory).unwrap();
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let certificate = directory.join("certificate.pem");
    let key = directory.join("key.pem");
    std::fs::write(&certificate, cert.pem()).unwrap();
    std::fs::write(&key, signing_key.serialize_pem()).unwrap();
    let ControllerAdminAccess::Authenticated(mut admin) = authenticated_access() else {
        unreachable!();
    };
    admin.transport = agentdesktop_core::config::ControllerAdminTransport::Tls { certificate, key };
    let access = ControllerAdminAccess::Authenticated(admin);
    let router = super::super::router(
        state(database),
        None,
        &access,
        super::super::Extensions {
            authentication: Some(Arc::new(TokenProvider)),
            ..Default::default()
        },
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(super::super::serve_router(listener, router, access));
    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(cert.pem().as_bytes()).unwrap())
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let url = format!("https://{address}/api/v1/devices");
    let mut response = None;
    for _ in 0..50 {
        match client.get(&url).send().await {
            Ok(ok) => {
                response = Some(ok);
                break;
            }
            Err(error) if error.is_connect() => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(error) => panic!("HTTPS probe failed: {error}"),
        }
    }
    let response = response.expect("admin TLS server did not start");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        response.headers()[header::STRICT_TRANSPORT_SECURITY],
        "max-age=31536000"
    );
    let response = client.get(&url).bearer_auth("viewer").send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    // The listener advertises ALPN, so HTTP/2 clients negotiate it.
    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    assert!(
        client
            .get(format!("http://{address}/api/v1/devices"))
            .bearer_auth("viewer")
            .send()
            .await
            .map_or(true, |response| !response.status().is_success()),
        "plaintext must not reach the TLS listener"
    );
    server.abort();
    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_file(path).unwrap();
}

fn audited_router(database: Database, audit: AuditLog) -> Router {
    super::super::router(
        state(database).with_audit(audit),
        None,
        &authenticated_access(),
        super::super::Extensions {
            authentication: Some(Arc::new(TokenProvider)),
            assets: Some(|_| None),
            ..Default::default()
        },
    )
    .unwrap()
}

async fn enrolled_database() -> (Database, std::path::PathBuf) {
    let (database, path) = database().await;
    database
        .enroll_device("device-1", "host", "https://id.example", "owner", None)
        .await
        .unwrap();
    (database, path)
}

async fn json_body(response: Response) -> serde_json::Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn device_deletion_is_audited_with_actor_target_and_outcome() {
    let (database, path) = enrolled_database().await;
    let (audit, lines) = AuditLog::in_memory(database.clone());
    let router = audited_router(database.clone(), audit);
    let mut headers = bearer("admin");
    headers.insert(header::USER_AGENT, HeaderValue::from_static("audit-test"));
    let response = request(
        router.clone(),
        "/api/v1/devices/device-1",
        Method::DELETE,
        headers.clone(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(database.get_device("device-1").await.unwrap().is_none());

    let events = stream_events(&lines);
    let [attempted, completed] = &events[..] else {
        panic!("expected attempted and completed events: {events:?}");
    };
    for event in [attempted, completed] {
        assert_eq!(event["schema"], "agentdesktop.audit.v1");
        assert_eq!(event["actor"]["kind"], "human");
        assert_eq!(event["actor"]["issuer"], "https://id.example");
        assert_eq!(event["actor"]["subject"], "admin");
        assert_eq!(event["action"], "devices.delete");
        assert_eq!(
            event["target"],
            json!({ "class": "devices", "id": "device-1" })
        );
        assert_eq!(event["request"]["method"], "DELETE");
        assert_eq!(event["request"]["path"], "/api/v1/devices/device-1");
        assert_eq!(event["request"]["user_agent"], "audit-test");
        assert!(event["time"].as_str().unwrap().ends_with('Z'));
    }
    assert_eq!(attempted["request"]["id"], completed["request"]["id"]);
    assert_eq!(attempted["event"], "action.attempted");
    assert_eq!(attempted["outcome"], "authorized");
    assert_eq!(completed["event"], "action.completed");
    assert_eq!(completed["outcome"], "succeeded");
    assert_eq!(completed["status"], 204);
    // Streamed lines are numbered per stream, so the SIEM can spot a lost line.
    assert_eq!(attempted["stream"]["seq"], 1);
    assert_eq!(completed["stream"]["seq"], 2);
    assert_eq!(attempted["stream"]["id"], completed["stream"]["id"]);
    assert!(completed["seq"].as_i64().unwrap() > attempted["seq"].as_i64().unwrap());

    let response = request(router, "/api/v1/devices/device-1", Method::DELETE, headers).await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let events = stream_events(&lines);
    let missing = events.last().unwrap();
    assert_eq!(missing["outcome"], "not_found");
    assert_eq!(missing["status"], 404);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn denials_are_audited_and_the_action_does_not_run() {
    let (database, path) = enrolled_database().await;
    let (audit, lines) = AuditLog::in_memory(database.clone());
    let router = audited_router(database.clone(), audit);
    let response = request(
        router.clone(),
        "/api/v1/devices/device-1",
        Method::DELETE,
        bearer("viewer"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(database.get_device("device-1").await.unwrap().is_some());
    let response = request(router, "/api/v1/audit", Method::GET, bearer("viewer")).await;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let events = stream_events(&lines);
    let [delete, read] = &events[..] else {
        panic!("expected two denials: {events:?}");
    };
    for event in [delete, read] {
        assert_eq!(event["event"], "authorization.denied");
        assert_eq!(event["outcome"], "denied");
        assert_eq!(event["reason"], "forbidden");
        assert_eq!(event["actor"]["subject"], "viewer");
    }
    assert_eq!(delete["action"], "devices.delete");
    assert_eq!(delete["target"]["id"], "device-1");
    assert_eq!(read["action"], "audit.read");
    assert_eq!(read["target"], json!({ "class": "audit", "id": null }));
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn mutations_fail_closed_when_the_audit_log_is_unavailable() {
    let (database, path) = enrolled_database().await;
    let router = audited_router(database.clone(), AuditLog::broken_stream(database.clone()));
    let response = request(
        router,
        "/api/v1/devices/device-1",
        Method::DELETE,
        bearer("admin"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json_body(response).await["error"], "audit_unavailable");
    assert!(database.get_device("device-1").await.unwrap().is_some());

    let (audit, lines) = AuditLog::in_memory(database.clone());
    sqlx::raw_sql("DROP TABLE admin_audit_events")
        .execute(database.pool())
        .await
        .unwrap();
    let response = request(
        audited_router(database.clone(), audit),
        "/api/v1/devices/device-1",
        Method::DELETE,
        bearer("admin"),
    )
    .await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(database.get_device("device-1").await.unwrap().is_some());
    assert!(lines.lock().unwrap().is_empty());
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn mutations_without_a_permission_check_are_flagged() {
    let audit = TestAudit::new().await;
    let auth = Auth::new(Arc::new(LocalAccess), audit.log.clone());
    let router = Router::new()
        .route(
            "/unchecked",
            auth.authenticate_api(axum::routing::post(|| async { "changed" })),
        )
        .route(
            "/read-only",
            auth.authenticate_api(get(|| async { "data" })),
        );
    let response = request(router.clone(), "/unchecked", Method::POST, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let events = audit.events();
    let [unchecked] = &events[..] else {
        panic!("expected one event: {events:?}");
    };
    assert_eq!(unchecked["event"], "action.completed");
    assert_eq!(unchecked["outcome"], "unaudited");
    assert_eq!(unchecked["action"], serde_json::Value::Null);
    assert_eq!(unchecked["status"], 200);
    assert_eq!(unchecked["actor"]["kind"], "local");
    assert_eq!(unchecked["request"]["path"], "/unchecked");

    let response = request(router, "/read-only", Method::POST, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(audit.events().len(), 1);
}

#[tokio::test]
async fn audit_api_pages_newest_first() {
    let (database, path) = database().await;
    let (audit, _) = AuditLog::in_memory(database.clone());
    let router = audited_router(database.clone(), audit);
    for device in ["a", "b", "c"] {
        let response = request(
            router.clone(),
            &format!("/api/v1/devices/{device}"),
            Method::DELETE,
            bearer("admin"),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    let seqs = |page: &serde_json::Value| -> Vec<i64> {
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["seq"].as_i64().unwrap())
            .collect()
    };
    let page = json_body(
        request(
            router.clone(),
            "/api/v1/audit?limit=4",
            Method::GET,
            bearer("admin"),
        )
        .await,
    )
    .await;
    assert_eq!(seqs(&page), [6, 5, 4, 3]);
    assert_eq!(page["next_before"], 3);
    assert_eq!(page["events"][0]["target"]["id"], "c");
    assert_eq!(page["events"][0]["schema"], "agentdesktop.audit.v1");
    let page = json_body(
        request(
            router.clone(),
            "/api/v1/audit?before=3&limit=4",
            Method::GET,
            bearer("admin"),
        )
        .await,
    )
    .await;
    assert_eq!(seqs(&page), [2, 1]);
    assert_eq!(page["next_before"], serde_json::Value::Null);
    // Allowed reads, including of the audit log itself, are not recorded.
    let page =
        json_body(request(router, "/api/v1/audit", Method::GET, bearer("admin")).await).await;
    assert_eq!(seqs(&page), [6, 5, 4, 3, 2, 1]);
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn served_requests_record_the_peer_address() {
    let (database, path) = database().await;
    let (audit, lines) = AuditLog::in_memory(database.clone());
    let router = super::super::router(
        state(database).with_audit(audit),
        None,
        &ControllerAdminAccess::Local,
        super::super::Extensions::default(),
    )
    .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(super::super::serve_router(
        listener,
        router,
        ControllerAdminAccess::Local,
    ));
    let response = reqwest::Client::new()
        .delete(format!("http://{address}/api/v1/devices/missing"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let events = stream_events(&lines);
    assert_eq!(events.len(), 2);
    for event in &events {
        assert!(
            event["request"]["peer"]
                .as_str()
                .unwrap()
                .starts_with("127.0.0.1:"),
            "{event}"
        );
        assert_eq!(event["actor"]["kind"], "local");
    }
    server.abort();
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn completion_is_recorded_when_the_client_goes_away() {
    let audit = TestAudit::new().await;
    let auth = Auth::new(Arc::new(LocalAccess), audit.log.clone());
    let router = Router::new().route(
        "/slow",
        auth.authenticate_api(axum::routing::delete(|auth: Context| async move {
            auth.require(AccessRequest {
                verb: authorization::Verb::Delete,
                resource_class: "devices",
                resource: Some("device-1"),
            })
            .await?;
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            Ok::<_, Error>(StatusCode::NO_CONTENT)
        })),
    );
    let abandoned = tokio::time::timeout(
        std::time::Duration::from_millis(30),
        request(router, "/slow", Method::DELETE, HeaderMap::new()),
    )
    .await;
    assert!(abandoned.is_err(), "the client must give up first");
    for _ in 0..200 {
        if audit.events().len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let events = audit.events();
    let [attempted, completed] = &events[..] else {
        panic!("expected attempted and completed events: {events:?}");
    };
    assert_eq!(attempted["outcome"], "authorized");
    assert_eq!(completed["outcome"], "succeeded");
    assert_eq!(completed["status"], 204);
    assert_eq!(completed["target"]["id"], "device-1");
}
