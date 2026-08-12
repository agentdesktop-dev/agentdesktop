use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use http::uri::Authority;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::hbone::HboneClient;

const GATEWAY_UNAVAILABLE: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\ncontent-type: text/plain; charset=utf-8\r\nx-agentdesktop-error: gateway-unavailable\r\ncontent-length: 26\r\nconnection: close\r\n\r\nagent gateway unavailable\n";

#[derive(Clone, Default)]
pub struct ForwarderMetrics {
    counters: Arc<ForwarderCounters>,
}

#[derive(Default)]
struct ForwarderCounters {
    requests: AtomicU64,
    upstream_responses: AtomicU64,
    identity_failures: AtomicU64,
    overload_rejections: AtomicU64,
    upstream_timeouts: AtomicU64,
    upstream_failures: AtomicU64,
    in_flight: AtomicUsize,
}

#[derive(serde::Serialize)]
pub struct ForwarderMetricsSnapshot {
    requests: u64,
    upstream_responses: u64,
    identity_failures: u64,
    overload_rejections: u64,
    upstream_timeouts: u64,
    upstream_failures: u64,
}

impl ForwarderMetrics {
    pub fn snapshot(&self) -> ForwarderMetricsSnapshot {
        ForwarderMetricsSnapshot {
            requests: self.counters.requests.load(Ordering::Relaxed),
            upstream_responses: self.counters.upstream_responses.load(Ordering::Relaxed),
            identity_failures: self.counters.identity_failures.load(Ordering::Relaxed),
            overload_rejections: self.counters.overload_rejections.load(Ordering::Relaxed),
            upstream_timeouts: self.counters.upstream_timeouts.load(Ordering::Relaxed),
            upstream_failures: self.counters.upstream_failures.load(Ordering::Relaxed),
        }
    }

    pub fn in_flight(&self) -> usize {
        self.counters.in_flight.load(Ordering::Relaxed)
    }

    fn start_flow(&self) -> ActiveFlow {
        self.counters.in_flight.fetch_add(1, Ordering::Relaxed);
        ActiveFlow(self.clone())
    }

    fn record_failure(&self, error: &anyhow::Error) {
        if error
            .chain()
            .any(|cause| cause.is::<tokio::time::error::Elapsed>())
        {
            self.counters
                .upstream_timeouts
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters
                .upstream_failures
                .fetch_add(1, Ordering::Relaxed);
        }
    }
}

struct ActiveFlow(ForwarderMetrics);

