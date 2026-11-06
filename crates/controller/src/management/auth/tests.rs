use super::providers::local::LocalAccess;
use super::*;
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
    let auth = Auth::new(Arc::new(LocalAccess));
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
    let auth = Auth::new(Arc::new(LocalAccess));
    let response = request(app(&auth), "/auth/session", Method::GET, HeaderMap::new()).await;
    assert_eq!(response.status(), StatusCode::OK);
    let info: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(info["mode"], "local");
    assert_eq!(info["authenticated"], true);
    assert_eq!(info["role"], "operator");
    assert_eq!(info["permissions"], json!(["admin.read", "devices.delete"]));
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
    for action in ["/auth/logout", "/auth/activity"] {
        assert_eq!(
            request(app(&auth), action, Method::POST, HeaderMap::new())
                .await
                .status(),
            StatusCode::NO_CONTENT
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
    let auth = Auth::new(Arc::new(LocalAccess));
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

#[tokio::test]
async fn middleware_supplies_context_and_handlers_own_authorization() {
    async fn without_context(_auth: Context) -> StatusCode {
        panic!("a handler without authenticated context must not execute");
    }
    let auth = Auth::new(Arc::new(LocalAccess));
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
    let context = Context::new(Arc::new(LocalAccess), principal.clone(), HeaderMap::new());
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
    let auth = Auth::new(Arc::new(LocalAccess));
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
        if access.verb.is_mutation() && principal.subject != "admin" {
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
