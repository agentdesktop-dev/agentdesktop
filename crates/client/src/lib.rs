use std::path::Path;

use anyhow::{Context, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Full};
use hyper::{Request, client::conn::http1};
use hyper_util::rt::TokioIo;
use serde::Serialize;
use serde::de::DeserializeOwned;
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

#[cfg(unix)]
type LocalStream = UnixStream;
#[cfg(windows)]
type LocalStream = NamedPipeClient;

fn connect_error(endpoint: &Path) -> String {
    format!(
        "connect to {}\nCheck that the Agentdesktop daemon is running.",
        endpoint.display()
    )
}

#[cfg(unix)]
async fn connect(endpoint: &Path) -> anyhow::Result<LocalStream> {
    UnixStream::connect(endpoint)
        .await
        .with_context(|| connect_error(endpoint))
}

#[cfg(windows)]
async fn connect(endpoint: &Path) -> anyhow::Result<LocalStream> {
    // CreateFile returns ERROR_PIPE_BUSY when all server instances are in use.
    // A fresh instance should become available as soon as the daemon accepts
    // one of them, so follow the retry pattern recommended by Tokio.
    const ERROR_PIPE_BUSY: i32 = 231;
    loop {
        match ClientOptions::new().open(endpoint.as_os_str()) {
            Ok(client) => return Ok(client),
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            Err(error) => {
                return Err(error).with_context(|| connect_error(endpoint));
            }
        }
    }
}

pub async fn get<T>(endpoint: &Path, path: &str) -> anyhow::Result<T>
where
    T: DeserializeOwned,
{
    request(endpoint, "GET", path).await
}

/// Sends a POST without a request body and decodes the JSON response.
pub async fn post<T>(endpoint: &Path, path: &str) -> anyhow::Result<T>
where
    T: DeserializeOwned,
{
    request(endpoint, "POST", path).await
}

async fn request<T>(endpoint: &Path, method: &str, path: &str) -> anyhow::Result<T>
where
    T: DeserializeOwned,
{
    let stream = connect(endpoint).await?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .context("start HTTP connection")?;

    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "local HTTP connection failed");
        }
    });

    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost")
        .body(Empty::<Bytes>::new())?;
    let response = sender.send_request(request).await.context("send request")?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .context("read response")?
        .to_bytes();

    if !status.is_success() {
        bail!(
            "daemon returned {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }

    serde_json::from_slice(&body).context("decode daemon response")
}

pub async fn post_json<T>(endpoint: &Path, path: &str, value: &T) -> anyhow::Result<()>
where
    T: Serialize,
{
    send_json(endpoint, path, value).await.map(drop)
}

/// Sends a POST with a JSON request body and decodes the JSON response.
pub async fn post_json_decode<T, R>(endpoint: &Path, path: &str, value: &T) -> anyhow::Result<R>
where
    T: Serialize,
    R: DeserializeOwned,
{
    let body = send_json(endpoint, path, value).await?;
    serde_json::from_slice(&body).context("decode daemon response")
}

async fn send_json<T>(endpoint: &Path, path: &str, value: &T) -> anyhow::Result<Bytes>
where
    T: Serialize,
{
    let stream = connect(endpoint).await?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .context("start HTTP connection")?;

    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "local HTTP connection failed");
        }
    });

    let body = serde_json::to_vec(value).context("encode request body")?;
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body)))?;
    let response = sender.send_request(request).await.context("send request")?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .context("read response")?
        .to_bytes();
    if !status.is_success() {
        bail!(
            "daemon returned {status}: {}",
            String::from_utf8_lossy(&body)
        );
    }
    Ok(body)
}

/// A non-2xx response from the daemon's local API.
#[derive(Debug)]
pub struct StatusError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for StatusError {
    /// `daemon returned <status> <reason>: <body>`, the shape of the other
    /// helpers' errors, so a caller can show it as is.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "daemon returned {}", self.status)?;
        if let Some(reason) = hyper::StatusCode::from_u16(self.status)
            .ok()
            .and_then(|status| status.canonical_reason())
        {
            write!(f, " {reason}")?;
        }
        write!(f, ": {}", self.body)
    }
}

