use agentgateway_edge_connector::config::Config;
use agentgateway_edge_connector::proxy;
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = Config::parse_and_validate()?;
    let listener = TcpListener::bind(config.listen).await?;
    println!(
        "running in {:?} mode, listening on {} and forwarding to {}",
        config.mode,
        listener.local_addr()?,
        config.upstream
    );
    proxy::serve(listener, config.upstream, shutdown_signal()).await
}

async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("failed to install shutdown signal handler: {error}");
    }
}
