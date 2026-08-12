use std::path::PathBuf;

use agentdesktop_controller::{
    admin::{self, AdminState, ControllerSettings},
    database::Database,
    gateway_jwt::GatewayJwtIssuer,
    oidc::OidcProvider,
    service::{FleetAgentService, load_desired_config},
};
use agentdesktop_core::{DEFAULT_CONTROLLER_CONFIG_PATH, config, telemetry};
use agentdesktop_proto::fleet::fleet_agent_server::FleetAgentServer;
use anyhow::Context;
use clap::Parser;
use tonic::transport::{Identity, Server, ServerTlsConfig};

#[derive(Parser)]
#[command(about = "AgentDesktop fleet controller")]
struct Args {
    /// Path to the controller YAML configuration file.
    #[arg(long, default_value = DEFAULT_CONTROLLER_CONFIG_PATH)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let _log_flush = telemetry::setup_logging("info", false);
    let config = config::load_controller(&args.config)?;
    if !config.fleet_listen.ip().is_loopback() && config.tls.is_none() {
        tracing::warn!(
            listen = %config.fleet_listen,
            "allowing insecure remote fleet listener for development"
        );
    }
    let desired_config = match &config.desired_config {
        Some(desired) => load_desired_config(Some(&desired.path), desired.revision)?,
        None => None,
    };
    let database = Database::connect(&config.database_url).await?;
    let oidc = match &config.oidc {
        Some(oidc) => {
            if oidc.issuer.starts_with("http://") {
                tracing::warn!(issuer = %oidc.issuer, "allowing insecure OIDC issuer for development");
            }
            let provider = OidcProvider::discover(
                oidc.issuer.clone(),
                oidc.client_id.clone(),
                oidc.redirect_uri.clone(),
            )
            .await
            .context("initialize OIDC enrollment")?;
            tracing::info!("OIDC enrollment enabled");
            Some(provider)
        }
        None => None,
    };
    let gateway_jwt_issuer = config
        .gateway_jwt
        .as_ref()
        .map(|gateway| {
            GatewayJwtIssuer::from_rsa_pem(
                &gateway.private_key,
                gateway.issuer.clone(),
                gateway.key_id.clone(),
                gateway.lifetime,
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
            fleet_listen: config.fleet_listen.to_string(),
            admin_listen: config.admin_listen.to_string(),
            oidc_enabled: oidc.is_some(),
            tls_enabled: config.tls.is_some(),
            gateway_jwt_enabled: gateway_jwt_issuer.is_some(),
        },
    );
    let service = FleetAgentService::new(oidc, database, desired_config, gateway_jwt_issuer);

    let mut server = Server::builder();
    if let Some(tls) = &config.tls {
        let certificate = std::fs::read(&tls.certificate)
            .with_context(|| format!("read TLS certificate from {}", tls.certificate.display()))?;
        let key = std::fs::read(&tls.key)
            .with_context(|| format!("read TLS key from {}", tls.key.display()))?;
        server = server
            .tls_config(ServerTlsConfig::new().identity(Identity::from_pem(certificate, key)))?;
    }

    let fleet_listen = config.fleet_listen;
    let admin_listen = config.admin_listen;
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
