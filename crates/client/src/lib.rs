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

#[cfg(unix)]
async fn connect(endpoint: &Path) -> anyhow::Result<LocalStream> {
    UnixStream::connect(endpoint)
        .await
        .with_context(|| format!("connect to {}", endpoint.display()))
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
                return Err(error).with_context(|| format!("connect to {}", endpoint.display()));
            }
        }
    }
}

pub async fn get<T>(endpoint: &Path, path: &str) -> anyhow::Result<T>
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
        .method("GET")
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
    Ok(())
}
