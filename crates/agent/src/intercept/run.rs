use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use agentdesktop_core::{
    config::{ClaudeDesktopGatewayMode, DaemonConfig, is_loopback_url},
    model::InterceptInfo,
};
use anyhow::Context;
use rustls::RootCertStore;
use tokio::{net::TcpListener, sync::watch, task::JoinHandle};
use url::Url;

use super::{
    CredentialSource, GatewayTarget, INTERCEPTED_HOSTS, InterceptConfig, Interceptor, serve, store,
};
use crate::{api, tick::CurrentConfig};

/// The credential policy client id the identity header is issued for: the same
/// one Claude Desktop's credential helper uses in inference mode.
const CLIENT_ID: &str = "claude-desktop";
/// How long a request waits for a credential fetch before it fails. The fetch
/// itself goes on, so a token refresh it started is still stored.
const CREDENTIAL_FETCH_TIMEOUT: Duration = Duration::from_secs(20);
/// A cached credential is not used within this margin of its expiry.
const CREDENTIAL_EXPIRY_MARGIN: Duration = Duration::from_secs(30);
/// Longest a credential is reused after it was fetched, so a revoked device
/// stops being served soon (the gateway validates tokens statelessly).
const CREDENTIAL_REUSE: Duration = Duration::from_secs(60);
/// Wait before trying again when the listener could not start.
const RETRY_AFTER: Duration = Duration::from_secs(10);

/// What the listener needs from the applied configuration. A change to the
/// gateway or to any `llmGateway` setting, such as its authentication,
/// restarts it, which also drops its cached credential.
#[derive(Clone, Debug)]
pub(crate) struct Wanted {
    /// The gateway's scheme, host and port.
    gateway: Url,
    /// The `llmGateway` settings, compared to detect a change.
    settings: serde_json::Value,
    /// The configuration the listener started with. Credentials are fetched
    /// for it, so the identity always belongs to the gateway that requests go
    /// to, even before the configuration is applied and saved.
    config: Arc<DaemonConfig>,
}

impl PartialEq for Wanted {
    fn eq(&self, other: &Self) -> bool {
        self.gateway == other.gateway && self.settings == other.settings
    }
}

impl Eq for Wanted {}

/// The listener settings `config` asks for, or `None` when Claude Desktop is not
/// in intercept mode.
pub(crate) fn wanted(config: &Arc<DaemonConfig>) -> Option<Wanted> {
    let desktop = config.programs.claude_desktop.as_ref()?;
    if desktop.gateway_mode != ClaudeDesktopGatewayMode::Intercept || !desktop.use_llm_gateway {
        return None;
    }
    let gateway = config.llm_gateway.as_ref()?;
    let mut url = gateway
        .intercept_url
        .clone()
        .unwrap_or_else(|| gateway.url.clone());
    url.set_path("/");
    url.set_query(None);
    url.set_fragment(None);
    Some(Wanted {
        gateway: url,
        settings: serde_json::to_value(gateway).unwrap_or_default(),
        config: config.clone(),
    })
}

/// The intercept listener's state, for daemon-info.
#[derive(Debug)]
pub struct Status {
    listen: SocketAddr,
    state_dir: PathBuf,
    inner: Mutex<StatusInner>,
}

#[derive(Debug, Default)]
struct StatusInner {
    enabled: bool,
    running: Option<Arc<Interceptor>>,
    error: Option<String>,
    ca_sha256: Option<String>,
}

impl Status {
    pub fn new(listen: SocketAddr, state_dir: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            listen,
            state_dir,
            inner: Mutex::default(),
        })
    }

    fn update(&self, change: impl FnOnce(&mut StatusInner)) {
        change(&mut self.inner.lock().unwrap_or_else(PoisonError::into_inner));
    }

    pub fn info(&self) -> InterceptInfo {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let health = inner
            .running
            .as_ref()
            .map(|interceptor| interceptor.health().snapshot())
            .unwrap_or_default();
        let ca_certificate = store::certificate_path(&self.state_dir);
        InterceptInfo {
            listen: self.listen.to_string(),
            enabled: inner.enabled,
            bound: inner.running.is_some(),
            error: inner.error.clone(),
            ca_certificate: ca_certificate
                .exists()
                .then(|| ca_certificate.display().to_string()),
            ca_sha256: inner.ca_sha256.clone(),
            forwarded: health.forwarded,
            gateway_failures: health.gateway_failures,
            tunnels: health.tunnels,
            tunnel_failures: health.tunnel_failures,
            last_failure: health
                .last_failure
                .as_ref()
                .map(|(_, message)| message.clone()),
            last_failure_unix_seconds: health.last_failure.as_ref().map(|(at, _)| {
                at.duration_since(UNIX_EPOCH)
                    .map_or(0, |since| since.as_secs())
            }),
        }
    }
}

