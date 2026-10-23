use std::{
    collections::HashSet,
    convert::Infallible,
    sync::{Arc, PoisonError},
    time::Duration,
};

use anyhow::Context;
use hyper::{Method, Request, Response, StatusCode, body::Incoming, service::service_fn};
use hyper_util::{
    rt::{TokioExecutor, TokioIo},
    server::conn::auto,
};
use rustls::{
    ServerConfig,
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use tokio::{net::TcpListener, task::JoinSet};

use super::{
    Health, InterceptCa, PendingTunnel, connect_tcp,
    forward::{Body, CredentialSource, Forwarder, GatewayTarget},
    normalize_host, text_response,
};

/// What the listener intercepts and where it sends it.
#[derive(Debug)]
pub struct InterceptConfig {
    /// Hosts that are decrypted and forwarded to the gateway, in lower case.
    /// The CA must cover each of them.
    pub hosts: Vec<String>,
    pub gateway: GatewayTarget,
    /// Pooled gateway connections idle (on the wall clock) for longer than
    /// this are dropped. Defaults to 60 s.
    pub idle_reset: Option<Duration>,
}

/// The shared state of one listener.
pub struct Interceptor {
    ca: Arc<InterceptCa>,
    hosts: HashSet<String>,
    forwarder: Forwarder,
    health: Health,
}

impl std::fmt::Debug for Interceptor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Interceptor")
            .field("hosts", &self.hosts)
            .finish_non_exhaustive()
    }
}

impl Interceptor {
    pub fn new(
        config: InterceptConfig,
        ca: Arc<InterceptCa>,
        credentials: Arc<dyn CredentialSource>,
    ) -> anyhow::Result<Arc<Self>> {
        let hosts: HashSet<String> = config
            .hosts
            .iter()
            .map(|host| normalize_host(host))
            .collect();
        if hosts.is_empty() {
            anyhow::bail!("the intercept listener needs at least one host");
        }
        if let Some(host) = hosts.iter().find(|host| !ca.covers(host)) {
            anyhow::bail!("the intercept CA does not cover {host}");
        }
        Ok(Arc::new(Self {
            ca,
            hosts,
            forwarder: Forwarder::new(config.gateway, credentials, config.idle_reset),
            health: Health::default(),
        }))
    }

    pub fn health(&self) -> &Health {
        &self.health
    }
}

/// Serve proxy connections on `listener`, which must be bound to a loopback
/// address. Runs until dropped; dropping it closes every connection and tunnel.
pub async fn serve(listener: TcpListener, interceptor: Arc<Interceptor>) -> anyhow::Result<()> {
    let local = listener.local_addr().context("read the listener address")?;
    if !local.ip().is_loopback() {
        anyhow::bail!("the intercept listener must be bound to a loopback address, not {local}");
    }
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                // A per-connection accept error (EMFILE, ECONNABORTED) must not
                // end the listener: pause briefly so a persistent one does not
                // spin, and keep accepting.
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => {
                        tracing::warn!(%error, "intercept: accept failed; retrying");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let interceptor = interceptor.clone();
                connections.spawn(async move {
                    let pending: PendingTunnel = Arc::default();
                    let slot = pending.clone();
                    let service = service_fn(move |request| {
                        let interceptor = interceptor.clone();
                        let slot = slot.clone();
                        async move { Ok::<_, Infallible>(proxy_request(request, &interceptor, &slot).await) }
                    });
                    if let Err(error) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await
                    {
                        tracing::debug!(%error, "intercept: proxy connection closed");
                    }
                    let tunnel = pending.lock().unwrap_or_else(PoisonError::into_inner).take();
                    if let Some(tunnel) = tunnel {
                        tunnel.await;
                    }
                });
            }
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    tracing::warn!(%error, "intercept: connection task failed");
                }
            }
        }
    }
}

