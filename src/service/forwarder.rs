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
    if max_tunnels == 0 {
        bail!("max tunnels must be greater than zero");
    }
    let permits = Arc::new(Semaphore::new(max_tunnels));
    let destination_for = Arc::new(destination_for);
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
                let hbone = hbone.clone();
                let destination_for = destination_for.clone();
                let fixed_destination = fixed_destination.clone();
                tunnels.spawn(async move {
                    let destination = destination_for(&stream, &fixed_destination);
                    let result = forward(stream, &hbone, destination).await;
                    drop(permit);
                    if let Err(error) = result {
                        tracing::warn!(event = "tunnel_failed", reason = %error);
                    }
                });
            }
        }
    }
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
        let hbone = HboneClient::connect(endpoint).await.unwrap();
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
}