/// Run the intercept listener while the applied configuration asks for it.
/// It starts, stops and restarts as configurations arrive, and when it cannot
/// start (port taken, CA or credential store unavailable) it retries on its
/// own: Claude Desktop is pinned to this address and cannot connect meanwhile.
pub(crate) async fn supervise(
    listen: SocketAddr,
    state: api::AppState,
    mut current: watch::Receiver<Option<CurrentConfig>>,
    status: Arc<Status>,
) {
    let mut running: Option<(Wanted, JoinHandle<()>)> = None;
    loop {
        let next = current
            .borrow_and_update()
            .as_ref()
            .and_then(|current| wanted(&current.config));
        if running.as_ref().map(|(wanted, _)| wanted) != next.as_ref() {
            if let Some((_, task)) = running.take() {
                task.abort();
                tracing::info!("intercept listener stopped for a configuration change");
            }
            status.update(|inner| {
                *inner = StatusInner {
                    enabled: next.is_some(),
                    ca_sha256: inner.ca_sha256.take(),
                    ..StatusInner::default()
                };
            });
            if let Some(wanted) = next {
                let task = tokio::spawn(run_listener(
                    listen,
                    wanted.clone(),
                    state.clone(),
                    status.clone(),
                ));
                running = Some((wanted, task));
            }
        }
        if current.changed().await.is_err() {
            if let Some((_, task)) = running.take() {
                task.abort();
            }
            return;
        }
    }
}

async fn run_listener(
    listen: SocketAddr,
    wanted: Wanted,
    state: api::AppState,
    status: Arc<Status>,
) {
    loop {
        match start(listen, &wanted, &state, &status).await {
            Ok((listener, interceptor)) => {
                tracing::info!(%listen, gateway = %wanted.gateway, "intercept listener serving");
                status.update(|inner| {
                    inner.running = Some(interceptor.clone());
                    inner.error = None;
                });
                let outcome = serve(listener, interceptor).await;
                let message = match outcome {
                    Ok(()) => "the listener stopped".to_owned(),
                    Err(error) => format!("{error:#}"),
                };
                tracing::error!(error = %message, "intercept listener stopped; restarting");
                status.update(|inner| {
                    inner.running = None;
                    inner.error = Some(message);
                });
            }
            Err(error) => {
                let message = format!("{error:#}");
                tracing::error!(%listen, error = %message, "intercept listener could not start; retrying");
                status.update(|inner| inner.error = Some(message));
            }
        }
        tokio::time::sleep(RETRY_AFTER).await;
    }
}

async fn start(
    listen: SocketAddr,
    wanted: &Wanted,
    state: &api::AppState,
    status: &Status,
) -> anyhow::Result<(TcpListener, Arc<Interceptor>)> {
    let hosts: Vec<String> = INTERCEPTED_HOSTS
        .iter()
        .map(|host| (*host).to_owned())
        .collect();
    let state_dir = state.state_dir.clone();
    let ca = {
        let hosts = hosts.clone();
        tokio::task::spawn_blocking(move || store::load_or_create(&state_dir, &hosts))
            .await
            .context("load the intercept CA")??
    };
    let fingerprint = store::fingerprint(ca.certificate_der());
    status.update(|inner| inner.ca_sha256 = Some(fingerprint));
    let interceptor = Interceptor::new(
        InterceptConfig {
            hosts,
            gateway: gateway_target(&wanted.gateway)?,
            idle_reset: None,
        },
        ca,
        Arc::new(DaemonCredentials {
            state: state.clone(),
            config: wanted.config.clone(),
            cached: Arc::default(),
        }),
    )?;
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("bind {listen}"))?;
    Ok((listener, interceptor))
}

/// The gateway connection for `url` (scheme, host and port only). HTTPS
/// gateways are verified against the system's trusted roots. Plain HTTP is
/// for a gateway on this device only: requests carry claude.ai cookies and
/// tokens. Configuration validation refuses it too.
fn gateway_target(url: &Url) -> anyhow::Result<GatewayTarget> {
    let host = url
        .host_str()
        .context("the gateway URL has no host")?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = url
        .port_or_known_default()
        .context("the gateway URL has no port")?;
    match url.scheme() {
        "http" if is_loopback_url(url) => Ok(GatewayTarget::http(host, port)),
        "http" => anyhow::bail!("a gateway that is not on this device must use HTTPS"),
        "https" => {
            let mut roots = RootCertStore::empty();
            let native = rustls_native_certs::load_native_certs();
            for error in &native.errors {
                tracing::debug!(%error, "intercept: a system root certificate could not be loaded");
            }
            let (added, _) = roots.add_parsable_certificates(native.certs);
            if added == 0 {
                anyhow::bail!("no trusted root certificates found for the gateway connection");
            }
            GatewayTarget::https(host, port, roots)
        }
        scheme => anyhow::bail!("the gateway URL must use HTTP or HTTPS, got {scheme}"),
    }
}

/// Gateway credentials for the identity header, from the daemon's gateway
/// authentication: the controller-issued JWT, or the OIDC access token.
struct DaemonCredentials {
    state: api::AppState,
    /// The configuration the listener started with.
    config: Arc<DaemonConfig>,
    /// The last credential; the lock also makes concurrent misses share one fetch.
    cached: Arc<tokio::sync::Mutex<Option<Cached>>>,
}

struct Cached {
    credential: String,
    expires_at_unix_seconds: u64,
    fetched: Instant,
}

