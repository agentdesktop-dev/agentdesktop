use std::net::SocketAddr;
use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use tokio::net::TcpListener;
use tokio::sync::watch;

#[cfg(target_os = "linux")]
use crate::config::{Config, upstream_origin};
use crate::identity::enrollment::{
    EnrollmentClient, certificate_expired, certificate_renewal_due, load_client_identity_for,
    load_enrollment_for,
};
use crate::identity::oauth::{ManagedIdentity, load_session_for};
use crate::identity::storage::{CredentialStore, default_storage_root};
use crate::local_gateway::LocalGateway;

#[cfg(target_os = "linux")]
pub mod capture;
mod forwarder;
mod hbone;
mod status;

use hbone::{HboneClient, RotatingClientIdentity};

const LOCAL_GATEWAY_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const CERTIFICATE_RENEW_BEFORE: Duration = Duration::from_secs(6 * 60 * 60);
const CERTIFICATE_RENEWAL_CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);
const CERTIFICATE_RENEWAL_RETRY_INTERVAL: Duration = Duration::from_secs(60);

struct RenewalContext {
    enrollment_url: url::Url,
    gateway_origin: url::Url,
    identity: ManagedIdentity,
    issuer: url::Url,
    tunnel_identity: RotatingClientIdentity,
    store: CredentialStore,
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let _telemetry = crate::telemetry::init()?;
    let (identity, client_identity, renewal_context) = managed_identity(&config)?;
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
    let gateway_endpoint = gateway_endpoint(&config).await?;
    let hbone = match config.mode {
        crate::config::DeploymentMode::Managed => {
            HboneClient::connect_mtls(
                gateway_endpoint,
                config
                    .upstream
                    .host_str()
                    .context("managed Gateway upstream has no hostname")?
                    .to_owned(),
                client_identity.context("managed mode requires an enrolled client identity")?,
                Duration::from_millis(config.connect_timeout_ms),
            )
            .await?
        }
        crate::config::DeploymentMode::Standalone => {
            #[cfg(target_os = "linux")]
            {
                let token = local_gateway
                    .as_ref()
                    .context("standalone tunneling requires an owned local Agent Gateway")?
                    .capture_token();
                capture::local_hbone(
                    gateway_endpoint,
                    token,
                    Duration::from_millis(config.connect_timeout_ms),
                )
                .await?
            }
            #[cfg(not(target_os = "linux"))]
            {
                HboneClient::connect(gateway_endpoint).await?
            }
        }
    };

    let native_listener = TcpListener::bind(config.listen).await?;
    let status_listener = TcpListener::bind(config.status_listen).await?;
    tracing::info!(
        event = "connector_started",
        mode = config.mode.as_str(),
        listen = %native_listener.local_addr()?,
        status_listen = %status_listener.local_addr()?,
    );
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(signal_shutdown(shutdown_tx));

    let native_shutdown = shutdown_rx.clone();
    let native_task = tokio::spawn(forwarder::serve_native(
        native_listener,
        hbone.clone(),
        config.native_target.clone(),
        config.max_in_flight,
        Duration::from_millis(config.shutdown_timeout_ms),
        wait_for_shutdown(native_shutdown),
    ));
    let status_shutdown = shutdown_rx.clone();
    let status_task = tokio::spawn(status::serve(
        status_listener,
        gateway_endpoint,
        config.mode,
        identity.clone(),
        wait_for_shutdown(status_shutdown),
    ));

    #[cfg(target_os = "linux")]
    let capture_task = if config.capture_enabled {
        let listener = TcpListener::bind("127.0.0.1:15001").await?;
        tracing::info!(event = "capture_relay_ready", listen = %listener.local_addr()?);
        let capture_shutdown = shutdown_rx.clone();
        Some(tokio::spawn(forwarder::serve_capture(
            listener,
            hbone,
            config.max_in_flight,
            Duration::from_millis(config.shutdown_timeout_ms),
            wait_for_shutdown(capture_shutdown),
        )))
    } else {
        None
    };
    let renewal_task = renewal_context.map(|context| tokio::spawn(renew_certificate(context)));
    let result = if let Some(gateway) = &mut local_gateway {
        tokio::select! {
            result = join_service(native_task) => {
                gateway.stop().await?;
                result
            }
            result = join_service(status_task) => {
                gateway.stop().await?;
                result
            }
            status = gateway.wait() => {
                bail!("local Agent Gateway exited unexpectedly with {}", status?);
            }
            result = wait_capture(capture_task) => {
                gateway.stop().await?;
                result
            }
        }
    } else {
        tokio::select! {
            result = join_service(native_task) => result,
            result = join_service(status_task) => result,
        }
    };
    if let Some(task) = renewal_task {
        task.abort();
    }
    result
}

fn managed_identity(
    config: &Config,
) -> anyhow::Result<(
    Option<ManagedIdentity>,
    Option<RotatingClientIdentity>,
    Option<RenewalContext>,
)> {
    let Some(issuer) = &config.identity_issuer else {
        return Ok((None, None, None));
    };
    let identity_dir = config
        .identity_dir
        .clone()
        .map_or_else(default_storage_root, Ok)?;
    let store = CredentialStore::load(&identity_dir)?;
    let gateway_origin = upstream_origin(&config.upstream)?;
    let session = load_session_for(issuer, &gateway_origin, &store)?;
    let identity = ManagedIdentity::new(session, store.clone());
    let client_identity =
        RotatingClientIdentity::new(load_client_identity_for(issuer, &gateway_origin, &store)?);
    let enrollment_url = config
        .enrollment_url
        .clone()
        .context("managed identity requires an enrollment URL")?;
    Ok((
        Some(identity.clone()),
        Some(client_identity.clone()),
        Some(RenewalContext {
            enrollment_url,
            gateway_origin,
            identity,
            issuer: issuer.clone(),
            tunnel_identity: client_identity,
            store,
        }),
    ))
}

async fn renew_certificate(context: RenewalContext) {
    loop {
        let delay = match renew_certificate_once(&context).await {
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

async fn renew_certificate_once(context: &RenewalContext) -> anyhow::Result<bool> {
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
        load_client_identity_for(&context.issuer, &context.gateway_origin, &context.store)?;
    context.tunnel_identity.replace(replacement)?;
    Ok(true)
}

async fn signal_shutdown(shutdown: watch::Sender<bool>) {
    if tokio::signal::ctrl_c().await.is_err() {
        tracing::error!(event = "shutdown_signal_failed");
    }
    let _ = shutdown.send(true);
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stopping| *stopping).await;
}

async fn join_service(task: tokio::task::JoinHandle<anyhow::Result<()>>) -> anyhow::Result<()> {
    task.await.context("service task failed")?
}

#[cfg(target_os = "linux")]
async fn wait_capture(
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
) -> anyhow::Result<()> {
    match task {
        Some(task) => join_service(task).await,
        None => std::future::pending().await,
    }
}

async fn gateway_endpoint(config: &Config) -> anyhow::Result<SocketAddr> {
    let host = config
        .upstream
        .host_str()
        .context("Agent Gateway upstream has no host")?;
    let port = config
        .upstream
        .port_or_known_default()
        .context("Agent Gateway upstream has no port")?;
    tokio::net::lookup_host((host, port))
        .await?
        .next()
        .context("Agent Gateway upstream resolved to no addresses")
}