impl std::error::Error for StatusError {}

/// Sends a request with an optional JSON body and returns the status and the
/// raw response body.
async fn exchange(
    endpoint: &Path,
    method: &str,
    path: &str,
    json: Option<Vec<u8>>,
) -> anyhow::Result<(hyper::StatusCode, Bytes)> {
    let stream = connect(endpoint).await?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .context("start HTTP connection")?;

    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "local HTTP connection failed");
        }
    });

    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost");
    if json.is_some() {
        request = request.header("content-type", "application/json");
    }
    let request = request.body(Full::new(Bytes::from(json.unwrap_or_default())))?;
    let response = sender.send_request(request).await.context("send request")?;
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .context("read response")?
        .to_bytes();
    Ok((status, body))
}

fn status_error(status: hyper::StatusCode, body: &Bytes) -> anyhow::Error {
    StatusError {
        status: status.as_u16(),
        body: String::from_utf8_lossy(body).into_owned(),
    }
    .into()
}

/// Sends a POST with a JSON body and decodes the JSON response. A non-2xx
/// response is an error carrying a [`StatusError`], so the caller can map
/// statuses to messages.
pub async fn post_json_response<B, T>(endpoint: &Path, path: &str, body: &B) -> anyhow::Result<T>
where
    B: Serialize,
    T: DeserializeOwned,
{
    let json = serde_json::to_vec(body).context("encode request body")?;
    let (status, body) = exchange(endpoint, "POST", path, Some(json)).await?;
    if !status.is_success() {
        return Err(status_error(status, &body));
    }
    serde_json::from_slice(&body).context("decode daemon response")
}