impl Drop for ActiveFlow {
    fn drop(&mut self) {
        self.0.counters.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

pub async fn serve_native(
    listener: TcpListener,
    hbone: HboneClient,
    metrics: ForwarderMetrics,
    destination: Authority,
    max_tunnels: usize,
    shutdown_timeout: Duration,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    serve(
        listener,
        hbone,
        metrics,
        max_tunnels,
        shutdown_timeout,
        shutdown,
        move |_, destination| Ok(destination.clone()),
        destination,
    )
    .await
}

#[cfg(target_os = "linux")]
pub async fn serve_native_sessions(
    listener: TcpListener,
    sessions: crate::session::SessionRegistry<u32>,
    metrics: ForwarderMetrics,
    destination: Authority,
    max_tunnels: usize,
    shutdown_timeout: Duration,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    serve_resolved(
        listener,
        metrics,
        max_tunnels,
        shutdown_timeout,
        shutdown,
        move |stream| {
            let sessions = sessions.clone();
            let destination = destination.clone();
            async move {
                let resolved = crate::session::linux::client_for_native(&sessions, &stream)
                    .await
                    .map(|client| (client, destination));
                (stream, resolved)
            }
        },
    )
    .await
}

#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub async fn serve_native_sessions(
    listener: TcpListener,
    sessions: crate::session::SessionRegistry<crate::session::windows::UserSid>,
    metrics: ForwarderMetrics,
    public_destination: std::net::SocketAddr,
    destination: Authority,
    max_tunnels: usize,
    shutdown_timeout: Duration,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    serve_resolved(
        listener,
        metrics,
        max_tunnels,
        shutdown_timeout,
        shutdown,
        move |stream| {
            let sessions = sessions.clone();
            let destination = destination.clone();
            async move {
                let resolved = async {
                    let context = crate::platform::windows::redirect_context(&stream)?;
                    if context.original_destination != public_destination {
                        bail!("WFP native flow has an unexpected original destination");
                    }
                    let client =
                        crate::session::windows::client_for_sid(&sessions, &context.user_sid)
                            .await?;
                    Ok((client, destination))
                }
                .await;
                (stream, resolved)
            }
        },
    )
    .await
}

#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub async fn serve_native_without_attribution(
    listener: TcpListener,
    metrics: ForwarderMetrics,
    max_tunnels: usize,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    if max_tunnels == 0 {
        bail!("max tunnels must be greater than zero");
    }
    let permits = Arc::new(Semaphore::new(max_tunnels));
    let mut rejections = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            Some(result) = rejections.join_next(), if !rejections.is_empty() => {
                if let Err(error) = result {
                    tracing::warn!(event = "tunnel_task_failed", reason = %error);
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                metrics.counters.requests.fetch_add(1, Ordering::Relaxed);
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    metrics.counters.overload_rejections.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(event = "tunnel_rejected", reason = "overloaded");
                    continue;
                };
                let metrics = metrics.clone();
                rejections.spawn(async move {
                    let _active_flow = metrics.start_flow();
                    metrics.counters.identity_failures.fetch_add(1, Ordering::Relaxed);
                    if let Err(error) = reject(
                        stream,
                        anyhow::anyhow!("Windows WFP user attribution is unavailable"),
                    )
                    .await
                    {
                        tracing::warn!(event = "tunnel_failed", reason = %error);
                    }
                    drop(permit);
                });
            }
        }
    }
    rejections.abort_all();
    Ok(())
}

#[cfg(target_os = "linux")]
pub async fn serve_capture(
    listener: TcpListener,
    hbone: HboneClient,
    max_tunnels: usize,
    shutdown_timeout: Duration,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    serve(
        listener,
        hbone,
        ForwarderMetrics::default(),
        max_tunnels,
        shutdown_timeout,
        shutdown,
        |stream, _| super::capture::original_authority(stream),
        "unused.invalid:1".parse()?,
    )
    .await
}

#[cfg(target_os = "linux")]
pub async fn serve_capture_sessions(
    listener: TcpListener,
    sessions: crate::session::SessionRegistry<u32>,
    max_tunnels: usize,
    shutdown_timeout: Duration,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    serve_resolved(
        listener,
        metrics,
        max_tunnels,
        shutdown_timeout,
        shutdown,
        move |stream| {
            let sessions = sessions.clone();
            async move {
                let resolved = async {
                    let original_destination = super::capture::original_socket_address(&stream)?;
                    let client = crate::session::linux::client_for_capture(
                        &sessions,
                        &stream,
                        original_destination,
                    )
                    .await?;
                    let destination = original_destination.to_string().parse()?;
                    Ok((client, destination))
                }
                .await;
                (stream, resolved)
            }
        },
    )
    .await
}

async fn serve<F>(
    listener: TcpListener,
    hbone: HboneClient,
    metrics: ForwarderMetrics,
    max_tunnels: usize,
    shutdown_timeout: Duration,
    shutdown: impl Future<Output = ()>,
    destination_for: F,
    fixed_destination: Authority,
) -> Result<()>
where
    F: Fn(&TcpStream, &Authority) -> Result<Authority> + Send + Sync + 'static,
{
    let destination_for = Arc::new(destination_for);
    serve_resolved(
        listener,
        metrics,
        max_tunnels,
        shutdown_timeout,
        shutdown,
        move |stream| {
            let hbone = hbone.clone();
            let destination_for = destination_for.clone();
            let fixed_destination = fixed_destination.clone();
            async move {
                let resolved = destination_for(&stream, &fixed_destination)
                    .map(|destination| (hbone, destination));
                (stream, resolved)
            }
        },
    )
    .await
}

