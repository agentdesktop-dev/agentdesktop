use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use agentdesktop::{
    config::{Config, DeploymentMode, upstream_origin},
    identity::{
        enrollment::load_client_identity_for,
        storage::{CredentialStore, default_storage_root},
    },
    service,
};
use agentdesktop_ui::provider_credentials;
use anyhow::Context;
use clap::Parser;
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
    let mut config = Cli::parse().config.validate()?;
    #[cfg(target_os = "linux")]
    if config.capture_enabled {
        anyhow::bail!("the UI development backend does not manage transparent capture");
    }

    if config.mode == DeploymentMode::Managed {
        wait_for_managed_identity(&config).await?;
    }
    let gateway_task = take_development_gateway(&mut config)
        .map(|(binary, gateway_config)| tokio::spawn(manage_gateway(binary, gateway_config)));
    if let Some(mut gateway_task) = gateway_task {
        tokio::select! {
            result = service::run(config) => {
                gateway_task.abort();
                result
            }
            result = &mut gateway_task => {
                result.context("development Gateway task failed")??;
                anyhow::bail!("development Gateway task exited unexpectedly")
            }
        }
    } else {
        service::run(config).await
    }
}

fn take_development_gateway(config: &mut Config) -> Option<(PathBuf, PathBuf)> {
    config
        .gateway_binary
        .take()
        .zip(config.gateway_config.take())
}

async fn wait_for_managed_identity(config: &Config) -> anyhow::Result<()> {
    let issuer = config
        .identity_issuer
        .as_ref()
        .context("managed development requires an identity issuer")?;
    let gateway_origin = upstream_origin(&config.upstream)?;
    let identity_root = config
        .identity_dir
        .clone()
        .map_or_else(default_storage_root, Ok)?;
    eprintln!("[desktop] waiting for managed device approval");
    loop {
        if credential_store_is_initialized(&identity_root)
            && let Ok(store) = CredentialStore::load(&identity_root)
            && load_client_identity_for(issuer, &gateway_origin, &store).is_ok()
        {
            eprintln!("[desktop] managed device identity is ready");
            return Ok(());
        }
        tokio::time::sleep(CREDENTIAL_CHECK_INTERVAL).await;
    }
}

fn credential_store_is_initialized(identity_root: &Path) -> bool {
    identity_root.join("credential-storage").is_file()
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

#[cfg(test)]
mod tests {
    use super::{credential_store_is_initialized, gateway_requires_provider_key, spawn_gateway};

    #[test]
    fn does_not_open_an_uninitialized_managed_credential_store() {
        let temporary = tempfile::tempdir().unwrap();
        assert!(!credential_store_is_initialized(temporary.path()));

        std::fs::write(temporary.path().join("credential-storage"), b"file\n").unwrap();
        assert!(credential_store_is_initialized(temporary.path()));
    }

    #[test]
    fn bundled_gateway_routes_models_without_rewriting() {
        let config = include_str!("../../../config/agentgateway-anthropic.yaml");
        assert!(config.contains("tunnelProtocol: connect"));
        assert!(config.contains("anthropic: {}"));
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
