use agentdesktop::apps::claude::{ConnectionStatus, connect_installed, is_installed};
use agentdesktop::config::{Config, upstream_origin};
use agentdesktop::identity::oauth::{ManagedIdentity, load_session_for};
use agentdesktop::identity::{
    cli::IdentityCommand,
    enrollment::{
        EnrollmentClient, EnrollmentStatus, certificate_expired, certificate_renewal_due,
        load_device_identity_for, load_enrollment_for, save_enrollment_for,
    },
    oauth::{LoginConfig, login},
    storage::{CredentialStorageMode, CredentialStore, default_storage_root},
};
use agentdesktop::local_gateway::LocalGateway;
use agentdesktop::organization::OrganizationBootstrap;
use agentdesktop::proxy::{self, ManagedDeviceIdentity, ProxyOptions};
use anyhow::{Context, bail};
use clap::{Parser, Subcommand, ValueEnum};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, ExitStatus};
use std::time::{Duration, Instant, SystemTime};
use tokio::net::TcpListener;

#[cfg(target_os = "linux")]
use agentdesktop::capture::CaptureRelay;

const LOCAL_GATEWAY_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const CERTIFICATE_RENEW_BEFORE: Duration = Duration::from_secs(6 * 60 * 60);
const CERTIFICATE_RENEWAL_CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);
const CERTIFICATE_RENEWAL_RETRY_INTERVAL: Duration = Duration::from_secs(60);

struct RenewalContext {
    enrollment_url: url::Url,
    gateway_origin: url::Url,
    identity: ManagedIdentity,
    issuer: url::Url,
    proxy_identity: ManagedDeviceIdentity,
    store: CredentialStore,
}

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
    /// Install or remove trust for local Agent Gateway inspection.
    #[cfg(target_os = "linux")]
    Trust {
        #[arg(value_enum)]
        action: TrustAction,
    },
    #[cfg(target_os = "linux")]
    #[command(name = "_launch-child", hide = true)]
    LaunchChild(agentdesktop::launch::LaunchChildArgs),
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, ValueEnum)]
enum TrustAction {
    Install,
    Remove,
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
        #[cfg(target_os = "linux")]
        ConnectorCommand::Trust { action } => {
            manage_inspection_trust(action)?;
            None
        }
        #[cfg(target_os = "linux")]
        ConnectorCommand::LaunchChild(args) => {
            agentdesktop::launch::run_child(args)?;
            None
        }
    };
    Ok(status.map_or(ExitCode::SUCCESS, exit_code))
}

#[cfg(target_os = "linux")]
fn manage_inspection_trust(action: TrustAction) -> anyhow::Result<()> {
    let config_root = std::env::var_os("XDG_CONFIG_HOME").map_or_else(
        || {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .context("HOME is not set")
                .map(|home| home.join(".config"))
        },
        |root| Ok(PathBuf::from(root)),
    )?;
    let certificate = config_root.join("agentgateway/inspection-ca/ca.crt");
    let contents = std::fs::read(&certificate).with_context(|| {
        format!(
            "local inspection CA was not found at {}; reinstall Agent Desktop first",
            certificate.display()
        )
    })?;
    let fingerprint = Sha256::digest(&contents)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    println!("Local Agent Gateway inspection CA");
    println!("SHA-256: {fingerprint}");
    println!(
        "Action: {} system trust for apps explicitly launched through Agent Desktop",
        match action {
            TrustAction::Install => "install",
            TrustAction::Remove => "remove",
        }
    );
    let helper_action = match action {
        TrustAction::Install => "trust-install",
        TrustAction::Remove => "trust-remove",
    };
    let status = Command::new("pkexec")
        .arg("/usr/libexec/agentdesktop-capture-setup")
        .arg(helper_action)
        .arg("--certificate")
        .arg(&certificate)
        .status()
        .context("authorize inspection trust change")?;
    if !status.success() {
        bail!("inspection trust change failed with {status}");
    }
    Ok(())
}

fn exit_code(status: ExitStatus) -> ExitCode {
    status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .map_or(ExitCode::FAILURE, ExitCode::from)
}

