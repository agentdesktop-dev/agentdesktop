use std::{net::SocketAddr, path::PathBuf};

use agentplane_controller::{
    database::Database,
    service::{FleetAgentService, load_desired_config},
};
use agentplane_core::telemetry;
use agentplane_proto::fleet::fleet_agent_server::FleetAgentServer;
use anyhow::Context;
use clap::Parser;
use tonic::transport::{Identity, Server, ServerTlsConfig};

#[derive(Parser)]
#[command(about = "Agentplane fleet controller")]
struct Args {
    #[arg(long, default_value = "127.0.0.1:8443")]
    listen: SocketAddr,

    #[arg(long)]
    enrollment_token: Option<String>,

    #[arg(long, default_value = "sqlite://agentplane-controller.db?mode=rwc")]
    database_url: String,

    #[arg(long)]
    desired_config: Option<PathBuf>,

    #[arg(long, default_value_t = 1)]
    desired_config_revision: u64,

    #[arg(long, requires = "tls_key")]
    tls_certificate: Option<PathBuf>,

    #[arg(long, requires = "tls_certificate")]
    tls_key: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let _log_flush = telemetry::setup_logging("info", false);
    let desired_config =
        load_desired_config(args.desired_config.as_deref(), args.desired_config_revision)?;
    let database = Database::connect(&args.database_url).await?;
    let service = FleetAgentService::new(args.enrollment_token, database, desired_config);

    let mut server = Server::builder();
    if let (Some(certificate), Some(key)) = (args.tls_certificate, args.tls_key) {
        let certificate = std::fs::read(&certificate)
            .with_context(|| format!("read TLS certificate from {}", certificate.display()))?;
        let key =
            std::fs::read(&key).with_context(|| format!("read TLS key from {}", key.display()))?;
        server = server
            .tls_config(ServerTlsConfig::new().identity(Identity::from_pem(certificate, key)))?;
    }

    tracing::info!(listen = %args.listen, "fleet controller listening");
    server
        .add_service(FleetAgentServer::new(service))
        .serve(args.listen)
        .await
        .context("serve fleet gRPC API")
}
