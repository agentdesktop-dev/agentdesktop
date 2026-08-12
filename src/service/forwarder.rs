use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use http::uri::Authority;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::hbone::HboneClient;

const GATEWAY_UNAVAILABLE: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\ncontent-type: text/plain; charset=utf-8\r\nx-agentdesktop-error: gateway-unavailable\r\ncontent-length: 26\r\nconnection: close\r\n\r\nagent gateway unavailable\n";

pub async fn serve_native(
    listener: TcpListener,
    hbone: HboneClient,
    destination: Authority,
    max_tunnels: usize,
    shutdown_timeout: Duration,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    serve(
        listener,
        hbone,
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
    destination: Authority,
    max_tunnels: usize,
    shutdown_timeout: Duration,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    serve_resolved(
        listener,
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
    public_destination: std::net::SocketAddr,
    destination: Authority,
    max_tunnels: usize,
    shutdown_timeout: Duration,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    serve_resolved(
        listener,
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
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    tracing::warn!(event = "tunnel_rejected", reason = "overloaded");
                    continue;
                };
                rejections.spawn(async move {
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
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    tracing::warn!(event = "tunnel_rejected", reason = "overloaded");
                    continue;
                };
                let resolve = resolve.clone();
                tunnels.spawn(async move {
                    let (stream, resolved) = resolve(stream).await;
                    let result = match resolved {
                        Ok((hbone, destination)) => forward(stream, &hbone, Ok(destination)).await,
                        Err(error) => reject(stream, error).await,
                    };
                    drop(permit);
                    if let Err(error) = result {
                        tracing::warn!(event = "tunnel_failed", reason = %error);
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

#[cfg(any(target_os = "linux", all(target_os = "windows", target_env = "msvc")))]
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

    use super::{HboneClient, serve_native};
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
        let relay = tokio::spawn(serve_native(
            listener,
            hbone,
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
        let relay = tokio::spawn(serve_native(
            listener,
            hbone,
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
        let _ = shutdown_tx.send(());
        relay.await.unwrap().unwrap();
    }

    #[cfg(all(target_os = "windows", target_env = "msvc"))]
    #[tokio::test]
    async fn windows_native_listener_fails_closed_without_wfp_attribution() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown, stopping) = oneshot::channel();
        let service = tokio::spawn(serve_native_without_attribution(listener, 8, async move {
            let _ = stopping.await;
        }));
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