async fn serve_resolved<Resolve, Resolved>(
    listener: TcpListener,
    metrics: ForwarderMetrics,
    max_tunnels: usize,
    shutdown_timeout: Duration,
    shutdown: impl Future<Output = ()>,
    resolve: Resolve,
) -> Result<()>
where
    Resolve: Fn(TcpStream) -> Resolved + Send + Sync + 'static,
    Resolved: Future<Output = (TcpStream, Result<(HboneClient, Authority)>)> + Send + 'static,
{
    if max_tunnels == 0 {
        bail!("max tunnels must be greater than zero");
    }
    let permits = Arc::new(Semaphore::new(max_tunnels));
    let resolve = Arc::new(resolve);
    let mut tunnels = JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            Some(result) = tunnels.join_next(), if !tunnels.is_empty() => {
                if let Err(error) = result {
                    tracing::warn!(event = "tunnel_task_failed", reason = %error);
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                metrics.counters.requests.fetch_add(1, Ordering::Relaxed);
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    metrics.counters.overload_rejections.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(event = "tunnel_rejected", reason = "overloaded");
                    continue;
                };
                let resolve = resolve.clone();
                let metrics = metrics.clone();
                tunnels.spawn(async move {
                    let _active_flow = metrics.start_flow();
                    let (stream, resolved) = resolve(stream).await;
                    let (result, resolution_failed) = match resolved {
                        Ok((hbone, destination)) => {
                            (forward(stream, &hbone, Ok(destination)).await, false)
                        }
                        Err(error) => {
                            metrics.counters.identity_failures.fetch_add(1, Ordering::Relaxed);
                            (reject(stream, error).await, true)
                        }
                    };
                    drop(permit);
                    match result {
                        Ok(()) => {
                            metrics.counters.upstream_responses.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) => {
                            if !resolution_failed {
                                metrics.record_failure(&error);
                            }
                            tracing::warn!(event = "tunnel_failed", reason = %error);
                        }
                    }
                });
            }
        }
    }
    drain_tunnels(&mut tunnels, shutdown_timeout).await
}

