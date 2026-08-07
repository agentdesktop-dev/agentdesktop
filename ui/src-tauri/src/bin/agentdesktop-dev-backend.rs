use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use agentdesktop::{
    config::Config,
    proxy::{self, ProxyOptions},
};
use agentdesktop_ui::provider_credentials;
use anyhow::bail;
use clap::Parser;
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use zeroize::{Zeroize, Zeroizing};

const CREDENTIAL_CHECK_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Parser)]
#[command(about = "UI-local Agent Desktop development backend")]
struct Cli {
    #[command(flatten)]
    config: Config,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Cli::parse().config.validate()?;
    if config.identity_issuer.is_some() {
        bail!("the UI development backend does not manage identity; run the installed service");
    }
    #[cfg(target_os = "linux")]
    if config.capture_enabled {
        bail!("the UI development backend does not manage transparent capture");
    }

    let _telemetry = agentdesktop::telemetry::init()?;
    let gateway_task = match (&config.gateway_binary, &config.gateway_config) {
        (Some(binary), Some(gateway_config)) => Some(tokio::spawn(manage_gateway(
            binary.clone(),
            gateway_config.clone(),
        ))),
        _ => None,
    };

    let listener = TcpListener::bind(config.listen).await?;
    let serve = proxy::serve_with_identity(
        listener,
        config.upstream,
        config.mode,
        None,
        ProxyOptions {
            connect_timeout: Duration::from_millis(config.connect_timeout_ms),
            request_timeout: Duration::from_millis(config.request_timeout_ms),
            shutdown_timeout: Duration::from_millis(config.shutdown_timeout_ms),
            max_in_flight: config.max_in_flight,
        },
        shutdown_signal(),
    );

    let result = serve.await;
    if let Some(task) = gateway_task {
        task.abort();
    }
    result
}

async fn manage_gateway(binary: PathBuf, config: PathBuf) -> anyhow::Result<()> {
    let requires_provider_key = gateway_requires_provider_key(&config)?;
    let mut child: Option<Child> = None;
    let mut active_secret: Option<Zeroizing<Vec<u8>>> = None;

    loop {
        if child
            .as_mut()
            .is_some_and(|process| process.try_wait().ok().flatten().is_some())
        {
            child = None;
            active_secret = None;
        }

        let next_secret = provider_credentials::load()?;
        let environment_configured = provider_credentials::environment_is_configured();
        let should_run = !requires_provider_key || next_secret.is_some() || environment_configured;
        let credential_changed = secrets_differ(
            active_secret.as_ref().map(|value| value.as_slice()),
            next_secret.as_ref().map(|value| value.as_slice()),
        );

        if !should_run {
            stop_gateway(&mut child).await?;
            active_secret = None;
        } else if child.is_none() || credential_changed {
            stop_gateway(&mut child).await?;
            child = Some(spawn_gateway(
                &binary,
                &config,
                next_secret.as_ref().map(|value| value.as_slice()),
            )?);
            active_secret = next_secret;
        }

        tokio::time::sleep(CREDENTIAL_CHECK_INTERVAL).await;
    }
}

fn gateway_requires_provider_key(config: &Path) -> anyhow::Result<bool> {
    Ok(std::fs::read_to_string(config)?.contains("$ANTHROPIC_API_KEY"))
}

fn secrets_differ(current: Option<&[u8]>, next: Option<&[u8]>) -> bool {
    current != next
}

fn spawn_gateway(binary: &Path, config: &Path, secret: Option<&[u8]>) -> anyhow::Result<Child> {
    let mut command = Command::new(binary);
    command.arg("-f").arg(config).kill_on_drop(true);
    if let Some(secret) = secret {
        let mut value = Zeroizing::new(String::from_utf8(secret.to_vec())?);
        command.env("ANTHROPIC_API_KEY", value.as_str());
        let child = command.spawn()?;
        value.zeroize();
        return Ok(child);
    }
    Ok(command.spawn()?)
}

async fn stop_gateway(child: &mut Option<Child>) -> anyhow::Result<()> {
    if let Some(process) = child
        && process.try_wait()?.is_none()
    {
        process.kill().await?;
    }
    *child = None;
    Ok(())
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        let _ = error;
    }
}

#[cfg(test)]
mod tests {
    use super::{gateway_requires_provider_key, spawn_gateway};

    #[test]
    fn bundled_gateway_accepts_claude_model_ids_unchanged() {
        let config = include_str!("../../../config/agentgateway-anthropic.yaml");
        assert!(config.contains("- name: \"*\""));
        assert!(!config.contains("anthropic/*"));
        assert!(!config.contains("stripPrefix"));
    }

    #[test]
    fn detects_anthropic_environment_reference() {
        let temporary = tempfile::tempdir().unwrap();
        let config = temporary.path().join("config.yaml");
        std::fs::write(&config, "apiKey: $ANTHROPIC_API_KEY\n").unwrap();
        assert!(gateway_requires_provider_key(&config).unwrap());

        std::fs::write(&config, "directResponse: ok\n").unwrap();
        assert!(!gateway_requires_provider_key(&config).unwrap());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn injects_provider_secret_into_owned_gateway_process() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let gateway = temporary.path().join("agentgateway");
        let output = temporary.path().join("credential-output");
        std::fs::write(
            &gateway,
            "#!/bin/sh\nprintf '%s' \"$ANTHROPIC_API_KEY\" > \"$2\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&gateway, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut child =
            spawn_gateway(&gateway, &output, Some(b"sk-ant-test-provider-credential")).unwrap();
        assert!(child.wait().await.unwrap().success());
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            "sk-ant-test-provider-credential"
        );
    }
}
