use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use clap::Parser;
use http::HeaderMap;
use http::header::HeaderValue;
use http::uri::Authority;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::hbone::HboneClient;
use crate::platform::linux::original_destination;

pub const TUNNEL_TOKEN_HEADER: &str = "x-agentgateway-edge-token";

#[derive(Debug, Parser)]
#[command(version, about = "Relay redirected Linux TCP flows over HBONE")]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:15001")]
    listen: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:15008")]
    hbone_endpoint: SocketAddr,
    #[arg(long, env = "AGENTGATEWAY_EDGE_CAPTURE_TOKEN_FILE")]
    token_file: PathBuf,
    #[arg(long, default_value_t = 128)]
    max_tunnels: usize,
}

pub async fn run_from(arguments: impl IntoIterator<Item = OsString>) -> Result<()> {
    let cli = Cli::parse_from(arguments);
    if !cli.listen.ip().is_loopback() || !cli.hbone_endpoint.ip().is_loopback() {
        bail!("prototype capture and HBONE endpoints must be loopback");
    }
    if cli.max_tunnels == 0 {
        bail!("max tunnels must be greater than zero");
    }
    let mut connect_headers = HeaderMap::new();
    connect_headers.insert(TUNNEL_TOKEN_HEADER, load_tunnel_token(&cli.token_file)?);
    let hbone = HboneClient::connect_with_headers(cli.hbone_endpoint, connect_headers).await?;
    let listener = TcpListener::bind(cli.listen).await?;
    tracing::info!(event = "capture_started", listen = %listener.local_addr()?);
    serve(listener, hbone, cli.max_tunnels, shutdown_signal()).await
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        tracing::error!(event = "shutdown_signal_failed");
    }
}

pub fn load_tunnel_token(path: &Path) -> Result<HeaderValue> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("read tunnel token metadata from {}", path.display()))?;
    if !metadata.is_file() {
        bail!("tunnel token {} must be a regular file", path.display());
    }
    if metadata.uid() != rustix::process::getuid().as_raw() {
        bail!(
            "tunnel token {} must be owned by the current user",
            path.display()
        );
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("tunnel token {} must have mode 0600", path.display());
    }
    let token = fs::read_to_string(path)
        .with_context(|| format!("read tunnel token from {}", path.display()))?;
    let mut value =
        HeaderValue::from_str(token.trim()).context("tunnel token is not a valid header value")?;
    if value.is_empty() {
        bail!("tunnel token must not be empty");
    }
    value.set_sensitive(true);
    Ok(value)
}

pub async fn serve(
    listener: TcpListener,
    hbone: HboneClient,
    max_tunnels: usize,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let permits = Arc::new(Semaphore::new(max_tunnels));
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    tracing::warn!(event = "capture_rejected", reason = "overloaded");
                    continue;
                };
                let hbone = hbone.clone();
                tokio::spawn(async move {
                    let result = forward(stream, &hbone).await;
                    drop(permit);
                    if let Err(error) = result {
                        let _ = error;
                        tracing::warn!(event = "capture_failed", reason = "tunnel_unavailable");
                    }
                });
            }
        }
    }
}

async fn forward(stream: TcpStream, hbone: &HboneClient) -> Result<()> {
    let destination = original_destination(&stream)?;
    forward_to(stream, destination, hbone).await
}

async fn forward_to(
    mut stream: TcpStream,
    destination: SocketAddr,
    hbone: &HboneClient,
) -> Result<()> {
    let authority: Authority = destination.to_string().parse()?;
    let mut tunnel = hbone.open_tunnel(authority).await?;
    tokio::io::copy_bidirectional(&mut stream, &mut tunnel).await?;
    stream.shutdown().await?;
    tunnel.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use bytes::Bytes;
    use http::{Method, Response};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::{Duration, timeout};

    use super::{HboneClient, forward_to, load_tunnel_token};

    #[test]
    fn loads_owner_only_tunnel_token_as_sensitive() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        fs::write(&path, "local-token\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let token = load_tunnel_token(&path).unwrap();

        assert_eq!(token, "local-token");
        assert!(token.is_sensitive());
    }

    #[test]
    fn rejects_tunnel_token_with_group_or_world_access() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        fs::write(&path, "local-token").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let error = load_tunnel_token(&path).unwrap_err();

        assert!(error.to_string().contains("mode 0600"));
    }

    #[tokio::test]
    async fn relays_tcp_flow_to_original_destination_over_hbone() {
        let hbone_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hbone_address = hbone_listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = hbone_listener.accept().await.unwrap();
            let mut connection = h2::server::handshake(stream).await.unwrap();
            if let Some(result) = connection.accept().await {
                let (request, mut respond) = result.unwrap();
                tokio::spawn(async move {
                    assert_eq!(request.method(), Method::CONNECT);
                    assert_eq!(request.uri().authority().unwrap(), "203.0.113.9:443");
                    let mut receive = request.into_body();
                    let mut send = respond.send_response(Response::new(()), false).unwrap();
                    let bytes = receive.data().await.unwrap().unwrap();
                    receive
                        .flow_control()
                        .release_capacity(bytes.len())
                        .unwrap();
                    assert_eq!(bytes, Bytes::from_static(b"captured tls bytes"));
                    send.send_data(Bytes::from_static(b"gateway tls bytes"), true)
                        .unwrap();
                });
            }
            while connection.accept().await.is_some() {}
        });

        timeout(Duration::from_secs(1), async {
            let hbone = HboneClient::connect(hbone_address).await.unwrap();
            let relay_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let relay_address = relay_listener.local_addr().unwrap();
            let relay = tokio::spawn(async move {
                let (stream, _) = relay_listener.accept().await.unwrap();
                forward_to(stream, "203.0.113.9:443".parse().unwrap(), &hbone)
                    .await
                    .unwrap();
            });

            let mut client = tokio::net::TcpStream::connect(relay_address).await.unwrap();
            client.write_all(b"captured tls bytes").await.unwrap();
            client.shutdown().await.unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();

            assert_eq!(response, b"gateway tls bytes");
            relay.await.unwrap();
            server.await.unwrap();
        })
        .await
        .expect("capture relay exchange timed out");
    }
}
