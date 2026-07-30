use std::path::Path;
use std::process::ExitStatus;

use anyhow::{Context, Result};
use tokio::process::{Child, Command};

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
