use std::time::Duration;
use std::{net::SocketAddr, path::PathBuf};

use agentplane_controller::{
    database::Database,
    gateway_jwt::GatewayJwtIssuer,
    oidc::OidcProvider,
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

    #[arg(long, requires = "oidc_client_id")]
    oidc_issuer: Option<String>,

    #[arg(long, requires = "oidc_issuer")]
    oidc_client_id: Option<String>,

    #[arg(long, default_value = "http://127.0.0.1:5555/callback")]
    oidc_redirect_uri: String,

    #[arg(long, default_value = "sqlite://agentplane-controller.db?mode=rwc")]
    database_url: String,

    #[arg(long)]
    desired_config: Option<PathBuf>,

    #[arg(long, default_value_t = 1)]
    desired_config_revision: u64,

    #[arg(long)]
    gateway_jwt_private_key: Option<PathBuf>,

    #[arg(long, default_value = "agentplane-controller")]
    gateway_jwt_issuer: String,

    #[arg(long, default_value = "agentplane")]
    gateway_jwt_key_id: String,

    #[arg(long, default_value_t = 300)]
    gateway_jwt_lifetime_seconds: u64,

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
    let oidc = match (args.oidc_issuer, args.oidc_client_id) {
        (Some(issuer), Some(client_id)) => {
            if issuer.starts_with("http://") {
                tracing::warn!(%issuer, "OIDC issuer does not use TLS");
            }
            let provider = OidcProvider::discover(issuer, client_id, args.oidc_redirect_uri)
                .await
                .context("initialize OIDC enrollment")?;
            tracing::info!("OIDC enrollment enabled");
            Some(provider)
        }
        (None, None) => None,
        _ => unreachable!("clap enforces paired OIDC arguments"),
    };
    let gateway_jwt_issuer = args
        .gateway_jwt_private_key
        .as_deref()
        .map(|path| {
            GatewayJwtIssuer::from_rsa_pem(
                path,
                args.gateway_jwt_issuer,
                args.gateway_jwt_key_id,
                Duration::from_secs(args.gateway_jwt_lifetime_seconds),
            )
        })
        .transpose()
        .context("initialize inference gateway JWT issuer")?;
    if gateway_jwt_issuer.is_some() {
        tracing::info!("inference gateway JWT issuance enabled");
    }
    let service = FleetAgentService::new(
        args.enrollment_token,
        oidc,
        database,
        desired_config,
        gateway_jwt_issuer,
    );

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
