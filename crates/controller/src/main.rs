use std::path::PathBuf;

use agentdesktop_controller::{
    admin::{self, AdminState, ControllerSettings},
    daemon_config::FleetConfiguration,
    database::Database,
    device_ca::DeviceCertificateIssuer,
    gateway_jwt::{self, GatewayJwtIssuer},
    oidc::OidcProvider,
    service::FleetAgentService,
};
use agentdesktop_core::{DEFAULT_CONTROLLER_CONFIG_PATH, config, telemetry};
use agentdesktop_proto::fleet::fleet_agent_server::FleetAgentServer;
use anyhow::Context;
use clap::Parser;
use tonic::{
    service::Routes,
    transport::{Certificate, Identity, Server, ServerTlsConfig},
};

#[derive(Parser)]
#[command(about = "Agentdesktop fleet controller")]
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
    let tls = config.tls.files();
    let database = Database::connect(&config.database_url).await?;
    let fleet_configuration =
        FleetConfiguration::open(config.daemon_config.as_ref(), &database).await?;
    let oidc = &config.oidc;
    if oidc.issuer.starts_with("http://") {
        tracing::warn!(issuer = %oidc.issuer, "allowing insecure OIDC issuer for development");
    }
    let oidc = OidcProvider::discover(
        oidc.issuer.clone(),
        oidc.client_id.clone(),
        oidc.redirect_uri.clone(),
    )
    .await
    .context("initialize OIDC enrollment")?;
    tracing::info!("OIDC enrollment enabled");
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
        .context("initialize LLM gateway JWT issuer")?;
    if gateway_jwt_issuer.is_some() {
        tracing::info!("LLM gateway JWT issuance enabled");
    }
    let gateway_jwks = gateway_jwt_issuer.as_ref().map(GatewayJwtIssuer::jwks);
    let admin_gateway_jwks = gateway_jwks.clone();
    let admin_state = AdminState::new(
        database.clone(),
        fleet_configuration.clone(),
        ControllerSettings {
            fleet_listen: config.fleet_listen.to_string(),
            admin_listen: config.admin_listen.to_string(),
            oidc_enabled: true,
            tls_enabled: true,
            gateway_jwt_enabled: gateway_jwt_issuer.is_some(),
        },
    );
    let ca_certificate =
        std::fs::read_to_string(&tls.client_ca_certificate).with_context(|| {
            format!(
                "read device CA certificate from {}",
                tls.client_ca_certificate.display()
            )
        })?;
    let ca_key = std::fs::read_to_string(&tls.client_ca_key)
        .with_context(|| format!("read device CA key from {}", tls.client_ca_key.display()))?;
    let device_certificate_issuer = DeviceCertificateIssuer::from_pem(ca_certificate, &ca_key)
        .context("initialize device certificate issuer")?;
    tracing::info!("device certificate issuance enabled");
    let service = FleetAgentService::new(
        Some(oidc),
        database,
        fleet_configuration.store().clone(),
        gateway_jwt_issuer,
        Some(device_certificate_issuer),
    );

    let certificate = std::fs::read(&tls.certificate)
        .with_context(|| format!("read TLS certificate from {}", tls.certificate.display()))?;
    let key = std::fs::read(&tls.key)
        .with_context(|| format!("read TLS key from {}", tls.key.display()))?;
    let client_ca = std::fs::read(&tls.client_ca_certificate).with_context(|| {
        format!(
            "read fleet client CA certificate from {}",
            tls.client_ca_certificate.display()
        )
    })?;
    let tls_config = ServerTlsConfig::new()
        .identity(Identity::from_pem(certificate, key))
        .client_ca_root(Certificate::from_pem(client_ca))
        .client_auth_optional(true);
    let mut server = Server::builder()
        .accept_http1(true)
        .tls_config(tls_config)?;

    let fleet_listen = config.fleet_listen;
    let admin_listen = config.admin_listen;
    tracing::info!(listen = %fleet_listen, "fleet controller listening");
    let fleet = async move {
        let routes = Routes::from(gateway_jwt::routes(gateway_jwks))
            .add_service(FleetAgentServer::new(service));
        server
            .add_routes(routes)
            .serve(fleet_listen)
            .await
            .context("serve fleet gRPC API")
    };
    let admin = admin::serve(admin_listen, admin_state, admin_gateway_jwks);
    tokio::try_join!(fleet, admin)?;
    Ok(())
}