impl Cached {
    fn usable(&self) -> bool {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        self.fetched.elapsed() < CREDENTIAL_REUSE
            && self.expires_at_unix_seconds > now + CREDENTIAL_EXPIRY_MARGIN.as_secs()
    }
}

#[async_trait::async_trait]
impl CredentialSource for DaemonCredentials {
    async fn credential(&self) -> Result<Option<String>, String> {
        let gateway = self
            .config
            .llm_gateway
            .as_ref()
            .ok_or_else(|| "no LLM gateway is configured".to_owned())?;
        if gateway.authentication.is_none() {
            return Ok(None);
        }
        // The fetch runs on its own task so the wait below does not cancel it
        // half-way: a token refresh must finish and be stored. The task holds
        // the cache lock, so of concurrent misses one fetches and the others
        // find its credential.
        let fetch = {
            let state = self.state.clone();
            let config = self.config.clone();
            let cached = self.cached.clone();
            tokio::spawn(async move {
                let mut cached = cached.lock().await;
                if let Some(cached) = cached.as_ref()
                    && cached.usable()
                {
                    return Ok(cached.credential.clone());
                }
                let fetched = api::gateway_credential(&state, &config, CLIENT_ID, false)
                    .await
                    .map_err(|(status, message)| format!("{message} ({status})"))?;
                *cached = Some(Cached {
                    credential: fetched.credential.clone(),
                    expires_at_unix_seconds: fetched.expires_at_unix_seconds,
                    fetched: Instant::now(),
                });
                Ok(fetched.credential)
            })
        };
        tokio::time::timeout(CREDENTIAL_FETCH_TIMEOUT, fetch)
            .await
            .map_err(|_| "the gateway credential fetch did not finish in time".to_owned())?
            .map_err(|error| format!("the gateway credential fetch failed: {error}"))?
            .map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(yaml: &str) -> Arc<DaemonConfig> {
        Arc::new(agentdesktop_core::config::parse_daemon(yaml).unwrap())
    }

    #[test]
    fn the_listener_is_wanted_only_in_intercept_mode() {
        assert_eq!(
            wanted(&config(
                "llmGateway:\n  url: https://gateway.example.com/anthropic\nprograms:\n  claudeDesktop: {}\n"
            )),
            None
        );
        assert_eq!(
            wanted(&config(
                "llmGateway:\n  url: https://gateway.example.com/anthropic\nprograms:\n  claudeDesktop:\n    gatewayMode: intercept\n"
            ))
            .map(|wanted| wanted.gateway.to_string()),
            Some("https://gateway.example.com/".to_owned()),
            "the path of url is not used"
        );
        assert_eq!(
            wanted(&config(
                "llmGateway:\n  url: https://gateway.example.com/anthropic\n  interceptUrl: http://127.0.0.1:4001\nprograms:\n  claudeDesktop:\n    gatewayMode: intercept\n"
            ))
            .map(|wanted| wanted.gateway.to_string()),
            Some("http://127.0.0.1:4001/".to_owned())
        );
    }

    #[test]
    fn any_gateway_setting_change_restarts_the_listener() {
        let base = "llmGateway:\n  url: https://gateway.example.com\n";
        let mode = "programs:\n  claudeDesktop:\n    gatewayMode: intercept\n";
        let oidc = |issuer: &str| {
            format!(
                "{base}  authentication:\n    type: oidc\n    issuer: {issuer}\n    clientId: agentdesktop\n{mode}"
            )
        };
        let first = wanted(&config(&oidc("https://idp.example.com"))).unwrap();
        let same = wanted(&config(&format!(
            "{}  claudeCode: {{}}\n",
            oidc("https://idp.example.com")
        )))
        .unwrap();
        let other_issuer = wanted(&config(&oidc("https://other-idp.example.com"))).unwrap();
        assert_eq!(
            first, same,
            "a change outside llmGateway keeps the listener"
        );
        assert_ne!(first, other_issuer);
        assert_eq!(first.gateway, other_issuer.gateway);
    }

    #[test]
    fn gateway_targets_follow_the_url_scheme() {
        let plain = gateway_target(&"http://127.0.0.1:4001/".parse().unwrap()).unwrap();
        assert_eq!(
            format!("{plain:?}"),
            "GatewayTarget { host: \"127.0.0.1\", port: 4001, tls: false }"
        );
        assert!(gateway_target(&"ftp://gateway.example.com/".parse().unwrap()).is_err());
        assert!(gateway_target(&"http://localhost:4001/".parse().unwrap()).is_ok());
        assert!(gateway_target(&"http://gateway.example.com/".parse().unwrap()).is_err());
    }

    #[test]
    fn a_credential_is_reused_only_briefly_and_never_near_expiry() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let cached = |expires_at_unix_seconds, age| Cached {
            credential: "jwt".to_owned(),
            expires_at_unix_seconds,
            fetched: Instant::now() - age,
        };
        assert!(cached(now + 300, Duration::ZERO).usable());
        assert!(!cached(now + 10, Duration::ZERO).usable());
        assert!(!cached(now + 300, Duration::from_secs(61)).usable());
    }
}
