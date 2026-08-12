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
pub(crate) mod hbone;
mod renewal;
mod status;

use hbone::HboneClient;

const LOCAL_GATEWAY_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn run(config: Config) -> anyhow::Result<()> {
    let _telemetry = crate::telemetry::init()?;
    #[cfg(target_os = "linux")]
    let central_session_mode = config.session_socket.is_some();
    #[cfg(all(target_os = "windows", target_env = "msvc"))]
    let central_session_mode = config.session_pipe.is_some();
    #[cfg(not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))))]
    let central_session_mode = false;
    let managed_identity = if central_session_mode {
        None
    } else {
        renewal::load(&config)?
    };
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
    let hbone = if central_session_mode {
        None
    } else {
        Some(match config.mode {
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
                    hbone::TlsRoots::Native,
                )
                .await?
            }
            crate::config::DeploymentMode::Standalone => {
                #[cfg(target_os = "linux")]
                {
                    let capability = local_gateway
                        .as_ref()
                        .context("standalone tunneling requires an owned local Agent Gateway")?
                        .capability();
                    crate::local_gateway::connect(
                        gateway_endpoint,
                        capability,
                        Duration::from_millis(config.connect_timeout_ms),
                    )
                    .await?
                }
                #[cfg(not(target_os = "linux"))]
                {
                    HboneClient::connect(gateway_endpoint).await?
                }
            }
        })
    };

    #[cfg(target_os = "linux")]
    let session_state = if let Some(path) = &config.session_socket {
        let registry = crate::session::SessionRegistry::<u32>::new(
            config.mode,
            gateway_endpoint,
            config
                .upstream
                .host_str()
                .context("managed Gateway upstream has no hostname")?
                .to_owned(),
            Duration::from_millis(config.connect_timeout_ms),
            hbone::TlsRoots::Native,
        );
        Some((
            std::sync::Arc::new(crate::session::linux::SessionSocket::bind(path)?),
            registry,
        ))
    } else {
        None
    };

    #[cfg(all(target_os = "windows", target_env = "msvc"))]
    let session_state = if let Some(path) = &config.session_pipe {
        let registry = crate::session::SessionRegistry::<crate::session::windows::UserSid>::new(
            crate::config::DeploymentMode::Managed,
            gateway_endpoint,
            config
                .upstream
                .host_str()
                .context("managed Gateway upstream has no hostname")?
                .to_owned(),
            Duration::from_millis(config.connect_timeout_ms),
            hbone::TlsRoots::Native,
        );
        Some((path.clone(), registry))
    } else {
        None
    };

    #[cfg(all(target_os = "windows", target_env = "msvc"))]
    let native_bind = config.wfp_proxy_listen.unwrap_or(config.listen);
    #[cfg(not(all(target_os = "windows", target_env = "msvc")))]
    let native_bind = config.listen;
    let native_listener = TcpListener::bind(native_bind).await?;
    #[cfg(all(target_os = "windows", target_env = "msvc"))]
    if config.wfp_proxy_listen.is_some() {
        crate::platform::windows::configure_native_redirect(
            config.listen,
            native_listener.local_addr()?,
        )?;
    }
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
    #[cfg(target_os = "linux")]
    let native_task = if let Some((_, registry)) = &session_state {
        tokio::spawn(forwarder::serve_native_sessions(
            native_listener,
            registry.clone(),
            config.native_target.clone(),
            config.max_in_flight,
            Duration::from_millis(config.shutdown_timeout_ms),
            wait_for_shutdown(native_shutdown),
        ))
    } else {
        tokio::spawn(forwarder::serve_native(
            native_listener,
            hbone
                .clone()
                .context("forwarding identity is unavailable")?,
            config.native_target.clone(),
            config.max_in_flight,
            Duration::from_millis(config.shutdown_timeout_ms),
            wait_for_shutdown(native_shutdown),
        ))
    };
    #[cfg(all(target_os = "windows", target_env = "msvc"))]
    let native_task =
        if let (Some((_, registry)), Some(_)) = (&session_state, config.wfp_proxy_listen) {
            tokio::spawn(forwarder::serve_native_sessions(
                native_listener,
                registry.clone(),
                config.listen,
                config.native_target.clone(),
                config.max_in_flight,
                Duration::from_millis(config.shutdown_timeout_ms),
                wait_for_shutdown(native_shutdown),
            ))
        } else if session_state.is_some() {
            tokio::spawn(forwarder::serve_native_without_attribution(
                native_listener,
                config.max_in_flight,
                wait_for_shutdown(native_shutdown),
            ))
        } else {
            tokio::spawn(forwarder::serve_native(
                native_listener,
                hbone
                    .clone()
                    .context("forwarding identity is unavailable")?,
                config.native_target.clone(),
                config.max_in_flight,
                Duration::from_millis(config.shutdown_timeout_ms),
                wait_for_shutdown(native_shutdown),
            ))
        };
    #[cfg(not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))))]
    let native_task = tokio::spawn(forwarder::serve_native(
        native_listener,
        hbone
            .clone()
            .context("forwarding identity is unavailable")?,
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
        if let Some((_, registry)) = &session_state {
            Some(tokio::spawn(forwarder::serve_capture_sessions(
                listener,
                registry.clone(),
                config.max_in_flight,
                Duration::from_millis(config.shutdown_timeout_ms),
                wait_for_shutdown(capture_shutdown),
            )))
        } else {
            Some(tokio::spawn(forwarder::serve_capture(
                listener,
                hbone.context("capture forwarding identity is unavailable")?,
                config.max_in_flight,
                Duration::from_millis(config.shutdown_timeout_ms),
                wait_for_shutdown(capture_shutdown),
            )))
        }
    } else {
        None
    };
    #[cfg(not(target_os = "linux"))]
    let capture_task = None;
    #[cfg(target_os = "linux")]
    let session_task = session_state.as_ref().map(|(socket, registry)| {
        tokio::spawn(crate::session::linux::serve_registrations(
            std::sync::Arc::clone(socket),
            registry.clone(),
            Duration::from_millis(config.connect_timeout_ms),
            shutdown_rx.clone(),
        ))
    });
    #[cfg(all(target_os = "windows", target_env = "msvc"))]
    let session_task = session_state.as_ref().map(|(path, registry)| {
        tokio::spawn(crate::session::windows::serve_registrations(
            path.clone(),
            registry.clone(),
            Duration::from_millis(config.connect_timeout_ms),
            shutdown_rx.clone(),
        ))
    });
    #[cfg(not(any(target_os = "linux", all(target_os = "windows", target_env = "msvc"))))]
    let session_task = None;
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
            result = wait_optional_service(capture_task) => {
                gateway.stop().await?;
                result
            }
            result = wait_optional_service(session_task) => {
                gateway.stop().await?;
                result
            }
        }
    } else {
        tokio::select! {
            result = join_service(native_task) => result,
            result = join_service(status_task) => result,
            result = wait_optional_service(capture_task) => result,
            result = wait_optional_service(session_task) => result,
        }
    };
    if let Some(task) = renewal_task {
        task.abort();
    }
    result
}

