use std::time::Duration;
use std::{net::SocketAddr, path::PathBuf};

use agentdesktop_controller::{
    admin::{self, AdminState, ControllerSettings},
    database::Database,
    gateway_jwt::GatewayJwtIssuer,
    oidc::OidcProvider,
    service::{FleetAgentService, load_desired_config},
};
use agentdesktop_core::telemetry;
use agentdesktop_proto::fleet::fleet_agent_server::FleetAgentServer;
use anyhow::Context;
use clap::Parser;
use tonic::transport::{Identity, Server, ServerTlsConfig};

#[derive(Parser)]
#[command(about = "AgentDesktop fleet controller")]
struct Args {
    /// Address on which the device-facing gRPC fleet API listens.
    #[arg(long, default_value = "127.0.0.1:8443")]
    listen: SocketAddr,

    /// Local address for the controller management UI.
    #[arg(long, default_value = "127.0.0.1:8080")]
    admin_listen: SocketAddr,

    /// Shared bootstrap token accepted for device enrollment.
    #[arg(long)]
    enrollment_token: Option<String>,

    /// OpenID Connect issuer URL used for interactive device enrollment.
    #[arg(long, requires = "oidc_client_id")]
    oidc_issuer: Option<String>,

    /// OpenID Connect client identifier used for interactive device enrollment.
    #[arg(long, requires = "oidc_issuer")]
    oidc_client_id: Option<String>,

    /// Redirect URI registered with the OpenID Connect provider.
    #[arg(long, default_value = "http://127.0.0.1:5555/callback")]
    oidc_redirect_uri: String,

    /// SQLite or PostgreSQL URL used for controller state.
    #[arg(long, default_value = "sqlite://agentdesktop-controller.db?mode=rwc")]
    database_url: String,

    /// Path to the YAML configuration distributed to enrolled devices.
    #[arg(long)]
    desired_config: Option<PathBuf>,

    /// Monotonically increasing revision assigned to the desired configuration.
    #[arg(long, default_value_t = 1)]
    desired_config_revision: u64,

    /// Path to an RSA private key used to issue inference-gateway JWTs.
    #[arg(long)]
    gateway_jwt_private_key: Option<PathBuf>,

    /// Issuer claim placed in inference-gateway JWTs.
    #[arg(long, default_value = "agentdesktop-controller")]
    gateway_jwt_issuer: String,

    /// Key identifier placed in inference-gateway JWT headers.
    #[arg(long, default_value = "agentdesktop")]
    gateway_jwt_key_id: String,

    /// Lifetime, in seconds, of issued inference-gateway JWTs.
    #[arg(long, default_value_t = 300)]
    gateway_jwt_lifetime_seconds: u64,

    /// Path to the PEM-encoded TLS certificate for the fleet API.
    #[arg(long, requires = "tls_key")]
    tls_certificate: Option<PathBuf>,

    /// Path to the PEM-encoded TLS private key for the fleet API.
    #[arg(long, requires = "tls_certificate")]
    tls_key: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if !args.admin_listen.ip().is_loopback() {
        anyhow::bail!("--admin-listen must use a loopback address");
    }
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
    let admin_state = AdminState::new(
        database.clone(),
        desired_config.clone(),
        ControllerSettings {
            fleet_listen: args.listen.to_string(),
            admin_listen: args.admin_listen.to_string(),
            enrollment_token_enabled: args.enrollment_token.is_some(),
            oidc_enabled: oidc.is_some(),
            tls_enabled: args.tls_certificate.is_some(),
            gateway_jwt_enabled: gateway_jwt_issuer.is_some(),
        },
    );
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

    let fleet_listen = args.listen;
    let admin_listen = args.admin_listen;
    tracing::info!(listen = %fleet_listen, "fleet controller listening");
    let fleet = async move {
        server
            .add_service(FleetAgentServer::new(service))
            .serve(fleet_listen)
            .await
            .context("serve fleet gRPC API")
    };
    let admin = admin::serve(admin_listen, admin_state);
    tokio::try_join!(fleet, admin)?;
    Ok(())
}
