use super::routes::ROUTES;
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
                    usage_receiver: None },
                discovery, enrollment: EnrollmentState::new(false), state_dir: dir.path().to_owned(),
                oidc_callback_listen: None, telemetry: None, logout: None, usage: None,
                programs: None,
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

#[tokio::test]
async fn codex_subscription_separates_credentials_and_preserves_responses_request() {
    tokio::time::timeout(Duration::from_secs(15), async {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = upstream.local_addr().unwrap();
        let upstream_task = tokio::spawn(async move {
            let (socket, _) = upstream.accept().await.unwrap();
            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    TokioIo::new(socket),
                    service_fn(move |request: Request<Incoming>| async move {
                        assert_eq!(request.uri(), "/backend-api/codex/responses?test=1");
                        assert_eq!(request.headers()[AUTHORIZATION], "Bearer identity");
                        assert_eq!(
                            request.headers()["x-llm-token"],
                            "chatgpt-subscription-token"
                        );
                        assert_eq!(request.headers()["chatgpt-account-id"], "test-account");
                        assert!(!request.headers().contains_key(PAIRING_HEADER));
                        assert_eq!(
                            request.into_body().collect().await.unwrap().to_bytes(),
                            "{\"model\":\"gpt-5.4\"}"
                        );
                        Ok::<_, Infallible>(
                            Response::builder()
                                .header(CONTENT_TYPE, "text/event-stream")
                                .body(Full::new(Bytes::from_static(
                                    b"data: subscription-response\n\n",
                                )))
                                .unwrap(),
                        )
                    }),
                )
                .await
                .unwrap();
        });
        let (dir, mut state) = request_source_state(address).await;
        let gateway = state.config.llm_gateway.as_mut().unwrap();
        gateway.github_oauth = None;
        gateway.proxy_url = Some(
            format!("http://{address}/backend-api/codex")
                .parse()
                .unwrap(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_address = listener.local_addr().unwrap();
        let proxy = tokio::spawn(serve(listener, state, default_config()));
        let client: Client<HttpConnector, Full<Bytes>> =
            Client::builder(TokioExecutor::new()).build_http();
        for (pairing, credential, expected) in [
            (
                "wrong-pairing",
                Some("chatgpt-subscription-token"),
                StatusCode::FORBIDDEN,
            ),
            (PAIRING, None, StatusCode::UNAUTHORIZED),
            (PAIRING, Some(PAIRING), StatusCode::UNAUTHORIZED),
            (PAIRING, Some("chatgpt-subscription-token"), StatusCode::OK),
        ] {
            let mut request = Request::builder()
                .method(Method::POST)
                .uri(format!("http://{proxy_address}/codex/responses?test=1"))
                .header(PAIRING_HEADER, pairing)
                .header("chatgpt-account-id", "test-account");
            if let Some(credential) = credential {
                request = request.header(AUTHORIZATION, format!("Bearer {credential}"));
            }
            let response = client
                .request(
                    request
                        .body(Full::new(Bytes::from_static(b"{\"model\":\"gpt-5.4\"}")))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            if expected == StatusCode::OK {
                assert_eq!(response.headers()[CONTENT_TYPE], "text/event-stream");
                assert_eq!(
                    response.into_body().collect().await.unwrap().to_bytes(),
                    "data: subscription-response\n\n"
                );
            }
        }
        proxy.abort();
        let _ = proxy.await;
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
            "/codex/responses",
            "codex",
            RouteCredential::Passthrough,
            Upstream::ProxyUrl,
        ),
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
            },
            discovery,
            enrollment: EnrollmentState::new(true),
            state_dir: dir.path().to_owned(),
            oidc_callback_listen: None,
            telemetry: None,
            logout: None,
            usage: None,
            programs: None,
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
            !message.contains("remote-config") && !message.contains(dir.path().to_str().unwrap()),
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
                                    let n = seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                                    assert_eq!(request.headers()[AUTHORIZATION], "Bearer identity");
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
        let (status, code) = error_code(client.request(request(b"{\"n\":2}")).await.unwrap()).await;
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
                                    assert_eq!(request.headers()[AUTHORIZATION], "Bearer cached");
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
        },
        discovery,
        enrollment: EnrollmentState::new(false),
        state_dir: dir.path().to_owned(),
        oidc_callback_listen: None,
        telemetry: None,
        logout: None,
        usage: None,
        programs: None,
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

async fn request_source_state(address: std::net::SocketAddr) -> (tempfile::TempDir, AppState) {
    let dir = tempfile::tempdir().unwrap();
    let store = SecretStore::new(dir.path()).unwrap();
    let account = URL_SAFE_NO_PAD.encode(Sha256::digest(b"https://issuer.example/\0proxy-test"));
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
        },
        discovery,
        enrollment: EnrollmentState::new(false),
        state_dir: dir.path().to_owned(),
        oidc_callback_listen: None,
        telemetry: None,
        logout: None,
        usage: None,
        programs: None,
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
        },
        discovery,
        enrollment: EnrollmentState::new(false),
        state_dir: dir.path().to_owned(),
        oidc_callback_listen: None,
        telemetry: None,
        logout: None,
        usage: None,
        programs: None,
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
                                Ok::<_, Infallible>(Response::new(Full::new(Bytes::from_static(
                                    b"ok",
                                ))))
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
            cache.insert(
				&key,
				"device-test",
				&agentdesktop_core::model::LlmGatewayCredential {
					credential: "short-lived".to_owned(),
					expires_at_unix_seconds: now_unix() + 600,
				},
			);
			cache.expire_at_for_test(&key, Deadline {
				monotonic: std::time::Instant::now() + Duration::from_millis(300),
				unix: now_unix() + 60,
			});
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
