use std::path::Path;
use std::process::ExitStatus;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::time::{Instant, sleep};
use url::Url;

pub struct LocalGateway {
    child: Child,
}

impl LocalGateway {
    pub fn spawn(binary: &Path, config: &Path) -> Result<Self> {
        let mut command = Command::new(binary);
        command.arg("-f").arg(config).kill_on_drop(true);
        let child = command
            .spawn()
            .with_context(|| format!("failed to start Agent Gateway at {}", binary.display()))?;
        Ok(Self { child })
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
}
