use agentgateway_edge_connector::config::{Config, upstream_origin};
use agentgateway_edge_connector::identity::oauth::{ManagedIdentity, load_session_for};
use agentgateway_edge_connector::identity::storage::{CredentialStore, default_storage_root};
use agentgateway_edge_connector::local_gateway::LocalGateway;
use agentgateway_edge_connector::proxy::{self, ProxyOptions};
use anyhow::bail;
use tokio::net::TcpListener;

const LOCAL_GATEWAY_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _telemetry = agentgateway_edge_connector::telemetry::init()?;
    let config = Config::parse_and_validate()?;
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

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        let _ = error;
        tracing::error!(event = "shutdown_signal_failed");
    }
}
