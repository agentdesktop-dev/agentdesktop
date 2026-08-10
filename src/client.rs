use std::path::Path;

#[cfg(unix)]
use anyhow::{Context, bail};
#[cfg(unix)]
use bytes::Bytes;
#[cfg(unix)]
use http_body_util::{BodyExt, Empty};
#[cfg(unix)]
use hyper::{Request, client::conn::http1};
#[cfg(unix)]
use hyper_util::rt::TokioIo;
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
            eprintln!("HTTP connection failed: {error}");
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

#[cfg(not(unix))]
pub async fn get<T>(_endpoint: &Path, _path: &str) -> anyhow::Result<T>
where
    T: DeserializeOwned,
{
    anyhow::bail!("local daemon transport is not implemented on this platform")
}
