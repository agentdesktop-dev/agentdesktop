use crate::{
    daemon_config::{self, DaemonConfigStore},
    database::Database,
    device_ca::DeviceCertificateIssuer,
    gateway_jwt::{self, GatewayJwtIssuer},
    management::{self, Settings, State},
    metrics::{self, Telemetry},
    oidc::OidcProvider,
    service::FleetAgentService,
};
use agentdesktop_core::config::ControllerConfig;
use agentdesktop_proto::fleet::fleet_agent_server::FleetAgentServer;
use agentdesktop_usage::UsageIngest;
use anyhow::Context;
use tonic::{
    service::Routes,
    transport::{Certificate, Identity, Server, ServerTlsConfig},
};

/// Runs fleet and management listeners with application-owned extensions.
/// Shared by every controller executable: enrollment/gateway setup stays here,
/// while the caller supplies the concrete admin authentication/policy integration.
/// Authenticated admin mode without a provider is rejected before network setup.
pub async fn run(
    config: ControllerConfig,
    database: Database,
    extensions: management::Extensions,
) -> anyhow::Result<()> {
    // Fail before any network setup when the access mode has no provider.
    management::auth(&config.admin_access, &extensions)?;
    let tls = config.tls.files();
    let daemon_config = match &config.daemon_config {
        Some(daemon) => Some(daemon_config::load(&daemon.path, daemon.revision)?),
        None => None,
    };
    let daemon_config = DaemonConfigStore::new(daemon_config);
    if let Some(daemon) = &config.daemon_config {
        daemon_config::watch(daemon.path.clone(), daemon.revision, daemon_config.clone())?;
    }
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
    let usage_token = config
        .usage
        .as_ref()
        .map(|usage| agentdesktop_usage::load_token(&usage.token_file))
        .transpose()
        .context("initialize usage collection")?;
    let usage_ingest = usage_token
        .as_deref()
        .map(|token| UsageIngest::service(database.usage(), token));
    if usage_ingest.is_some() {
        tracing::info!("usage collection enabled");
    }
    agentdesktop_usage::spawn_maintenance(database.usage());
    let admin_state = State::new(
        database.clone(),
        daemon_config.clone(),
        Settings {
            fleet_listen: config.fleet_listen.to_string(),
            admin_listen: config.admin_listen.to_string(),
            oidc_enabled: true,
            tls_enabled: true,
            gateway_jwt_enabled: gateway_jwt_issuer.is_some(),
            usage_enabled: usage_ingest.is_some(),
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
    // One meter provider: Prometheus on metricsListen, plus OTLP push when the
    // standard OTEL_EXPORTER_OTLP_* variables name an endpoint.
    let telemetry = std::sync::Arc::new(Telemetry::new().context("initialize controller metrics")?);
    // The fleet gauges cost a database read every 30 s; skip it when nothing exports them.
    if telemetry.exported(config.metrics_listen.is_some()) {
        telemetry.spawn_refresh(database.clone(), daemon_config.clone());
    }
    let fleet_metrics = telemetry.metrics.clone();
    let metrics_server = config
        .metrics_listen
        .map(|address| metrics::serve(address, telemetry.clone()));
    let service = FleetAgentService::new(
        Some(oidc),
        database,
        daemon_config,
        gateway_jwt_issuer,
        Some(device_certificate_issuer),
    )
    .with_usage_collection(usage_ingest.is_some())
    .with_metrics(fleet_metrics);

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
        let mut routes = Routes::from(gateway_jwt::routes(gateway_jwks))
            .add_service(FleetAgentServer::new(service));
        if let Some(usage_ingest) = usage_ingest {
            routes = routes.add_service(usage_ingest);
        }
        server
            .add_routes(routes)
            .serve(fleet_listen)
            .await
            .context("serve fleet gRPC API")
    };
    let admin = management::serve(
        admin_listen,
        admin_state,
        admin_gateway_jwks,
        config.admin_access,
        extensions,
    );
    let metrics = async move {
        match metrics_server {
            Some(server) => server.await,
            None => std::future::pending().await,
        }
    };
    let result = tokio::try_join!(fleet, admin, metrics);
    telemetry.shutdown();
    result?;
    Ok(())
}