async fn serve(config: Config) -> anyhow::Result<()> {
    let _telemetry = agentdesktop::telemetry::init()?;
    let (identity, device_identity, renewal_context) = if let Some(issuer) = &config.identity_issuer
    {
        let identity_dir = config
            .identity_dir
            .clone()
            .map_or_else(default_storage_root, Ok)?;
        let store = CredentialStore::load(&identity_dir)?;
        let gateway_origin = upstream_origin(&config.upstream)?;
        let session = load_session_for(issuer, &gateway_origin, &store)?;
        let identity = ManagedIdentity::new(session, store.clone());
        let device_identity =
            ManagedDeviceIdentity::new(load_device_identity_for(issuer, &gateway_origin, &store)?);
        let enrollment_url = config
            .enrollment_url
            .clone()
            .context("managed identity requires an enrollment URL")?;
        (
            Some(identity.clone()),
            Some(device_identity.clone()),
            Some(RenewalContext {
                enrollment_url,
                gateway_origin,
                identity,
                issuer: issuer.clone(),
                proxy_identity: device_identity,
                store,
            }),
        )
    } else {
        (None, None, None)
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
    #[cfg(target_os = "linux")]
    let mut capture_relay = if config.capture_enabled {
        let gateway = local_gateway
            .as_ref()
            .context("capture requires an owned local Agent Gateway")?;
        let relay = CaptureRelay::start(
            "127.0.0.1:15001".parse()?,
            "127.0.0.1:15008".parse()?,
            gateway.capture_token(),
            config.max_in_flight,
        )
        .await?;
        tracing::info!(event = "capture_relay_ready", listen = %relay.local_addr()?);
        Some(tokio::spawn(relay.serve(std::future::pending())))
    } else {
        None
    };
    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(
        event = "connector_started",
        mode = config.mode.as_str(),
        listen = %listener.local_addr()?
    );
    let renewal_task =
        renewal_context.map(|context| tokio::spawn(renew_device_certificate(context)));
    let serve = proxy::serve_with_rotating_managed_identity(
        listener,
        config.upstream,
        config.mode,
        identity,
        device_identity,
        ProxyOptions {
            connect_timeout: std::time::Duration::from_millis(config.connect_timeout_ms),
            request_timeout: std::time::Duration::from_millis(config.request_timeout_ms),
            shutdown_timeout: std::time::Duration::from_millis(config.shutdown_timeout_ms),
            max_in_flight: config.max_in_flight,
        },
        shutdown_signal(),
    );
    let result = if let Some(gateway) = &mut local_gateway {
        tokio::select! {
            result = serve => {
                gateway.stop().await?;
                result
            }
            status = gateway.wait() => {
                bail!("local Agent Gateway exited unexpectedly with {}", status?);
            }
            result = async {
                match &mut capture_relay {
                    Some(relay) => relay.await.context("capture relay task failed")?,
                    None => std::future::pending().await,
                }
            } => {
                gateway.stop().await?;
                result
            }
        }
    } else {
        serve.await
    };
    #[cfg(target_os = "linux")]
    if let Some(relay) = capture_relay {
        relay.abort();
    }
    if let Some(task) = renewal_task {
        task.abort();
    }
    result
}

async fn renew_device_certificate(context: RenewalContext) {
    loop {
        let delay = match renew_device_certificate_once(&context).await {
            Ok(true) => {
                tracing::info!(event = "device_certificate_renewed");
                CERTIFICATE_RENEWAL_CHECK_INTERVAL
            }
            Ok(false) => CERTIFICATE_RENEWAL_CHECK_INTERVAL,
            Err(_) => {
                tracing::warn!(event = "device_certificate_renewal_failed");
                CERTIFICATE_RENEWAL_RETRY_INTERVAL
            }
        };
        tokio::time::sleep(delay).await;
    }
}

async fn renew_device_certificate_once(context: &RenewalContext) -> anyhow::Result<bool> {
    let enrollment = load_enrollment_for(&context.issuer, &context.gateway_origin, &context.store)?;
    if !certificate_renewal_due(&enrollment, SystemTime::now(), CERTIFICATE_RENEW_BEFORE)? {
        return Ok(false);
    }
    let client = EnrollmentClient::new(&context.enrollment_url)?;
    if certificate_expired(&enrollment, SystemTime::now())? {
        client
            .recover_and_save(
                &context.identity,
                &context.issuer,
                &context.gateway_origin,
                &context.store,
            )
            .await?;
    } else {
        client
            .renew_and_save(
                &context.identity,
                &context.issuer,
                &context.gateway_origin,
                &context.store,
            )
            .await?;
    }
    let replacement =
        load_device_identity_for(&context.issuer, &context.gateway_origin, &context.store)?;
    context.proxy_identity.replace(replacement)?;
    Ok(true)
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
                agentdesktop::identity::oauth::open_authorization_url,
            )
            .await?
        }
    };
    let identity = ManagedIdentity::new(session, store.clone());
    let client = EnrollmentClient::new(&bootstrap.identity.enrollment_url)?;
    let mut enrollment =
        match load_enrollment_for(&bootstrap.identity.issuer, &gateway_origin, &store) {
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
    let mut announced = false;
    loop {
        enrollment = client.status(&identity, &enrollment).await?;
        save_enrollment_for(
            &bootstrap.identity.issuer,
            &gateway_origin,
            &store,
            &enrollment,
        )?;
        match enrollment.status {
            EnrollmentStatus::Approved => break,
            EnrollmentStatus::Pending | EnrollmentStatus::Issuing if Instant::now() < deadline => {
                if !announced {
                    println!("Waiting for your organization to approve this device...");
                    announced = true;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            EnrollmentStatus::Pending | EnrollmentStatus::Issuing => {
                anyhow::bail!(
                    "device approval did not complete; contact {}",
                    bootstrap.organization.support_url
                )
            }
            EnrollmentStatus::Rejected => anyhow::bail!(
                "this device enrollment was rejected; contact {}",
                bootstrap.organization.support_url
            ),
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
