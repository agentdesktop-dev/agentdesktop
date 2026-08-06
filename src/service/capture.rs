use std::fs;
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use clap::Args;
use http::HeaderMap;
use http::header::HeaderValue;
use http::uri::Authority;
use tokio::net::{TcpListener, TcpStream};

use super::forwarder;
use super::hbone::HboneClient;
use crate::platform::linux::original_destination;

pub const TUNNEL_TOKEN_HEADER: &str = "x-agentdesktop-token";
pub const TUNNEL_TOKEN_ENV: &str = "AGENTDESKTOP_CAPTURE_TOKEN";

pub struct CaptureToken {
    value: HeaderValue,
}

impl CaptureToken {
    pub fn generate() -> Result<Self> {
        let mut random = [0_u8; 32];
        getrandom::fill(&mut random).context("generate capture token")?;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
        Self::from_header_value(HeaderValue::from_str(&encoded)?)
    }

    fn from_header_value(mut value: HeaderValue) -> Result<Self> {
        if value.is_empty() {
            bail!("tunnel token must not be empty");
        }
        value.set_sensitive(true);
        Ok(Self { value })
    }

    pub(crate) fn environment_value(&self) -> &str {
        self.value
            .to_str()
            .expect("validated capture tokens contain visible ASCII")
    }

    fn header_value(&self) -> HeaderValue {
        self.value.clone()
    }
}

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
    let hbone = local_hbone(args.hbone_endpoint, &token, Duration::from_secs(5)).await?;
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

pub async fn local_hbone(
    endpoint: SocketAddr,
    token: &CaptureToken,
    connect_timeout: Duration,
) -> Result<HboneClient> {
    let mut headers = HeaderMap::new();
    headers.insert(TUNNEL_TOKEN_HEADER, token.header_value());
    HboneClient::connect_with_headers(endpoint, headers, connect_timeout).await
}

pub fn original_authority(stream: &TcpStream) -> Result<Authority> {
    Ok(original_destination(stream)?.to_string().parse()?)
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

pub fn load_tunnel_token(path: &Path) -> Result<CaptureToken> {
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
    let value =
        HeaderValue::from_str(token.trim()).context("tunnel token is not a valid header value")?;
    CaptureToken::from_header_value(value)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::{CaptureToken, load_tunnel_token};

    #[test]
    fn generates_unique_sensitive_tokens() {
        let first = CaptureToken::generate().unwrap();
        let second = CaptureToken::generate().unwrap();
        assert_ne!(first.environment_value(), second.environment_value());
        assert!(first.header_value().is_sensitive());
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