/// Sends a DELETE; any 2xx is success and the body is not decoded. A non-2xx
/// response is an error carrying a [`StatusError`].
pub async fn delete(endpoint: &Path, path: &str) -> anyhow::Result<()> {
    let (status, body) = exchange(endpoint, "DELETE", path, None).await?;
    if !status.is_success() {
        return Err(status_error(status, &body));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::connect_error;

    #[test]
    fn connection_error_suggests_checking_the_daemon() {
        let endpoint = Path::new("/tmp/agentdesktop.sock");

        assert_eq!(
            connect_error(endpoint),
            "connect to /tmp/agentdesktop.sock\nCheck that the Agentdesktop daemon is running."
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn status_error_display_names_the_status_and_the_body() {
        let error = super::StatusError {
            status: 404,
            body: "no session abc".to_owned(),
        };
        let text = error.to_string();
        assert!(text.contains("404"), "{text}");
        assert!(text.contains("no session abc"), "{text}");
    }

    #[cfg(target_os = "linux")]
    mod socket {
        use std::{
            convert::Infallible,
            path::PathBuf,
            sync::{Arc, Mutex},
        };

        use bytes::Bytes;
        use http_body_util::{BodyExt, Full};
        use hyper::{Request, Response, service::service_fn};
        use hyper_util::rt::TokioIo;
        use serde::{Deserialize, Serialize};
        use tokio::net::UnixListener;

        use crate::{StatusError, delete, post_json_response};

        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct Payload {
            value: String,
        }

        /// (method, path, content-type, body)
        type Seen = Arc<Mutex<Vec<(String, String, String, String)>>>;

        /// A daemon stand-in on a Unix socket that answers every request with
        /// `status` and `body`.
        struct FakeDaemon {
            socket: PathBuf,
            seen: Seen,
            task: tokio::task::JoinHandle<()>,
        }

        impl Drop for FakeDaemon {
            fn drop(&mut self) {
                self.task.abort();
                let _ = std::fs::remove_file(&self.socket);
            }
        }

        fn fake_daemon(name: &str, status: u16, canned: &'static str) -> FakeDaemon {
            let socket = std::env::temp_dir().join(format!(
                "agentdesktop-client-test-{}-{name}.sock",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).unwrap();
            let seen: Seen = Arc::default();
            let record = seen.clone();
            let task = tokio::spawn(async move {
                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let record = record.clone();
                    tokio::spawn(async move {
                        let _ = hyper::server::conn::http1::Builder::new()
                            .serve_connection(
                                TokioIo::new(stream),
                                service_fn(move |request: Request<hyper::body::Incoming>| {
                                    let record = record.clone();
                                    async move {
                                        let method = request.method().to_string();
                                        let path = request.uri().path().to_owned();
                                        let content_type = request
                                            .headers()
                                            .get("content-type")
                                            .and_then(|value| value.to_str().ok())
                                            .unwrap_or("")
                                            .to_owned();
                                        let body =
                                            request.into_body().collect().await.unwrap().to_bytes();
                                        record.lock().unwrap().push((
                                            method,
                                            path,
                                            content_type,
                                            String::from_utf8_lossy(&body).into_owned(),
                                        ));
                                        Ok::<_, Infallible>(
                                            Response::builder()
                                                .status(status)
                                                .body(Full::new(Bytes::from_static(
                                                    canned.as_bytes(),
                                                )))
                                                .unwrap(),
                                        )
                                    }
                                }),
                            )
                            .await;
                    });
                }
            });
            FakeDaemon { socket, seen, task }
        }

        #[tokio::test]
        async fn post_json_response_sends_json_and_decodes_the_reply() {
            let daemon = fake_daemon("post-ok", 201, r#"{"value":"created"}"#);
            let reply: Payload = post_json_response(
                &daemon.socket,
                "/v1/things",
                &Payload {
                    value: "asked".into(),
                },
            )
            .await
            .unwrap();
            assert_eq!(
                reply,
                Payload {
                    value: "created".into()
                }
            );
            let seen = daemon.seen.lock().unwrap().clone();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].0, "POST");
            assert_eq!(seen[0].1, "/v1/things");
            assert!(seen[0].2.starts_with("application/json"), "{}", seen[0].2);
            assert_eq!(seen[0].3, r#"{"value":"asked"}"#);
        }

        #[tokio::test]
        async fn post_json_response_carries_a_status_error_for_non_2xx() {
            for (status, body) in [(409u16, "the proxy is not running"), (429, "too many")] {
                let daemon = fake_daemon(&format!("post-{status}"), status, body);
                let error = post_json_response::<_, Payload>(
                    &daemon.socket,
                    "/v1/things",
                    &Payload { value: "x".into() },
                )
                .await
                .expect_err("a non-2xx response is an error");
                let status_error = error
                    .downcast_ref::<StatusError>()
                    .unwrap_or_else(|| panic!("no StatusError in {error:#}"));
                assert_eq!(status_error.status, status);
                assert_eq!(status_error.body, body);
            }
        }

        #[tokio::test]
        async fn delete_succeeds_on_any_2xx_without_decoding_the_body() {
            for (status, body) in [(204u16, ""), (200, "not json at all")] {
                let daemon = fake_daemon(&format!("delete-{status}"), status, body);
                delete(&daemon.socket, "/v1/things/abc").await.unwrap();
                let seen = daemon.seen.lock().unwrap().clone();
                assert_eq!(seen.len(), 1);
                assert_eq!(seen[0].0, "DELETE");
                assert_eq!(seen[0].1, "/v1/things/abc");
            }
        }

        #[tokio::test]
        async fn delete_carries_a_status_error_for_non_2xx() {
            let daemon = fake_daemon("delete-404", 404, "no session abc");
            let error = delete(&daemon.socket, "/v1/things/abc")
                .await
                .expect_err("a 404 is an error");
            let status_error = error
                .downcast_ref::<StatusError>()
                .unwrap_or_else(|| panic!("no StatusError in {error:#}"));
            assert_eq!(status_error.status, 404);
            assert_eq!(status_error.body, "no session abc");
        }
    }
}
