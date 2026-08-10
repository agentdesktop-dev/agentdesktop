use std::path::Path;

use anyhow::{Context, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::{Request, client::conn::http1};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use tokio::net::UnixStream;

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
