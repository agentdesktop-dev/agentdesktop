use std::{
    cell::{Cell, RefCell},
    fs,
    panic::AssertUnwindSafe,
    path::Path,
    time::{Duration, Instant},
};

use anyhow::{Context, ensure};
use futures_util::FutureExt;
use tempfile::TempDir;
use testcontainers::{
    ContainerAsync, GenericBuildableImage, GenericImage, ImageExt,
    core::{AccessMode, BuildImageOptions, ExecCommand, Mount},
    runners::{AsyncBuilder, AsyncRunner},
};
use tokio::time::timeout;
use tracing::{Instrument, debug, error, info, info_span};

const BASE_IMAGE: &str =
    "istio/base@sha256:cab6852ff5ae39349136f41af6ee892a228c8fb9634ec25e7550bd9b517a7a93";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const BUILD_TIMEOUT: Duration = Duration::from_secs(15 * 60);

pub struct Container {
    inner: ContainerAsync<GenericImage>,
    artifacts: TempDir,
    next_command: Cell<usize>,
    processes: RefCell<Vec<String>>,
}

impl Container {
    pub async fn run(
        provider: &str,
        dockerfile: &str,
        scenario: impl AsyncFnOnce(&Self) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        super::setup_logging();
        async {
            let started = Instant::now();
            let artifacts = tempfile::Builder::new().prefix("agentdesktop-provider-").tempdir()?;
            let result = Self::start(provider, dockerfile).await;
            let inner = match result {
                Ok(inner) => inner,
                Err(error) => {
                    fs::write(artifacts.path().join("startup-error"), format!("{error:#}"))?;
                    return Err(error.context(format!("Provider test artifacts: {}", artifacts.keep().display())));
                }
            };
            let container = Self { inner, artifacts, next_command: Cell::new(0), processes: RefCell::new(Vec::new()) };
            let outcome = AssertUnwindSafe(async {
                container.exec(&["agentdesktop", "--version"]).await
                    .context("mounted daemon must match the container architecture and libc")?;
                scenario(&container).await
            }).catch_unwind().await;
            if !matches!(outcome, Ok(Ok(()))) {
                container.collect_logs().await;
            }
            let Self { inner, artifacts, .. } = container;
            let cleanup_started = Instant::now();
            let cleanup = timeout(COMMAND_TIMEOUT, inner.rm()).await;
            info!(elapsed = ?cleanup_started.elapsed(), success = matches!(cleanup, Ok(Ok(()))), "Container removal finished");
            let result = match outcome {
                Ok(result) => result,
                Err(panic) => {
                    let path = artifacts.keep();
                    error!(artifacts = %path.display(), "Provider test panicked");
                    std::panic::resume_unwind(panic);
                }
            };
            let result = result.and_then(|()| { cleanup.context("container removal timed out")??; Ok(()) });
            if let Err(error) = result {
                let path = artifacts.keep();
                error!(artifacts = %path.display(), elapsed = ?started.elapsed(), "Provider test failed");
                return Err(error.context(format!("Provider test artifacts: {}", path.display())));
            }
            info!(elapsed = ?started.elapsed(), "Provider test passed");
            Ok(())
        }.instrument(info_span!("provider_test", provider)).await
    }