#[cfg(target_os = "linux")]
pub async fn run_session_agent(config: Config) -> anyhow::Result<()> {
    let socket = config
        .session_socket
        .as_deref()
        .context("session agent requires --session-socket")?;
    match config.mode {
        crate::config::DeploymentMode::Managed => {
            let context =
                renewal::load(&config)?.context("session agent requires managed identity")?;
            let identity = context.client_identity.clone();
            let renewal = renewal::spawn(context);
            let result =
                crate::session::linux::run_user_agent(socket, identity, Duration::from_secs(1))
                    .await;
            renewal.abort();
            result
        }
        crate::config::DeploymentMode::Standalone => {
            let binary = config
                .gateway_binary
                .as_deref()
                .context("standalone session agent requires --gateway-binary")?;
            let gateway_config = config
                .gateway_config
                .as_deref()
                .context("standalone session agent requires --gateway-config")?;
            let mut gateway = LocalGateway::spawn(binary, gateway_config)?;
            gateway
                .wait_until_reachable(&config.upstream, LOCAL_GATEWAY_STARTUP_TIMEOUT)
                .await?;
            let endpoint = gateway_endpoint(&config).await?;
            let token = gateway.capability().environment_value().to_owned();
            tokio::select! {
                result = crate::session::linux::run_local_user_agent(
                    socket,
                    endpoint,
                    token,
                    Duration::from_secs(1),
                ) => {
                    gateway.stop().await?;
                    result
                }
                status = gateway.wait() => {
                    bail!("local Agent Gateway exited unexpectedly with {}", status?);
                }
            }
        }
    }
}

#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub async fn run_session_agent(config: Config) -> anyhow::Result<()> {
    if config.mode != crate::config::DeploymentMode::Managed {
        bail!("Windows session agent is only available in managed mode");
    }
    let pipe = config
        .session_pipe
        .as_deref()
        .context("session agent requires --session-pipe")?;
    let context = renewal::load(&config)?.context("session agent requires managed identity")?;
    let identity = context.client_identity.clone();
    let renewal = renewal::spawn(context);
    let result =
        crate::session::windows::run_user_agent(pipe, identity, Duration::from_secs(1)).await;
    renewal.abort();
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

async fn wait_optional_service(
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
