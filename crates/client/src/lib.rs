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

/// A non-2xx answer from the daemon, as the error of [`post_json_response`].
/// `Display` is `daemon returned <status> <reason>: <body>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusError {
    pub status: u16,
    pub body: String,
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let reason = hyper::StatusCode::from_u16(self.status)
            .ok()
            .and_then(|status| status.canonical_reason())
            .unwrap_or("");
        write!(
            formatter,
            "daemon returned {} {reason}: {}",
            self.status, self.body
        )
    }
}

impl std::error::Error for StatusError {}

/// Sends a POST with a JSON body and decodes the JSON response. A non-2xx
/// answer is an error that carries a [`StatusError`].
pub async fn post_json_response<B, T>(endpoint: &Path, path: &str, body: &B) -> anyhow::Result<T>
where
    B: Serialize,
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

    let encoded = serde_json::to_vec(body).context("encode request body")?;
    let request = Request::builder()
        .method("POST")
        .uri(path)
        .header("host", "localhost")
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(encoded)))?;
    let response = sender.send_request(request).await.context("send request")?;
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .context("read response")?
        .to_bytes();
    if !status.is_success() {
        return Err(StatusError {
            status: status.as_u16(),
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
        .into());
    }
    serde_json::from_slice(&bytes).context("decode daemon response")
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{StatusError, connect_error};

    #[test]
    fn status_error_display_names_the_status_and_carries_the_body() {
        let error = StatusError {
            status: 409,
            body: "executor not found: C:\\missing".into(),
        };
        assert_eq!(
            error.to_string(),
            "daemon returned 409 Conflict: executor not found: C:\\missing"
        );
        assert_eq!(
            StatusError {
                status: 404,
                body: String::new(),
            }
            .to_string(),
            "daemon returned 404 Not Found: "
        );
    }

    #[cfg(unix)]
    mod unix {
        use std::path::Path;

        use serde::{Deserialize, Serialize};
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::UnixListener,
        };

        use super::super::{StatusError, post_json_response};

        #[derive(Serialize)]
        struct Body {
            program: String,
        }

        #[derive(Debug, Deserialize, PartialEq)]
        struct Answer {
            id: String,
        }

        /// Accepts one connection, returns the raw request text, answers with
        /// `status_line` and `body`.
        async fn serve_once(
            socket: &Path,
            status_line: &'static str,
            body: &'static str,
        ) -> tokio::task::JoinHandle<String> {
            let listener = UnixListener::bind(socket).unwrap();
            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut buffer = [0u8; 1024];
                loop {
                    let read = stream.read(&mut buffer).await.unwrap();
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if rest.len() >= length {
                            break;
                        }
                    }
                    if read == 0 {
                        break;
                    }
                }
                let response = format!(
                    "HTTP/1.1 {status_line}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                String::from_utf8_lossy(&request).to_string()
            })
        }

        #[tokio::test]
        async fn post_json_response_sends_the_json_body_and_decodes_the_answer() {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("daemon.sock");
            let server = serve_once(&socket, "201 Created", r#"{"id":"abc"}"#).await;

            let answer: Answer = post_json_response(
                &socket,
                "/v1/launch",
                &Body {
                    program: "claude-code".into(),
                },
            )
            .await
            .unwrap();

            assert_eq!(answer, Answer { id: "abc".into() });
            let request = server.await.unwrap();
            assert!(request.starts_with("POST /v1/launch HTTP/1.1"), "{request}");
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("content-type: application/json"),
                "{request}"
            );
            assert!(
                request.ends_with(r#"{"program":"claude-code"}"#),
                "{request}"
            );
        }

        #[tokio::test]
        async fn post_json_response_reports_a_non_2xx_answer_as_a_status_error() {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("daemon.sock");
            let server = serve_once(&socket, "409 Conflict", "executor not found: x").await;

            let error = post_json_response::<_, Answer>(
                &socket,
                "/v1/launch",
                &Body {
                    program: "claude-code".into(),
                },
            )
            .await
            .unwrap_err();

            let status = error
                .downcast_ref::<StatusError>()
                .unwrap_or_else(|| panic!("a StatusError in {error:#}"));
            assert_eq!(status.status, 409);
            assert_eq!(status.body, "executor not found: x");
            assert_eq!(
                error.to_string(),
                "daemon returned 409 Conflict: executor not found: x"
            );
            server.await.unwrap();
        }
    }

    #[test]
    fn connection_error_suggests_checking_the_daemon() {
        let endpoint = Path::new("/tmp/agentdesktop.sock");

        assert_eq!(
            connect_error(endpoint),
            "connect to /tmp/agentdesktop.sock\nCheck that the Agentdesktop daemon is running."
        );
    }
}
