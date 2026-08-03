use agentdesktop::apps::claude::{ConnectionStatus, connect_installed, is_installed};
use agentdesktop::config::{Config, upstream_origin};
use agentdesktop::identity::oauth::{ManagedIdentity, load_session_for};
use agentdesktop::identity::{
    cli::IdentityCommand,
    enrollment::{
        DeviceStatus, EnrollmentClient, EnrollmentStatus, load_enrollment_for, save_enrollment_for,
    },
    oauth::{LoginConfig, login},
    storage::{CredentialStorageMode, CredentialStore, default_storage_root},
};
use agentdesktop::local_gateway::LocalGateway;
use agentdesktop::organization::OrganizationBootstrap;
use agentdesktop::proxy::{self, ProxyOptions};
use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;

const LOCAL_GATEWAY_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Parser)]
#[command(version, about = "Route AI application traffic through Agent Gateway")]
struct Cli {
    #[command(subcommand)]
    command: ConnectorCommand,
}

#[derive(Debug, Subcommand)]
enum ConnectorCommand {
    /// Run the application-facing forwarding service.
    Serve(Config),
    /// Connect supported AI agents to the installed service.
    ConnectAgents {
        #[arg(long, help = "Connect supported agents without prompting")]
        yes: bool,
    },
    /// Configure managed identity for Agent Desktop.
    Identity {
        #[command(subcommand)]
        command: IdentityCommand,
    },
    /// Relay redirected Linux TCP flows over HBONE.
    #[cfg(target_os = "linux")]
    Capture(agentdesktop::capture::CaptureArgs),
    /// Run a command tree in an Agent Desktop execution scope.
    #[cfg(target_os = "linux")]
    Launch(agentdesktop::launch::LaunchArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<ExitCode> {
    let status = match Cli::parse().command {
        ConnectorCommand::Serve(config) => {
            serve(config.validate()?).await?;
            None
        }
        ConnectorCommand::ConnectAgents { yes } => {
            connect_agents(yes).await?;
            None
        }
        ConnectorCommand::Identity { command } => {
            agentdesktop::identity::cli::run(command).await?;
            None
        }
        #[cfg(target_os = "linux")]
        ConnectorCommand::Capture(args) => {
            let _telemetry = agentdesktop::telemetry::init()?;
            agentdesktop::capture::run(args).await?;
            None
        }
        #[cfg(target_os = "linux")]
        ConnectorCommand::Launch(args) => Some(agentdesktop::launch::run(args)?),
    };
    Ok(status.map_or(ExitCode::SUCCESS, exit_code))
}

fn exit_code(status: ExitStatus) -> ExitCode {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from)
}

async fn serve(config: Config) -> anyhow::Result<()> {
    let _telemetry = agentdesktop::telemetry::init()?;
    let identity = if let Some(issuer) = &config.identity_issuer {
        let identity_dir = config
            .identity_dir
            .clone()
            .map_or_else(default_storage_root, Ok)?;
        let store = CredentialStore::load(&identity_dir)?;
        let session = load_session_for(issuer, &upstream_origin(&config.upstream)?, &store)?;
        Some(ManagedIdentity::new(session, store))
    } else {
        None
    };
    let mut local_gateway = match (&config.gateway_binary, &config.gateway_config) {
        (Some(binary), Some(gateway_config)) => {
            tracing::info!(event = "local_gateway_starting");
            Some(LocalGateway::spawn(binary, gateway_config)?)
        }
        _ => None,
    };
    if let Some(gateway) = &mut local_gateway {
        gateway
            .wait_until_reachable(&config.upstream, LOCAL_GATEWAY_STARTUP_TIMEOUT)
            .await?;
        tracing::info!(event = "local_gateway_ready");
    }
    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(
        event = "connector_started",
        mode = config.mode.as_str(),
        listen = %listener.local_addr()?
    );
    let serve = proxy::serve_with_identity(
        listener,
        config.upstream,
        config.mode,
        identity,
        ProxyOptions {
            connect_timeout: std::time::Duration::from_millis(config.connect_timeout_ms),
            request_timeout: std::time::Duration::from_millis(config.request_timeout_ms),
            shutdown_timeout: std::time::Duration::from_millis(config.shutdown_timeout_ms),
            max_in_flight: config.max_in_flight,
        },
        shutdown_signal(),
    );
    if let Some(gateway) = &mut local_gateway {
        tokio::select! {
            result = serve => {
                gateway.stop().await?;
                result
            }
            status = gateway.wait() => {
                bail!("local Agent Gateway exited unexpectedly with {}", status?);
            }
        }
    } else {
        serve.await
    }
}

async fn connect_agents(yes: bool) -> anyhow::Result<()> {
    if let Some((root, bootstrap)) = installed_managed_bootstrap()? {
        prepare_managed_connection(&root, &bootstrap)
            .await
            .with_context(|| {
                format!(
                    "managed setup could not finish; contact {}",
                    bootstrap.organization.support_url
                )
            })?;
    }
    if !is_installed()? {
        println!("No supported AI agents were found.");
        return Ok(());
    }
    if !yes {
        use std::io::Write;

        println!("Claude Code was found.");
        println!("This will update your Claude Code settings so requests use Agent Desktop.");
        print!("Connect Claude Code? [Y/n] ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        let bytes_read = std::io::stdin().read_line(&mut answer)?;
        if bytes_read == 0
            || !matches!(
                answer.trim().to_ascii_lowercase().as_str(),
                "" | "y" | "yes"
            )
        {
            println!("No agents were changed.");
            return Ok(());
        }
    }
    match connect_installed()? {
        ConnectionStatus::Connected => println!("Claude Code connected."),
        ConnectionStatus::AlreadyConnected => println!("Claude Code is already connected."),
        ConnectionStatus::NotInstalled => println!("No supported AI agents were found."),
    }
    Ok(())
}

fn installed_managed_bootstrap() -> anyhow::Result<Option<(PathBuf, OrganizationBootstrap)>> {
    let executable = std::env::current_exe()?;
    let Some(root) = executable.parent().and_then(Path::parent) else {
        return Ok(None);
    };
    let path = root.join("share/organization.json");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some((
        root.to_owned(),
        OrganizationBootstrap::parse(&std::fs::read(path)?)?,
    )))
}

async fn prepare_managed_connection(
    root: &Path,
    bootstrap: &OrganizationBootstrap,
) -> anyhow::Result<()> {
    let gateway_origin = bootstrap.gateway.url.clone();
    let storage_root = default_storage_root()?;
    let store = if storage_root.exists() {
        CredentialStore::load(&storage_root)?
    } else {
        CredentialStore::setup(CredentialStorageMode::Auto, &storage_root)?
    };
    let session = match load_session_for(&bootstrap.identity.issuer, &gateway_origin, &store) {
        Ok(session) => session,
        Err(_) => {
            println!(
                "Opening your {} sign-in in the browser...",
                bootstrap.organization.display_name
            );
            login(
                &LoginConfig {
                    issuer: bootstrap.identity.issuer.clone(),
                    client_id: bootstrap.identity.client_id.clone(),
                    audience: bootstrap.identity.audience.clone(),
                    scope: bootstrap.identity.scope.clone(),
                    gateway_origin: gateway_origin.clone(),
                },
                &store,
                |authorization_url| open::that(authorization_url.as_str()).map_err(Into::into),
            )
            .await?
        }
    };
    let identity = ManagedIdentity::new(session, store.clone());
    let client = EnrollmentClient::discover(&bootstrap.identity.issuer).await?;
    let thumbprint = identity.dpop_thumbprint().await?;
    let enrollment = match load_enrollment_for(
        &bootstrap.identity.issuer,
        &gateway_origin,
        &thumbprint,
        &store,
    ) {
        Ok(enrollment) => enrollment,
        Err(_) => {
            let enrollment = client.request(&identity).await?;
            save_enrollment_for(
                &bootstrap.identity.issuer,
                &gateway_origin,
                &store,
                &enrollment,
            )?;
            enrollment
        }
    };
    let deadline = Instant::now() + Duration::from_secs(300);
    let enrollment_id = enrollment.enrollment_id;
    let mut announced = false;
    loop {
        let enrollment = client.status(&identity, &enrollment_id).await?;
        save_enrollment_for(
            &bootstrap.identity.issuer,
            &gateway_origin,
            &store,
            &enrollment,
        )?;
        match (enrollment.status, enrollment.device_status) {
            (EnrollmentStatus::Approved, Some(DeviceStatus::Active)) => break,
            (EnrollmentStatus::Approved, Some(DeviceStatus::Revoked)) => {
                anyhow::bail!("this device is no longer approved by your organization")
            }
            (EnrollmentStatus::Pending, _) if Instant::now() < deadline => {
                if !announced {
                    println!("Waiting for your organization to approve this device...");
                    announced = true;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            (EnrollmentStatus::Pending, _) => {
                anyhow::bail!(
                    "device approval did not complete; contact {}",
                    bootstrap.organization.support_url
                )
            }
            _ => anyhow::bail!("the enrollment authority returned an invalid device state"),
        }
    }

    let control = root.join("bin/agentdesktop-install");
    let status = Command::new(control)
        .args(["service", "enable", "--root"])
        .arg(root)
        .status()?;
    if !status.success() {
        anyhow::bail!("could not start Agent Desktop");
    }
    wait_for_managed_health().await?;
    println!("Agent Desktop is ready.");
    Ok(())
}

async fn wait_for_managed_health() -> anyhow::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let client = reqwest::Client::new();
    while Instant::now() < deadline {
        if client
            .get("http://127.0.0.1:8080/_agentdesktop/healthz")
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("Agent Desktop did not become ready")
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        let _ = error;
        tracing::error!(event = "shutdown_signal_failed");
    }
}
