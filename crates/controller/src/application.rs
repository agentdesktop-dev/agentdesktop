use std::sync::Arc;

use crate::{
    daemon_config::{self, DaemonConfigStore},
    database::Database,
    device_ca::DeviceCertificateIssuer,
    enrollment_token::EnrollmentTokens,
    fleet_limits::FleetLimits,
    gateway_jwt::{self, GatewayJwtIssuer},
    http_listener::{HttpListener, HttpListenerLimits, HttpListenerTls},
    management::{self, Settings, State},
    metrics::{LimitMetrics, Telemetry},
    oidc::OidcProvider,
    service::FleetAgentService,
};
use agentdesktop_core::config::ControllerConfig;
use agentdesktop_usage::UsageIngest;
use anyhow::Context;
use tonic::{
    service::Routes,
    transport::{Certificate, Identity, ServerTlsConfig},
};

/// Runs the fleet, management and gateway listeners with application-owned extensions.
/// Shared by every controller executable: enrollment/gateway setup stays here,
/// while the caller supplies the concrete admin authentication/policy integration.
/// Authenticated admin mode without a provider is rejected before network setup.
pub async fn run(
    config: ControllerConfig,
    database: Database,
    extensions: management::Extensions,
) -> anyhow::Result<()> {
    // Before any TLS configuration is built: the fleet listener uses the
    // process-wide rustls provider.
    crate::install_rustls_provider();
    // Fail before any network setup when the access mode has no provider.
    management::auth(&config.admin_listener.access, &extensions)?;
    // Out-of-range fleet limits fail here too, before any network setup.
    let limits = FleetLimits::from_config(&config.fleet_listener.limits)?;
    // Metrics are recorded only when something serves them.
    let telemetry = config
        .metrics_listener
        .as_ref()
        .map(|_| Telemetry::new().map(Arc::new))
        .transpose()?;
    let metrics = telemetry
        .as_ref()
        .map_or_else(LimitMetrics::disabled, |telemetry| telemetry.limits());
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
    let enrollment_tokens = config
        .enrollment_tokens
        .as_ref()
        .map(EnrollmentTokens::load)
        .transpose()
        .context("initialize enrollment tokens")?;
    if enrollment_tokens.is_some() {
        tracing::info!("token enrollment enabled");
    }
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
            fleet_listen: config.fleet_listener.listen.to_string(),
            admin_listen: config.admin_listener.listen.to_string(),
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
    let service = FleetAgentService::new(
        Some(oidc),
        database,
        daemon_config,
        gateway_jwt_issuer,
        Some(device_certificate_issuer),
        &limits,
        &metrics,
    )
    .with_enrollment_tokens(enrollment_tokens)
    .with_usage_collection(usage_ingest.is_some());

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
    let fleet_listen = config.fleet_listener.listen;
    let fleet_listener = limits.listener(fleet_listen, Some(tls_config), &metrics)?;
    // Started only with gatewayJwt: the gateway listener serves its JWKS.
    let gateway_listener = gateway_jwks
        .map(|jwks| {
            let listener = &config.gateway_listener;
            let tls = listener.tls.then(|| HttpListenerTls {
                certificate: &tls.certificate,
                key: &tls.key,
            });
            let limits = HttpListenerLimits::new(
                listener.limits.max_connections,
                listener.limits.connections_per_address,
            );
            anyhow::Ok((
                HttpListener::bind("gateway", listener.listen, tls, limits, &metrics)?,
                jwks,
            ))
        })
        .transpose()?;
    let metrics_listener = config
        .metrics_listener
        .as_ref()
        .zip(telemetry)
        .map(|(listener, telemetry)| {
            let limits = HttpListenerLimits::new(
                listener.limits.max_connections,
                listener.limits.connections_per_address,
            );
            anyhow::Ok((
                HttpListener::bind("metrics", listener.listen, None, limits, &metrics)?,
                telemetry,
            ))
        })
        .transpose()?;

    tracing::info!(listen = %fleet_listen, "fleet controller listening");
    let fleet = async move {
        let mut routes = Routes::default();
        if let Some(usage_ingest) = usage_ingest {
            routes = routes.add_service(usage_ingest);
        }
        fleet_listener
            .serve(service, routes)
            .await
            .context("serve fleet gRPC API")
    };
    let admin = management::serve(admin_state, config.admin_listener, extensions, &metrics);
    let gateway = async move {
        match gateway_listener {
            Some((listener, jwks)) => listener.serve(gateway_jwt::routes(jwks)).await,
            None => std::future::pending().await,
        }
    };
    let metrics_listener = async move {
        match metrics_listener {
            Some((listener, telemetry)) => listener.serve(telemetry.router()).await,
            None => std::future::pending().await,
        }
    };
    tokio::try_join!(fleet, admin, gateway, metrics_listener)?;
    Ok(())
}
