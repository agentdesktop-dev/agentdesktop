#[cfg(target_os = "linux")]
use std::net::SocketAddr;
use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use anyhow::{Context, Result, bail};
#[cfg(target_os = "linux")]
use base64::Engine as _;
#[cfg(target_os = "linux")]
use http::HeaderMap;
use http::header::HeaderValue;
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep};
use url::Url;

#[cfg(target_os = "linux")]
use crate::service::hbone::HboneClient;

#[cfg(target_os = "linux")]
pub const TUNNEL_TOKEN_HEADER: &str = "x-agentdesktop-token";
#[cfg(target_os = "linux")]
pub const TUNNEL_TOKEN_ENV: &str = "AGENTDESKTOP_CAPTURE_TOKEN";

#[cfg(target_os = "linux")]
pub struct GatewayCapability {
    value: HeaderValue,
}

pub(crate) fn validate_capability(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 256
        || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        bail!("local Gateway capability is invalid");
    }
    HeaderValue::from_str(value).context("local Gateway capability is not a valid header value")?;
    Ok(())
}

#[cfg(target_os = "linux")]
impl GatewayCapability {
    pub fn generate() -> Result<Self> {
        let mut random = [0_u8; 32];
        getrandom::fill(&mut random).context("generate local Gateway capability")?;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
        Self::from_str(&encoded)
    }

    pub(crate) fn from_str(value: &str) -> Result<Self> {
        validate_capability(value)?;
        let mut value = HeaderValue::from_str(value)
            .context("local Gateway capability is not a valid header value")?;
        value.set_sensitive(true);
        Ok(Self { value })
    }

    pub(crate) fn environment_value(&self) -> &str {
        self.value
            .to_str()
            .expect("validated local Gateway capabilities contain visible ASCII")
    }

    fn header_value(&self) -> HeaderValue {
        self.value.clone()
    }
}

#[cfg(target_os = "linux")]
pub(crate) async fn connect(
    endpoint: SocketAddr,
    capability: &GatewayCapability,
    connect_timeout: Duration,
) -> Result<HboneClient> {
    let mut headers = HeaderMap::new();
    headers.insert(TUNNEL_TOKEN_HEADER, capability.header_value());
    HboneClient::connect_with_headers(endpoint, headers, connect_timeout).await
}

#[cfg(target_os = "linux")]
pub(crate) async fn connect_with_capability(
    endpoint: SocketAddr,
    capability: &str,
    connect_timeout: Duration,
) -> Result<HboneClient> {
    connect(
        endpoint,
        &GatewayCapability::from_str(capability)?,
        connect_timeout,
    )
    .await
}

pub struct LocalGateway {
    child: Child,
    #[cfg(target_os = "linux")]
    capability: GatewayCapability,
}

impl LocalGateway {
    pub fn spawn(binary: &Path, config: &Path) -> Result<Self> {
        #[cfg(target_os = "linux")]
        let capability = GatewayCapability::generate()?;
        let mut command = Command::new(binary);
        command.arg("-f").arg(config).kill_on_drop(true);
        #[cfg(target_os = "linux")]
        command.env(TUNNEL_TOKEN_ENV, capability.environment_value());
        let child = command
            .spawn()
            .with_context(|| format!("failed to start Agent Gateway at {}", binary.display()))?;
        Ok(Self {
            child,
            #[cfg(target_os = "linux")]
            capability,
        })
    }

    #[cfg(target_os = "linux")]
    pub fn capability(&self) -> &GatewayCapability {
        &self.capability
    }

    pub async fn wait(&mut self) -> Result<ExitStatus> {
        self.child
            .wait()
            .await
            .context("failed to wait for local Agent Gateway")
    }

    pub async fn wait_until_reachable(&mut self, upstream: &Url, timeout: Duration) -> Result<()> {
        let host = upstream
            .host_str()
            .context("local Agent Gateway upstream has no host")?;
        let port = upstream
            .port_or_known_default()
            .context("local Agent Gateway upstream has no port")?;
        let deadline = Instant::now() + timeout;

        loop {
            if let Some(status) = self.child.try_wait()? {
                bail!("local Agent Gateway exited during startup with {status}");
            }
            if TcpStream::connect((host, port)).await.is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "local Agent Gateway did not become reachable at {host}:{port} within {} seconds",
                    timeout.as_secs()
                );
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    pub async fn stop(&mut self) -> Result<()> {
        if self.child.try_wait()?.is_none() {
            self.child
                .kill()
                .await
                .context("failed to stop local Agent Gateway")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use std::fs;
    #[cfg(target_os = "linux")]
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::LocalGateway;

    #[cfg(target_os = "linux")]
    #[test]
    fn generated_capability_is_sensitive() {
        let capability = super::GatewayCapability::generate().unwrap();
        assert!(capability.value.is_sensitive());
    }

    #[test]
    fn reports_spawn_failure() {
        let error = LocalGateway::spawn(
            Path::new("/path/that/does/not/exist/agentgateway"),
            Path::new("config.yaml"),
        )
        .err()
        .expect("spawn should fail");

        assert!(error.to_string().contains("failed to start Agent Gateway"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn injects_and_retains_capture_token_for_gateway_generation() {
        let temporary = tempfile::tempdir().unwrap();
        let gateway = temporary.path().join("agentgateway");
        let output = temporary.path().join("capture-token");
        fs::write(
            &gateway,
            "#!/bin/sh\nprintf '%s' \"$AGENTDESKTOP_CAPTURE_TOKEN\" > \"$2\"\n",
        )
        .unwrap();
        fs::set_permissions(&gateway, fs::Permissions::from_mode(0o700)).unwrap();

        let mut process = LocalGateway::spawn(&gateway, &output).unwrap();
        let expected = process.capability().environment_value().to_owned();
        process.wait().await.unwrap();

        assert!(!expected.is_empty());
        assert_eq!(fs::read_to_string(output).unwrap(), expected);
    }
}
