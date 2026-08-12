use std::path::Path;

#[cfg(unix)]
use anyhow::{Context, bail};
#[cfg(unix)]
use bytes::Bytes;
#[cfg(unix)]
use http_body_util::{BodyExt, Empty, Full};
#[cfg(unix)]
use hyper::{Request, client::conn::http1};
#[cfg(unix)]
use hyper_util::rt::TokioIo;
#[cfg(unix)]
use serde::Serialize;
use serde::de::DeserializeOwned;
#[cfg(unix)]
use tokio::net::UnixStream;

#[cfg(unix)]
pub async fn get<T>(socket: &Path, path: &str) -> anyhow::Result<T>
where
    T: DeserializeOwned,
{
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to {}", socket.display()))?;
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

#[cfg(unix)]
pub async fn post_json<T>(socket: &Path, path: &str, value: &T) -> anyhow::Result<()>
where
    T: Serialize,
{
    let stream = UnixStream::connect(socket)
        .await
        .with_context(|| format!("connect to {}", socket.display()))?;
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

#[cfg(not(unix))]
pub async fn get<T>(_endpoint: &Path, _path: &str) -> anyhow::Result<T>
where
    T: DeserializeOwned,
{
    anyhow::bail!("local daemon transport is not implemented on this platform")
}

#[cfg(not(unix))]
pub async fn post_json<T>(_endpoint: &Path, _path: &str, _value: &T) -> anyhow::Result<()>
where
    T: serde::Serialize,
{
    anyhow::bail!("local daemon transport is not implemented on this platform")
}
