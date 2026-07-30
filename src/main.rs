use agentgateway_edge_connector::config::Config;
use agentgateway_edge_connector::local_gateway::LocalGateway;
use agentgateway_edge_connector::proxy;
use anyhow::bail;
use tokio::net::TcpListener;

const LOCAL_GATEWAY_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse_and_validate()?;
    let mut local_gateway = match (&config.gateway_binary, &config.gateway_config) {
        (Some(binary), Some(gateway_config)) => {
            println!("starting local Agent Gateway from {}", binary.display());
            Some(LocalGateway::spawn(binary, gateway_config)?)
        }
        _ => None,
    };
    if let Some(gateway) = &mut local_gateway {
        gateway
            .wait_until_reachable(&config.upstream, LOCAL_GATEWAY_STARTUP_TIMEOUT)
            .await?;
        println!("local Agent Gateway is reachable at {}", config.upstream);
    }
    let listener = TcpListener::bind(config.listen).await?;
    println!(
        "running in {:?} mode, listening on {} and forwarding to {}",
        config.mode,
        listener.local_addr()?,
        config.upstream
    );
    let serve = proxy::serve(listener, config.upstream, config.mode, shutdown_signal());
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
        eprintln!("failed to install shutdown signal handler: {error}");
    }
}
