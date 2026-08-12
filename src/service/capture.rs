use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use http::uri::Authority;
use tokio::net::{TcpListener, TcpStream};

use super::forwarder;
use crate::local_gateway::{GatewayCapability, connect};
use crate::platform::linux::original_destination;

#[derive(Args, Debug)]
pub struct CaptureArgs {
    #[arg(long, default_value = "127.0.0.1:15001")]
    listen: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:15008")]
    hbone_endpoint: SocketAddr,
    #[arg(long, env = "AGENTDESKTOP_CAPTURE_TOKEN_FILE")]
    token_file: PathBuf,
    #[arg(long, default_value_t = 128)]
    max_tunnels: usize,
}

pub async fn run(args: CaptureArgs) -> Result<()> {
    validate_capture_endpoints(args.listen, args.hbone_endpoint)?;
    let token = load_tunnel_token(&args.token_file)?;
    let hbone = connect(args.hbone_endpoint, &token, Duration::from_secs(5)).await?;
    let listener = TcpListener::bind(args.listen).await?;
    tracing::info!(event = "capture_started", listen = %listener.local_addr()?);
    forwarder::serve_capture(
        listener,
        hbone,
        args.max_tunnels,
        Duration::from_secs(5),
        shutdown_signal(),
    )
    .await
}

pub fn original_authority(stream: &TcpStream) -> Result<Authority> {
    Ok(original_socket_address(stream)?.to_string().parse()?)
}

pub fn original_socket_address(stream: &TcpStream) -> Result<SocketAddr> {
    Ok(original_destination(stream)?)
}

fn validate_capture_endpoints(listen: SocketAddr, hbone_endpoint: SocketAddr) -> Result<()> {
    if !listen.ip().is_loopback() || !hbone_endpoint.ip().is_loopback() {
        bail!("prototype capture and HBONE endpoints must be loopback");
    }
    Ok(())
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        tracing::error!(event = "shutdown_signal_failed");
    }
}

pub fn load_tunnel_token(path: &Path) -> Result<GatewayCapability> {
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
    GatewayCapability::from_str(token.trim())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::load_tunnel_token;
    use crate::local_gateway::GatewayCapability;

    #[test]
    fn generates_unique_tokens() {
        let first = GatewayCapability::generate().unwrap();
        let second = GatewayCapability::generate().unwrap();
        assert_ne!(first.environment_value(), second.environment_value());
    }

    #[test]
    fn rejects_broad_token_file_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        fs::write(&path, "local-token").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let error = match load_tunnel_token(&path) {
            Ok(_) => panic!("broad token file permissions were accepted"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("mode 0600"));
    }
}