async fn drain_tunnels(tunnels: &mut JoinSet<()>, shutdown_timeout: Duration) -> Result<()> {
    if tokio::time::timeout(shutdown_timeout, async {
        while tunnels.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        tunnels.abort_all();
        bail!("tunnel shutdown exceeded configured timeout");
    }
    Ok(())
}

async fn reject(mut stream: TcpStream, error: anyhow::Error) -> Result<()> {
    let _ = stream.write_all(GATEWAY_UNAVAILABLE).await;
    stream.shutdown().await?;
    Err(error)
}

async fn forward(
    mut stream: TcpStream,
    hbone: &HboneClient,
    destination: Result<Authority>,
) -> Result<()> {
    let result = async {
        let mut tunnel = hbone.open_tunnel(destination?).await?;
        tokio::io::copy_bidirectional(&mut stream, &mut tunnel).await?;
        tunnel.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = stream.write_all(GATEWAY_UNAVAILABLE).await;
    }
    stream.shutdown().await?;
    result
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http::Response;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;
    use tokio::time::{Duration, timeout};

    use super::{ForwarderMetrics, HboneClient, serve_native};
    #[cfg(all(target_os = "windows", target_env = "msvc"))]
    use super::{serve_native_sessions, serve_native_without_attribution};

    #[tokio::test]
    async fn native_listener_forwards_opaque_bytes_to_fixed_authority() {
        let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hbone = HboneClient::connect(gateway.local_addr().unwrap())
            .await
            .unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = gateway.accept().await.unwrap();
            let mut connection = h2::server::handshake(stream).await.unwrap();
            if let Some(result) = connection.accept().await {
                let (request, mut respond) = result.unwrap();
                tokio::spawn(async move {
                    assert_eq!(request.uri().authority().unwrap(), "native.internal:18443");
                    let mut receive = request.into_body();
                    let mut send = respond.send_response(Response::new(()), false).unwrap();
                    let bytes = receive.data().await.unwrap().unwrap();
                    receive
                        .flow_control()
                        .release_capacity(bytes.len())
                        .unwrap();
                    send.send_data(Bytes::from_static(b"HTTP/1.1 200 OK\r\n\r\nSMOKE_OK"), true)
                        .unwrap();
                });
            }
            while connection.accept().await.is_some() {}
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let metrics = ForwarderMetrics::default();
        let metrics_snapshot = metrics.clone();
        let relay = tokio::spawn(serve_native(
            listener,
            hbone,
            metrics,
            "native.internal:18443".parse().unwrap(),
            1,
            Duration::from_secs(1),
            async {
                let _ = shutdown_rx.await;
            },
        ));
        let response = timeout(Duration::from_secs(2), async {
            let mut client = TcpStream::connect(address).await.unwrap();
            client
                .write_all(b"POST /v1/messages HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            client.shutdown().await.unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            response
        })
        .await
        .expect("native forwarding exchange timed out");
        assert!(response.ends_with(b"SMOKE_OK"));
        let snapshot = metrics_snapshot.snapshot();
        assert_eq!(snapshot.requests, 1);
        assert_eq!(snapshot.upstream_responses, 1);
        assert_eq!(snapshot.upstream_failures, 0);
        assert_eq!(metrics_snapshot.in_flight(), 0);
        let _ = shutdown_tx.send(());
        relay.await.unwrap().unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_listener_fails_closed_when_gateway_is_unavailable() {
        let unavailable = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = unavailable.local_addr().unwrap();
        drop(unavailable);
        let hbone = HboneClient::connect_with_headers(
            endpoint,
            http::HeaderMap::new(),
            Duration::from_millis(250),
        )
        .await
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let metrics = ForwarderMetrics::default();
        let metrics_snapshot = metrics.clone();
        let relay = tokio::spawn(serve_native(
            listener,
            hbone,
            metrics,
            "native.internal:18443".parse().unwrap(),
            1,
            Duration::from_secs(1),
            async {
                let _ = shutdown_rx.await;
            },
        ));

        let response = timeout(Duration::from_secs(2), async {
            let mut client = TcpStream::connect(address).await.unwrap();
            client
                .write_all(b"POST /v1/messages HTTP/1.1\r\n\r\n")
                .await
                .unwrap();
            client.shutdown().await.unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();
            response
        })
        .await
        .expect("fail-closed response timed out");

        assert!(response.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(response.ends_with(b"agent gateway unavailable\n"));
        let snapshot = metrics_snapshot.snapshot();
        assert_eq!(snapshot.requests, 1);
        assert_eq!(snapshot.upstream_responses, 0);
        assert_eq!(snapshot.upstream_failures, 1);
        assert_eq!(metrics_snapshot.in_flight(), 0);
        let _ = shutdown_tx.send(());
        relay.await.unwrap().unwrap();
    }

    #[cfg(all(target_os = "windows", target_env = "msvc"))]
    #[tokio::test]
    async fn windows_native_listener_fails_closed_without_wfp_attribution() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, stopping) = oneshot::channel();
        let service = tokio::spawn(serve_native_without_attribution(
            listener,
            ForwarderMetrics::default(),
            8,
            async move {
                let _ = stopping.await;
            },
        ));
        let mut client = TcpStream::connect(address).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        assert!(response.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
        assert!(response.ends_with(b"agent gateway unavailable\n"));
        shutdown.send(()).unwrap();
        service.await.unwrap().unwrap();
    }

    #[cfg(all(target_os = "windows", target_env = "msvc"))]
    #[tokio::test]
    async fn windows_attributed_listener_rejects_direct_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let registry = crate::session::SessionRegistry::<crate::session::windows::UserSid>::new(
            crate::config::DeploymentMode::Managed,
            "127.0.0.1:1".parse().unwrap(),
            "gateway.example".to_owned(),
            Duration::from_secs(1),
            crate::service::hbone::TlsRoots::Native,
        );
        let (shutdown, stopping) = oneshot::channel();
        let service = tokio::spawn(serve_native_sessions(
            listener,
            registry,
            ForwarderMetrics::default(),
            "127.0.0.1:8080".parse().unwrap(),
            "native.agentdesktop.internal:18443".parse().unwrap(),
            8,
            Duration::from_secs(1),
            async move {
                let _ = stopping.await;
            },
        ));
        let mut client = TcpStream::connect(address).await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        assert!(response.starts_with(b"HTTP/1.1 502 Bad Gateway\r\n"));
        shutdown.send(()).unwrap();
        service.await.unwrap().unwrap();
    }
}