/// One request on a proxy connection. Only `CONNECT` is served: Claude sends
/// every connection that way, `https://` or not.
async fn proxy_request(
    mut request: Request<Incoming>,
    interceptor: &Arc<Interceptor>,
    slot: &PendingTunnel,
) -> Response<Body> {
    if request.method() != Method::CONNECT {
        return text_response(
            StatusCode::METHOD_NOT_ALLOWED,
            "agentdesktop intercept: only CONNECT is supported",
        );
    }
    let Some((host, port)) = request
        .uri()
        .authority()
        .and_then(|authority| Some((normalize_host(authority.host()), authority.port_u16()?)))
    else {
        return text_response(
            StatusCode::BAD_REQUEST,
            "agentdesktop intercept: CONNECT needs host:port",
        );
    };
    let upgrade = hyper::upgrade::on(&mut request);

    if interceptor.hosts.contains(&host) {
        // An intercepted host on any other port would skip inspection.
        if port != 443 {
            return text_response(
                StatusCode::FORBIDDEN,
                "agentdesktop intercept: this host is only reachable on port 443",
            );
        }
        tracing::debug!(host, "intercept: decrypting");
        let interceptor = interceptor.clone();
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::pin(async move {
            match upgrade.await {
                Ok(upgraded) => {
                    intercepted_connection(TokioIo::new(upgraded), host, interceptor).await
                }
                Err(error) => {
                    tracing::debug!(%error, "intercept: CONNECT upgrade did not complete")
                }
            }
        }));
        return Response::new(super::forward::empty_body());
    }

    // Not intercepted: a plain tunnel. Connect before answering, so a failure
    // reaches the client as a proxy error instead of a reset.
    let outbound = match connect_tcp(&host, port).await {
        Ok(outbound) => outbound,
        Err(error) => {
            tracing::debug!(host, port, %error, "intercept: tunnel destination unreachable");
            interceptor
                .health
                .record_tunnel_failure(format!("could not connect to {host}:{port}: {error}"));
            return text_response(
                StatusCode::BAD_GATEWAY,
                "agentdesktop intercept: destination unreachable",
            );
        }
    };
    interceptor.health.record_tunnel();
    *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(Box::pin(async move {
        match upgrade.await {
            Ok(upgraded) => {
                let mut inbound = TokioIo::new(upgraded);
                let mut outbound = outbound;
                if let Err(error) = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await
                {
                    tracing::debug!(%error, "intercept: tunnel closed with an error");
                }
            }
            Err(error) => tracing::debug!(%error, "intercept: CONNECT upgrade did not complete"),
        }
    }));
    Response::new(super::forward::empty_body())
}

/// Terminate TLS for `host` on a `CONNECT` tunnel and serve HTTP/2 or HTTP/1.1
/// on it, forwarding every request to the gateway.
async fn intercepted_connection(
    io: TokioIo<hyper::upgrade::Upgraded>,
    host: String,
    interceptor: Arc<Interceptor>,
) {
    let config = match server_config(&interceptor.ca, &host) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(host, error = %format!("{error:#}"), "intercept: no certificate");
            return;
        }
    };
    let tls = match tokio_rustls::TlsAcceptor::from(config).accept(io).await {
        Ok(tls) => tls,
        Err(error) => {
            // A client that does not trust the intercept CA, or an SNI that
            // does not match the CONNECT host, ends here.
            tracing::debug!(host, %error, "intercept: TLS handshake failed");
            return;
        }
    };
    let pending: PendingTunnel = Arc::default();
    let slot = pending.clone();
    let service = service_fn(move |request| {
        let interceptor = interceptor.clone();
        let slot = slot.clone();
        let host = host.clone();
        async move {
            Ok::<_, Infallible>(
                interceptor
                    .forwarder
                    .forward(request, &host, &interceptor.health, &slot)
                    .await,
            )
        }
    });
    if let Err(error) = auto::Builder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(TokioIo::new(tls), service)
        .await
    {
        tracing::debug!(%error, "intercept: intercepted connection closed");
    }
    let tunnel = pending
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(tunnel) = tunnel {
        tunnel.await;
    }
}

/// A TLS configuration that answers only for `host`, offering HTTP/2 and
/// HTTP/1.1. It does not offer HTTP/2 extended CONNECT, so WebSockets arrive
/// on HTTP/1.1 connections.
fn server_config(ca: &InterceptCa, host: &str) -> anyhow::Result<Arc<ServerConfig>> {
    let resolver = Arc::new(SingleHost {
        host: host.to_owned(),
        key: ca.leaf(host)?,
    });
    let mut config = ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// Presents the leaf only when the TLS server name is the `CONNECT` host, so a
/// tunnel opened for one host cannot be used to talk to another.
#[derive(Debug)]
struct SingleHost {
    host: String,
    key: Arc<CertifiedKey>,
}

impl ResolvesServerCert for SingleHost {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        hello
            .server_name()
            .is_some_and(|name| normalize_host(name) == self.host)
            .then(|| self.key.clone())
    }
}
