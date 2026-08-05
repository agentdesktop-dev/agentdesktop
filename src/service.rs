use std::time::{Duration, SystemTime};

use anyhow::{Context, bail};
use tokio::net::TcpListener;

#[cfg(target_os = "linux")]
use crate::capture::CaptureRelay;
use crate::config::{Config, upstream_origin};
use crate::identity::enrollment::{
    EnrollmentClient, certificate_expired, certificate_renewal_due, load_device_identity_for,
    load_enrollment_for,
};
use crate::identity::oauth::{ManagedIdentity, load_session_for};
use crate::identity::storage::{CredentialStore, default_storage_root};
use crate::local_gateway::LocalGateway;
use crate::proxy::{self, ManagedDeviceIdentity, ProxyOptions};

const LOCAL_GATEWAY_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const CERTIFICATE_RENEW_BEFORE: Duration = Duration::from_secs(6 * 60 * 60);
const CERTIFICATE_RENEWAL_CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);
const CERTIFICATE_RENEWAL_RETRY_INTERVAL: Duration = Duration::from_secs(60);

struct RenewalContext {
    enrollment_url: url::Url,
    gateway_origin: url::Url,
    identity: ManagedIdentity,
    issuer: url::Url,
    proxy_identity: ManagedDeviceIdentity,
    store: CredentialStore,
}

pub async fn run(config: Config) -> anyhow::Result<()> {
    let _telemetry = crate::telemetry::init()?;
    let (identity, device_identity, renewal_context) = managed_identity(&config)?;
    let mut local_gateway = match (&config.gateway_binary, &config.gateway_config) {
        (Some(binary), Some(gateway_config)) => {
            tracing::info!(event = "local_gateway_starting");
            Some(LocalGateway::spawn(binary, gateway_config)?)
        }
        _ => None,
    };
    if let Some(gateway) = &mut local_gateway {
        gateway
            .wait_until_reachable(&config.upstream, LOCAL_GATEWAY_STARTUP_TIMEOUT)
            .await?;
        tracing::info!(event = "local_gateway_ready");
    }
    #[cfg(target_os = "linux")]
    let mut capture_relay = if config.capture_enabled {
        let gateway = local_gateway
            .as_ref()
            .context("capture requires an owned local Agent Gateway")?;
        let relay = CaptureRelay::start(
            "127.0.0.1:15001".parse()?,
            "127.0.0.1:15008".parse()?,
            gateway.capture_token(),
            config.max_in_flight,
        )
        .await?;
        tracing::info!(event = "capture_relay_ready", listen = %relay.local_addr()?);
        Some(tokio::spawn(relay.serve(std::future::pending())))
    } else {
        None
    };
    let listener = TcpListener::bind(config.listen).await?;
    tracing::info!(event = "connector_started", mode = config.mode.as_str(), listen = %listener.local_addr()?);
    let renewal_task = renewal_context.map(|context| tokio::spawn(renew_certificate(context)));
    let serve = proxy::serve_with_rotating_managed_identity(
        listener,
        config.upstream,
        config.mode,
        identity,
        device_identity,
        ProxyOptions {
            connect_timeout: Duration::from_millis(config.connect_timeout_ms),
            request_timeout: Duration::from_millis(config.request_timeout_ms),
            shutdown_timeout: Duration::from_millis(config.shutdown_timeout_ms),
            max_in_flight: config.max_in_flight,
        },
        shutdown_signal(),
    );
    let result = if let Some(gateway) = &mut local_gateway {
        tokio::select! {
            result = serve => {
                gateway.stop().await?;
                result
            }
            status = gateway.wait() => {
                bail!("local Agent Gateway exited unexpectedly with {}", status?);
            }
            result = async {
                match &mut capture_relay {
                    Some(relay) => relay.await.context("capture relay task failed")?,
                    None => std::future::pending().await,
                }
            } => {
                gateway.stop().await?;
                result
            }
        }
    } else {
        serve.await
    };
    #[cfg(target_os = "linux")]
    if let Some(relay) = capture_relay {
        relay.abort();
    }
    if let Some(task) = renewal_task {
        task.abort();
    }
    result
}

fn managed_identity(
    config: &Config,
) -> anyhow::Result<(
    Option<ManagedIdentity>,
    Option<ManagedDeviceIdentity>,
    Option<RenewalContext>,
)> {
    let Some(issuer) = &config.identity_issuer else {
        return Ok((None, None, None));
    };
    let identity_dir = config
        .identity_dir
        .clone()
        .map_or_else(default_storage_root, Ok)?;
    let store = CredentialStore::load(&identity_dir)?;
    let gateway_origin = upstream_origin(&config.upstream)?;
    let session = load_session_for(issuer, &gateway_origin, &store)?;
    let identity = ManagedIdentity::new(session, store.clone());
    let device_identity =
        ManagedDeviceIdentity::new(load_device_identity_for(issuer, &gateway_origin, &store)?);
    let enrollment_url = config
        .enrollment_url
        .clone()
        .context("managed identity requires an enrollment URL")?;
    Ok((
        Some(identity.clone()),
        Some(device_identity.clone()),
        Some(RenewalContext {
            enrollment_url,
            gateway_origin,
            identity,
            issuer: issuer.clone(),
            proxy_identity: device_identity,
            store,
        }),
    ))
}

async fn renew_certificate(context: RenewalContext) {
    loop {
        let delay = match renew_certificate_once(&context).await {
            Ok(true) => {
                tracing::info!(event = "device_certificate_renewed");
                CERTIFICATE_RENEWAL_CHECK_INTERVAL
            }
            Ok(false) => CERTIFICATE_RENEWAL_CHECK_INTERVAL,
            Err(_) => {
                tracing::warn!(event = "device_certificate_renewal_failed");
                CERTIFICATE_RENEWAL_RETRY_INTERVAL
            }
        };
        tokio::time::sleep(delay).await;
    }
}

async fn renew_certificate_once(context: &RenewalContext) -> anyhow::Result<bool> {
    let enrollment = load_enrollment_for(&context.issuer, &context.gateway_origin, &context.store)?;
    if !certificate_renewal_due(&enrollment, SystemTime::now(), CERTIFICATE_RENEW_BEFORE)? {
        return Ok(false);
    }
    let client = EnrollmentClient::new(&context.enrollment_url)?;
    if certificate_expired(&enrollment, SystemTime::now())? {
        client
            .recover_and_save(
                &context.identity,
                &context.issuer,
                &context.gateway_origin,
                &context.store,
            )
            .await?;
    } else {
        client
            .renew_and_save(
                &context.identity,
                &context.issuer,
                &context.gateway_origin,
                &context.store,
            )
            .await?;
    }
    let replacement =
        load_device_identity_for(&context.issuer, &context.gateway_origin, &context.store)?;
    context.proxy_identity.replace(replacement)?;
    Ok(true)
}

async fn shutdown_signal() {
    if tokio::signal::ctrl_c().await.is_err() {
        tracing::error!(event = "shutdown_signal_failed");
    }
}
