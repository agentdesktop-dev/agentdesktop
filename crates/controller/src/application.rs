use std::{path::Path, time::Duration};

use crate::{
    daemon_config::{self, DaemonConfigStore},
    database::{DaemonConfigAuthor, Database},
    device_ca::DeviceCertificateIssuer,
    enrollment_token::EnrollmentTokens,
    fleet_limits::FleetLimits,
    gateway_jwt::{self, GatewayJwtIssuer},
    http_listener::{HttpListener, HttpListenerLimits, HttpListenerTls},
    management::{self, DaemonConfigSource, Settings, State},
    oidc::OidcProvider,
    service::FleetAgentService,
};
use agentdesktop_core::config::ControllerConfig;
use agentdesktop_usage::UsageIngest;
use anyhow::{Context, bail};
use tonic::{
    service::Routes,
    transport::{Certificate, Identity, ServerTlsConfig},
};
use tracing::{info, warn};

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
    let tls = config.tls.files();
    let (active_daemon_config, daemon_config_source) = match &config.daemon_config {
        Some(daemon) if let Some(path) = &daemon.path => (
            Some(daemon_config::load(path, daemon.file_revision())?),
            DaemonConfigSource::File,
        ),
        Some(daemon) if let Some(database_config) = &daemon.database => (
            load_database_daemon_config(&database, database_config.seed_path.as_deref()).await?,
            DaemonConfigSource::Database,
        ),
        Some(_) => bail!("validated daemonConfig has no source"),
        None => (None, DaemonConfigSource::None),
    };
    let daemon_config = DaemonConfigStore::new(active_daemon_config);
    if let Some(daemon) = &config.daemon_config {
        if let Some(path) = &daemon.path {
            daemon_config::watch(path.clone(), daemon.file_revision(), daemon_config.clone())?;
        } else if daemon.database.is_some() {
            spawn_database_daemon_config_poller(database.clone(), daemon_config.clone());
        }
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
        daemon_config_source,
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
    let fleet_listener = limits.listener(fleet_listen, Some(tls_config))?;
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
                HttpListener::bind("gateway", listener.listen, tls, limits)?,
                jwks,
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
    let admin = management::serve(admin_state, config.admin_listener, extensions);
    let gateway = async move {
        match gateway_listener {
            Some((listener, jwks)) => listener.serve(gateway_jwt::routes(jwks)).await,
            None => std::future::pending().await,
        }
    };
    tokio::try_join!(fleet, admin, gateway)?;
    Ok(())
}

async fn load_database_daemon_config(
    database: &Database,
    seed_path: Option<&Path>,
) -> anyhow::Result<Option<agentdesktop_proto::fleet::DaemonConfig>> {
    if database.latest_daemon_config_revision().await?.is_none()
        && let Some(path) = seed_path
    {
        let seed = daemon_config::load(path, 1)?;
        let yaml =
            String::from_utf8(seed.yaml).context("daemon configuration seed is not UTF-8")?;
        let author = DaemonConfigAuthor {
            kind: "seed".to_owned(),
            issuer: String::new(),
            subject: "seed".to_owned(),
            client_id: None,
            display: Some("Initial seed".to_owned()),
        };
        let _ = database
            .insert_daemon_config_revision(0, &yaml, &author, "")
            .await?;
    }

    database
        .latest_daemon_config()
        .await?
        .map(|revision| daemon_config::from_database(revision.summary.revision, revision.yaml))
        .transpose()
}

fn spawn_database_daemon_config_poller(database: Database, store: DaemonConfigStore) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        loop {
            interval.tick().await;
            let result = async {
                let Some(latest) = database.latest_daemon_config_revision().await? else {
                    return Ok::<_, anyhow::Error>(());
                };
                let current = match store.current() {
                    Some(config) => i64::try_from(config.revision)
                        .context("active daemon configuration revision is out of range")?,
                    None => 0,
                };
                if latest <= current {
                    return Ok(());
                }
                let revision = database
                    .daemon_config_revision(latest)
                    .await?
                    .context("latest daemon configuration revision is missing")?;
                let config =
                    daemon_config::from_database(revision.summary.revision, revision.yaml)?;
                if store.publish(config) {
                    info!(
                        revision = latest,
                        "loaded daemon configuration revision from database"
                    );
                }
                Ok(())
            }
            .await;
            if let Err(error) = result {
                warn!(
                    error = %format!("{error:#}"),
                    "failed to poll daemon configuration revisions"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn database_configuration_is_seeded_once_and_stays_empty_without_a_seed() {
        let directory = std::env::temp_dir().join(format!(
            "agentdesktop-config-seed-test-{}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).expect("create test directory");
        let database_path = directory.join("controller.db");
        let database = Database::connect(&format!("sqlite://{}?mode=rwc", database_path.display()))
            .await
            .expect("connect test database");

        assert!(
            load_database_daemon_config(&database, None)
                .await
                .expect("load empty database")
                .is_none()
        );
        assert!(
            database
                .latest_daemon_config_revision()
                .await
                .expect("read empty revision table")
                .is_none()
        );

        let seed_path = directory.join("daemon.yaml");
        std::fs::write(&seed_path, "programs: {}\n").expect("write seed");
        let seeded = load_database_daemon_config(&database, Some(&seed_path))
            .await
            .expect("load seed")
            .expect("seeded configuration");
        assert_eq!(seeded.revision, 1);

        std::fs::write(&seed_path, "invalid: [").expect("replace seed");
        let still_seeded = load_database_daemon_config(&database, Some(&seed_path))
            .await
            .expect("load existing configuration")
            .expect("existing configuration");
        assert_eq!(still_seeded.sha256, seeded.sha256);
        assert_eq!(
            database
                .latest_daemon_config_revision()
                .await
                .expect("read seeded revision"),
            Some(1)
        );

        drop(database);
        std::fs::remove_dir_all(directory).expect("remove test directory");
    }
}
