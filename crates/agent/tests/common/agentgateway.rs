use std::time::{Duration, Instant};

use anyhow::Context;
use testcontainers::{ContainerAsync, GenericImage, ImageExt, runners::AsyncRunner};
use tokio::time::timeout;
use tracing::{info, warn};

// The image that `examples/` pulls as cr.agentgateway.dev/agentgateway:v1.6.0,
// from GitHub's registry and by digest: a CI pull through cr.agentgateway.dev
// failed with HTTP 525.
const IMAGE: &str = "ghcr.io/agentgateway/agentgateway";
const TAG: &str = "v1.6.0@sha256:9d3e6044ddcdc0878b1787f77bd401252b95e22684203fb5e874c4c42d2ed90c";
const CONFIG_PATH: &str = "/etc/agentgateway/config.yaml";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(60);
/// A registry error fails one attempt, not the test.
const START_ATTEMPTS: u64 = 3;

/// A real agentgateway on the host network, listening on `127.0.0.1:port`.
pub struct Agentgateway {
    inner: ContainerAsync<GenericImage>,
}

impl Agentgateway {
    /// Starts agentgateway with `config` and waits until `port` accepts
    /// connections.
    pub async fn start(config: &str, port: u16) -> anyhow::Result<Self> {
        let started = Instant::now();
        let mut attempt = 1;
        let inner = loop {
            let outcome = GenericImage::new(IMAGE, TAG)
                .with_cmd(["-f", CONFIG_PATH])
                .with_network("host")
                .with_copy_to(CONFIG_PATH, config.as_bytes().to_vec())
                .with_startup_timeout(STARTUP_TIMEOUT)
                .start()
                .await;
            match outcome {
                Ok(inner) => break inner,
                Err(error) if attempt < START_ATTEMPTS => {
                    warn!(%error, attempt, "agentgateway did not start; trying again");
                    tokio::time::sleep(Duration::from_secs(2 * attempt)).await;
                    attempt += 1;
                }
                Err(error) => {
                    return Err(error).context("start agentgateway (Docker is required)");
                }
            }
        };
        let gateway = Self { inner };
        let ready = timeout(STARTUP_TIMEOUT, async {
            while tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_err()
            {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await;
        if ready.is_err() {
            anyhow::bail!(
                "agentgateway did not listen on port {port}\n{}",
                gateway.logs().await
            );
        }
        info!(elapsed = ?started.elapsed(), port, "Agentgateway ready");
        Ok(gateway)
    }

    pub async fn logs(&self) -> String {
        let stdout = self.inner.stdout_to_vec().await.unwrap_or_default();
        let stderr = self.inner.stderr_to_vec().await.unwrap_or_default();
        format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        )
    }

    pub async fn stop(self) -> anyhow::Result<()> {
        timeout(Duration::from_secs(30), self.inner.rm())
            .await
            .context("agentgateway removal timed out")??;
        Ok(())
    }
}
