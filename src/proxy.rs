use std::collections::HashSet;
use std::future::Future;

use anyhow::Result;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONNECTION, CONTENT_TYPE, HOST};
use axum::http::{HeaderMap, HeaderName, Request, Response, StatusCode};
use axum::routing::any;
use reqwest::{Client, Url};
use tokio::net::TcpListener;

const GATEWAY_ERROR: &str = "agent gateway unavailable\n";

#[derive(Clone)]
struct ProxyState {
    client: Client,
    upstream: Url,
}

pub async fn serve(
    listener: TcpListener,
    upstream: Url,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let state = ProxyState {
        client: Client::new(),
        upstream,
    };
    let app = Router::new().fallback(any(forward)).with_state(state);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

async fn forward(State(state): State<ProxyState>, request: Request<Body>) -> Response<Body> {
    match forward_request(&state, request).await {
        Ok(response) => response,
        Err(error) => {
            eprintln!("failed to forward request to Agent Gateway: {error:#}");
            Response::builder()
                .status(StatusCode::BAD_GATEWAY)
                .header(CONTENT_TYPE, "text/plain; charset=utf-8")
                .header("x-agentgateway-edge-error", "upstream-unavailable")
                .body(Body::from(GATEWAY_ERROR))
                .expect("static error response must be valid")
        }
    }
}

async fn forward_request(state: &ProxyState, request: Request<Body>) -> Result<Response<Body>> {
    let (parts, body) = request.into_parts();
    let url = upstream_url(&state.upstream, &parts.uri)?;
    let mut headers = parts.headers;
    remove_hop_by_hop_headers(&mut headers);
    headers.remove(HOST);

    let upstream_response = state
        .client
        .request(parts.method, url)
        .headers(headers)
        .body(reqwest::Body::wrap_stream(body.into_data_stream()))
        .send()
        .await?;

    let status = upstream_response.status();
    let mut headers = upstream_response.headers().clone();
    remove_hop_by_hop_headers(&mut headers);
    let body = Body::from_stream(upstream_response.bytes_stream());

    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response)
}

fn upstream_url(upstream: &Url, uri: &axum::http::Uri) -> Result<Url> {
    let mut target = upstream.as_str().trim_end_matches('/').to_owned();
    target.push_str(uri.path());
    if let Some(query) = uri.query() {
        target.push('?');
        target.push_str(query);
    }
    Ok(Url::parse(&target)?)
}

fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
    let connection_headers: HashSet<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();

    for name in connection_headers {
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

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::{HeaderMap, Method, Uri};
    use axum::routing::any;
    use futures_util::StreamExt;
    use http_body_util::BodyExt;
    use tokio::sync::{Mutex, mpsc, oneshot};
    use tokio_stream::wrappers::ReceiverStream;

    use super::*;

    #[derive(Clone, Debug)]
    struct CapturedRequest {
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        body: Bytes,
    }

    async fn start_proxy(upstream: Url) -> (std::net::SocketAddr, oneshot::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            serve(listener, upstream, async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
        });
        (address, shutdown_tx)
    }

    #[tokio::test]
    async fn preserves_request_and_response_http_semantics() {
        let captured = Arc::new(Mutex::new(None));
        let fake_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = fake_listener.local_addr().unwrap();
        let fake_app = Router::new()
            .fallback(any(
                move |State(captured): State<Arc<Mutex<Option<CapturedRequest>>>>,
                      request: Request<Body>| async move {
                    let (parts, body) = request.into_parts();
                    let body = body.collect().await.unwrap().to_bytes();
                    *captured.lock().await = Some(CapturedRequest {
                        method: parts.method,
                        uri: parts.uri,
                        headers: parts.headers,
                        body,
                    });
                    (
                        StatusCode::CREATED,
                        [("x-upstream-response", "preserved")],
                        "gateway response",
                    )
                },
            ))
            .with_state(captured.clone());
        tokio::spawn(async move {
            axum::serve(fake_listener, fake_app).await.unwrap();
        });

        let (proxy_address, shutdown) =
            start_proxy(Url::parse(&format!("http://{fake_address}/gateway-base/")).unwrap()).await;
        let response = Client::new()
            .post(format!("http://{proxy_address}/v1/messages?beta=true"))
            .header("x-api-key", "placeholder")
            .header("x-request-marker", "preserved")
            .body("claude request")
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response.headers()["x-upstream-response"], "preserved");
        assert_eq!(response.text().await.unwrap(), "gateway response");
        let request = captured.lock().await.take().unwrap();
        assert_eq!(request.method, Method::POST);
        assert_eq!(request.uri, "/gateway-base/v1/messages?beta=true");
        assert_eq!(request.headers["x-api-key"], "placeholder");
        assert_eq!(request.headers["x-request-marker"], "preserved");
        assert_eq!(request.body, "claude request");
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn streams_response_chunks_without_waiting_for_completion() {
        let (release_tx, release_rx) = oneshot::channel();
        let release_rx = Arc::new(Mutex::new(Some(release_rx)));
        let fake_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = fake_listener.local_addr().unwrap();
        let fake_app = Router::new()
            .fallback(any(
                move |State(release_rx): State<Arc<Mutex<Option<oneshot::Receiver<()>>>>>| async move {
                    let (body_tx, body_rx) = mpsc::channel::<Result<Bytes, Infallible>>(1);
                    let release_rx = release_rx.lock().await.take().unwrap();
                    tokio::spawn(async move {
                        body_tx.send(Ok(Bytes::from_static(b"first"))).await.unwrap();
                        let _ = release_rx.await;
                        body_tx.send(Ok(Bytes::from_static(b"second"))).await.unwrap();
                    });
                    Body::from_stream(ReceiverStream::new(body_rx))
                },
            ))
            .with_state(release_rx);
        tokio::spawn(async move {
            axum::serve(fake_listener, fake_app).await.unwrap();
        });

        let (proxy_address, shutdown) =
            start_proxy(Url::parse(&format!("http://{fake_address}")).unwrap()).await;
        let response = Client::new()
            .get(format!("http://{proxy_address}/v1/messages"))
            .send()
            .await
            .unwrap();
        let mut body = response.bytes_stream();

        let first = tokio::time::timeout(Duration::from_secs(1), body.next())
            .await
            .expect("first chunk was buffered until response completion")
            .unwrap()
            .unwrap();
        assert_eq!(first, "first");
        release_tx.send(()).unwrap();
        assert_eq!(body.next().await.unwrap().unwrap(), "second");
        assert!(body.next().await.is_none());
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn streams_request_chunks_without_waiting_for_completion() {
        let (observed_tx, observed_rx) = oneshot::channel();
        let observed_tx = Arc::new(Mutex::new(Some(observed_tx)));
        let fake_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fake_address = fake_listener.local_addr().unwrap();
        let fake_app = Router::new()
            .fallback(any(
                move |State(observed_tx): State<Arc<Mutex<Option<oneshot::Sender<()>>>>>,
                      request: Request<Body>| async move {
                    let mut body = request.into_body().into_data_stream();
                    let first = body.next().await.unwrap().unwrap();
                    observed_tx.lock().await.take().unwrap().send(()).unwrap();
                    let second = body.next().await.unwrap().unwrap();
                    if first == "first" && second == "second" && body.next().await.is_none() {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::BAD_REQUEST
                    }
                },
            ))
            .with_state(observed_tx);
        tokio::spawn(async move {
            axum::serve(fake_listener, fake_app).await.unwrap();
        });

        let (proxy_address, shutdown) =
            start_proxy(Url::parse(&format!("http://{fake_address}")).unwrap()).await;
        let (body_tx, body_rx) = mpsc::channel::<Result<Bytes, Infallible>>(1);
        let request = tokio::spawn(async move {
            Client::new()
                .post(format!("http://{proxy_address}/v1/messages"))
                .body(reqwest::Body::wrap_stream(ReceiverStream::new(body_rx)))
                .send()
                .await
                .unwrap()
        });

        body_tx
            .send(Ok(Bytes::from_static(b"first")))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), observed_rx)
            .await
            .expect("first request chunk was buffered until request completion")
            .unwrap();
        body_tx
            .send(Ok(Bytes::from_static(b"second")))
            .await
            .unwrap();
        drop(body_tx);
        assert_eq!(request.await.unwrap().status(), StatusCode::NO_CONTENT);
        shutdown.send(()).unwrap();
    }

    #[tokio::test]
    async fn exits_cleanly_after_shutdown_signal() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(serve(
            listener,
            Url::parse("http://127.0.0.1:1").unwrap(),
            async {
                let _ = shutdown_rx.await;
            },
        ));

        shutdown_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("server did not shut down")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn returns_stable_error_when_upstream_is_unavailable() {
        let unavailable = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unavailable_address = unavailable.local_addr().unwrap();
        drop(unavailable);
        let (proxy_address, shutdown) =
            start_proxy(Url::parse(&format!("http://{unavailable_address}")).unwrap()).await;

        let response = Client::new()
            .post(format!("http://{proxy_address}/v1/messages"))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            response.headers()["x-agentgateway-edge-error"],
            "upstream-unavailable"
        );
        assert_eq!(response.text().await.unwrap(), GATEWAY_ERROR);
        shutdown.send(()).unwrap();
    }

    #[test]
    fn removes_standard_and_connection_nominated_hop_headers() {
        let mut headers = HeaderMap::from_iter([
            (CONNECTION, "keep-alive, x-private-hop".parse().unwrap()),
            ("keep-alive".parse().unwrap(), "timeout=5".parse().unwrap()),
            ("x-private-hop".parse().unwrap(), "remove".parse().unwrap()),
            ("x-end-to-end".parse().unwrap(), "keep".parse().unwrap()),
        ]);

        remove_hop_by_hop_headers(&mut headers);

        assert!(!headers.contains_key(CONNECTION));
        assert!(!headers.contains_key("keep-alive"));
        assert!(!headers.contains_key("x-private-hop"));
        assert_eq!(headers["x-end-to-end"], "keep");
    }
}
