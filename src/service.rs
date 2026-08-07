use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::config::Config;
use crate::local_gateway::LocalGateway;

#[cfg(target_os = "linux")]
pub mod capture;
mod forwarder;
mod hbone;
mod renewal;
mod status;

use hbone::HboneClient;

const LOCAL_GATEWAY_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn run(config: Config) -> anyhow::Result<()> {
    let _telemetry = crate::telemetry::init()?;
    let managed_identity = renewal::load(&config)?;
    let identity = managed_identity
        .as_ref()
        .map(|context| context.identity.clone());
    let client_identity = managed_identity
        .as_ref()
        .map(|context| context.client_identity.clone());
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
    #[cfg(not(target_os = "linux"))]
    let capture_task = None;
    let renewal_task = managed_identity.map(renewal::spawn);
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
