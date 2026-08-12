use std::time::{Duration, SystemTime};

use anyhow::Context;

use super::hbone::RotatingClientIdentity;
use crate::config::{Config, upstream_origin};
use crate::identity::enrollment::{
    EnrollmentClient, certificate_expired, certificate_renewal_due, load_client_identity_for,
    load_enrollment_for,
};
use crate::identity::oauth::{ManagedIdentity, load_session_for};
use crate::identity::storage::{CredentialStore, default_storage_root};

const CERTIFICATE_RENEW_BEFORE: Duration = Duration::from_secs(6 * 60 * 60);
const CERTIFICATE_RENEWAL_CHECK_INTERVAL: Duration = Duration::from_secs(15 * 60);
const CERTIFICATE_RENEWAL_RETRY_INTERVAL: Duration = Duration::from_secs(60);
const PERSISTED_IDENTITY_CHECK_INTERVAL: Duration = Duration::from_secs(5);

pub struct ManagedIdentityContext {
    pub identity: ManagedIdentity,
    pub client_identity: RotatingClientIdentity,
    renewal: RenewalContext,
}

struct RenewalContext {
    enrollment_url: url::Url,
    gateway_origin: url::Url,
    identity: ManagedIdentity,
    issuer: url::Url,
    tunnel_identity: RotatingClientIdentity,
    store: CredentialStore,
}

pub fn load(config: &Config) -> anyhow::Result<Option<ManagedIdentityContext>> {
    let Some(issuer) = &config.identity_issuer else {
        return Ok(None);
    };
    let identity_dir = config
        .identity_dir
        .clone()
        .map_or_else(default_storage_root, Ok)?;
    let store = CredentialStore::load(&identity_dir)?;
    let gateway_origin = upstream_origin(&config.upstream)?;
    let session = load_session_for(issuer, &gateway_origin, &store)?;
    let identity = ManagedIdentity::new(session, store.clone());
    let client_identity =
        RotatingClientIdentity::new(load_client_identity_for(issuer, &gateway_origin, &store)?);
    let enrollment_url = config
        .enrollment_url
        .clone()
        .context("managed identity requires an enrollment URL")?;
    Ok(Some(ManagedIdentityContext {
        identity: identity.clone(),
        client_identity: client_identity.clone(),
        renewal: RenewalContext {
            enrollment_url,
            gateway_origin,
            identity,
            issuer: issuer.clone(),
            tunnel_identity: client_identity,
            store,
        },
    }))
}

pub fn spawn(context: ManagedIdentityContext) -> tokio::task::JoinHandle<()> {
    tokio::spawn(renew_certificate(context.renewal))
}

async fn renew_certificate(context: RenewalContext) {
    let mut next_renewal_check = tokio::time::Instant::now();
    loop {
        match reload_client_identity(&context) {
            Ok(true) => tracing::info!(event = "device_certificate_reloaded"),
            Ok(false) => {}
            Err(_) => tracing::warn!(event = "device_certificate_reload_failed"),
        }

        if tokio::time::Instant::now() >= next_renewal_check {
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
            next_renewal_check = tokio::time::Instant::now() + delay;
        }
        tokio::time::sleep(PERSISTED_IDENTITY_CHECK_INTERVAL).await;
    }
}

fn reload_client_identity(context: &RenewalContext) -> anyhow::Result<bool> {
    let replacement =
        load_client_identity_for(&context.issuer, &context.gateway_origin, &context.store)?;
    context.tunnel_identity.replace_if_changed(replacement)
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
        load_client_identity_for(&context.issuer, &context.gateway_origin, &context.store)?;
    context.tunnel_identity.replace_if_changed(replacement)?;
    Ok(true)
}