    async fn start(
        provider: &str,
        dockerfile: &str,
    ) -> anyhow::Result<ContainerAsync<GenericImage>> {
        ensure!(
            cfg!(target_os = "linux"),
            "provider integration tests require a Linux-built daemon binary"
        );
        let daemon = Path::new(env!("CARGO_BIN_EXE_agentdesktop-headless")).canonicalize()?;
        let dockerfile = fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(dockerfile),
        )?;
        let started = Instant::now();
        info!("Building test image with Docker cache");
        let image = timeout(
            BUILD_TIMEOUT,
            GenericBuildableImage::new(format!("agentdesktop-provider-{provider}"), "test")
                .with_dockerfile_string(dockerfile)
                .build_image_with(
                    BuildImageOptions::new().with_build_arg("BASE_IMAGE", BASE_IMAGE),
                ),
        )
        .await;
        info!(elapsed = ?started.elapsed(), success = matches!(image, Ok(Ok(_))), "Docker build finished");
        let image = image
            .context("Docker build timed out")?
            .context("build provider image (Docker is required)")?;
        let started = Instant::now();
        let container = image
            .with_network("host")
            .with_mount(
                Mount::bind_mount(daemon.to_string_lossy(), "/usr/local/bin/agentdesktop")
                    .with_access_mode(AccessMode::ReadOnly),
            )
            .with_host_config_modifier(|config| config.init = Some(true))
            .with_startup_timeout(COMMAND_TIMEOUT)
            .start()
            .await?;
        info!(elapsed = ?started.elapsed(), "Container started");
        Ok(container)
    }

    pub async fn exec(&self, args: &[&str]) -> anyhow::Result<String> {
        self.exec_as("root", args, COMMAND_TIMEOUT).await
    }

    pub async fn exec_as(
        &self,
        user: &str,
        args: &[&str],
        deadline: Duration,
    ) -> anyhow::Result<String> {
        // Bound the process itself as well as the Docker API request.
        let seconds = deadline.as_secs().max(1).to_string();
        let mut command = vec![
            "timeout",
            "--signal=KILL",
            &seconds,
            "runuser",
            "--preserve-environment",
            "-u",
            user,
            "--",
        ];
        command.extend_from_slice(args);
        let sequence = self.next_command.get();
        self.next_command.set(sequence + 1);
        let prefix = self.artifacts.path().join(format!("{sequence:03}"));
        fs::write(prefix.with_extension("command"), format!("{command:?}"))?;
        let started = Instant::now();
        let result = timeout(deadline + Duration::from_secs(2), async {
            let mut output = self.inner.exec(ExecCommand::new(command)).await?;
            // Reading to EOF waits for exit without CmdWaitFor::exit’s 500 ms polling.
            let stdout = output.stdout_to_vec().await?;
            let stderr = output.stderr_to_vec().await?;
            fs::write(prefix.with_extension("stdout"), &stdout)?;
            fs::write(prefix.with_extension("stderr"), &stderr)?;
            let status = output.exit_code().await?;
            let stdout = String::from_utf8_lossy(&stdout).into_owned();
            let stderr = String::from_utf8_lossy(&stderr);
            ensure!(
                status == Some(0),
                "{args:?} failed ({status:?})\nstdout:\n{}\nstderr:\n{}",
                tail(&stdout),
                tail(&stderr)
            );
            Ok::<_, anyhow::Error>(stdout)
        })
        .await
        .context("container command timed out")?;
        if started.elapsed() >= Duration::from_secs(1) {
            info!(command = ?args, elapsed = ?started.elapsed(), success = result.is_ok(), "Slow command finished");
        } else {
            debug!(command = ?args, elapsed = ?started.elapsed(), success = result.is_ok(), "Command finished");
        }
        result
    }

    pub async fn write(&self, path: &str, contents: &str) -> anyhow::Result<()> {
        self.exec(&[
            "sh",
            "-c",
            "mkdir -p -- \"$(dirname -- \"$1\")\" && printf '%s' \"$2\" > \"$1\"",
            "write",
            path,
            contents,
        ])
        .await?;
        Ok(())
    }

    pub async fn read(&self, path: &str) -> anyhow::Result<String> {
        self.exec(&["cat", "--", path]).await
    }

    pub async fn start_process(&self, name: &str, args: &[&str]) -> anyhow::Result<()> {
        let started = Instant::now();
        let log = format!("/tmp/{name}");
        self.processes.borrow_mut().push(log.clone());
        let mut command = vec![
            "sh",
            "-c",
            "nohup sh -c 'echo $$ > \"$0.pid\"; exec \"$@\"' \"$@\" > \"$1.log\" 2>&1 < /dev/null &",
            "start",
            &log,
        ];
        command.extend_from_slice(args);
        self.exec(&command).await?;
        info!(process = name, elapsed = ?started.elapsed(), "Process launched");
        Ok(())
    }

    pub async fn stop_process(&self, name: &str) -> anyhow::Result<()> {
        let started = Instant::now();
        let pid = self.read(&format!("/tmp/{name}.pid")).await?;
        self.exec(&["sh", "-c", "kill -INT \"$1\"", "stop", pid.trim()])
            .await?;
        self.exec(&[
            "timeout",
            "10",
            "sh",
            "-c",
            "while kill -0 \"$1\" 2>/dev/null; do sleep 0.1; done",
            "wait",
            pid.trim(),
        ])
        .await?;
        info!(process = name, elapsed = ?started.elapsed(), "Process stopped");
        Ok(())
    }

    pub async fn wait_ready(&self, args: &[&str]) -> anyhow::Result<()> {
        let started = Instant::now();
        timeout(COMMAND_TIMEOUT, async {
            loop {
                if self
                    .exec_as("root", args, Duration::from_secs(3))
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("service did not become ready")?;
        info!(command = ?args, elapsed = ?started.elapsed(), "Service ready");
        Ok(())
    }

    async fn collect_logs(&self) {
        if let Ok(Ok(logs)) = timeout(Duration::from_secs(5), self.inner.stdout_to_vec()).await {
            let _ = fs::write(self.artifacts.path().join("container.stdout"), logs);
        }
        if let Ok(Ok(logs)) = timeout(Duration::from_secs(5), self.inner.stderr_to_vec()).await {
            let _ = fs::write(self.artifacts.path().join("container.stderr"), logs);
        }
        let processes = self.processes.borrow().clone();
        for process in processes {
            let _ = self
                .exec_as(
                    "root",
                    &["cat", &format!("{process}.log")],
                    Duration::from_secs(5),
                )
                .await;
        }
    }
}

fn tail(output: &str) -> String {
    let mut lines: Vec<_> = output.lines().rev().take(60).collect();
    lines.reverse();
    lines.join("\n")
}
