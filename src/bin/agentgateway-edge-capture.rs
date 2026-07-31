#[cfg(not(target_os = "linux"))]
fn main() -> anyhow::Result<()> {
    anyhow::bail!("transparent capture relay is only available on Linux")
}

#[cfg(target_os = "linux")]
mod linux {
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use agentgateway_edge_connector::capture;
    use agentgateway_edge_connector::capture::{TUNNEL_TOKEN_HEADER, load_tunnel_token};
    use agentgateway_edge_connector::hbone::HboneClient;
    use anyhow::{Result, bail};
    use clap::Parser;
    use http::HeaderMap;
    use tokio::net::TcpListener;

    #[derive(Debug, Parser)]
    #[command(version, about = "Relay redirected Linux TCP flows over HBONE")]
    struct Cli {
        #[arg(long, default_value = "127.0.0.1:15001")]
        listen: SocketAddr,
        #[arg(long, default_value = "127.0.0.1:15008")]
        hbone_endpoint: SocketAddr,
        #[arg(long, env = "AGENTGATEWAY_EDGE_CAPTURE_TOKEN_FILE")]
        token_file: PathBuf,
        #[arg(long, default_value_t = 128)]
        max_tunnels: usize,
    }

    pub async fn run() -> Result<()> {
        let cli = Cli::parse();
        if !cli.listen.ip().is_loopback() || !cli.hbone_endpoint.ip().is_loopback() {
            bail!("prototype capture and HBONE endpoints must be loopback");
        }
        if cli.max_tunnels == 0 {
            bail!("max tunnels must be greater than zero");
        }
        let mut connect_headers = HeaderMap::new();
        connect_headers.insert(TUNNEL_TOKEN_HEADER, load_tunnel_token(&cli.token_file)?);
        let hbone = HboneClient::connect_with_headers(cli.hbone_endpoint, connect_headers).await?;
        let listener = TcpListener::bind(cli.listen).await?;
        tracing::info!(event = "capture_started", listen = %listener.local_addr()?);
        capture::serve(listener, hbone, cli.max_tunnels, shutdown_signal()).await
    }

    async fn shutdown_signal() {
        if tokio::signal::ctrl_c().await.is_err() {
            tracing::error!(event = "shutdown_signal_failed");
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _telemetry = agentgateway_edge_connector::telemetry::init()?;
    linux::run().await
}
