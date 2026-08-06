use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep};
use url::Url;

#[cfg(target_os = "linux")]
use crate::service::capture::{CaptureToken, TUNNEL_TOKEN_ENV};

pub struct LocalGateway {
    child: Child,
    #[cfg(target_os = "linux")]
    capture_token: CaptureToken,
}

impl LocalGateway {
    pub fn spawn(binary: &Path, config: &Path) -> Result<Self> {
        #[cfg(target_os = "linux")]
        let capture_token = CaptureToken::generate()?;
        let mut command = Command::new(binary);
        command.arg("-f").arg(config).kill_on_drop(true);
        #[cfg(target_os = "linux")]
        command.env(TUNNEL_TOKEN_ENV, capture_token.environment_value());
        let child = command
            .spawn()
            .with_context(|| format!("failed to start Agent Gateway at {}", binary.display()))?;
        Ok(Self {
            child,
            #[cfg(target_os = "linux")]
            capture_token,
        })
    }

    #[cfg(target_os = "linux")]
    pub fn capture_token(&self) -> &CaptureToken {
        &self.capture_token
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
        let expected = process.capture_token().environment_value().to_owned();
        process.wait().await.unwrap();

        assert!(!expected.is_empty());
        assert_eq!(fs::read_to_string(output).unwrap(), expected);
    }
}
