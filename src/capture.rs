use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use http::uri::Authority;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use crate::hbone::HboneClient;
use crate::platform::linux::original_destination;

pub async fn serve(
    listener: TcpListener,
    hbone: HboneClient,
    max_tunnels: usize,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let permits = Arc::new(Semaphore::new(max_tunnels));
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => return Ok(()),
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    tracing::warn!(event = "capture_rejected", reason = "overloaded");
                    continue;
                };
                let hbone = hbone.clone();
                tokio::spawn(async move {
                    let result = forward(stream, &hbone).await;
                    drop(permit);
                    if let Err(error) = result {
                        let _ = error;
                        tracing::warn!(event = "capture_failed", reason = "tunnel_unavailable");
                    }
                });
            }
        }
    }
}

async fn forward(stream: TcpStream, hbone: &HboneClient) -> Result<()> {
    let destination = original_destination(&stream)?;
    forward_to(stream, destination, hbone).await
}

async fn forward_to(
    mut stream: TcpStream,
    destination: SocketAddr,
    hbone: &HboneClient,
) -> Result<()> {
    let authority: Authority = destination.to_string().parse()?;
    let mut tunnel = hbone.open_tunnel(authority).await?;
    tokio::io::copy_bidirectional(&mut stream, &mut tunnel).await?;
    stream.shutdown().await?;
    tunnel.shutdown().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http::{Method, Response};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::{Duration, timeout};

    use super::{HboneClient, forward_to};

    #[tokio::test]
    async fn relays_tcp_flow_to_original_destination_over_hbone() {
        let hbone_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hbone_address = hbone_listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = hbone_listener.accept().await.unwrap();
            let mut connection = h2::server::handshake(stream).await.unwrap();
            if let Some(result) = connection.accept().await {
                let (request, mut respond) = result.unwrap();
                tokio::spawn(async move {
                    assert_eq!(request.method(), Method::CONNECT);
                    assert_eq!(request.uri().authority().unwrap(), "203.0.113.9:443");
                    let mut receive = request.into_body();
                    let mut send = respond.send_response(Response::new(()), false).unwrap();
                    let bytes = receive.data().await.unwrap().unwrap();
                    receive
                        .flow_control()
                        .release_capacity(bytes.len())
                        .unwrap();
                    assert_eq!(bytes, Bytes::from_static(b"captured tls bytes"));
                    send.send_data(Bytes::from_static(b"gateway tls bytes"), true)
                        .unwrap();
                });
            }
            while connection.accept().await.is_some() {}
        });

        timeout(Duration::from_secs(1), async {
            let hbone = HboneClient::connect(hbone_address).await.unwrap();
            let relay_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let relay_address = relay_listener.local_addr().unwrap();
            let relay = tokio::spawn(async move {
                let (stream, _) = relay_listener.accept().await.unwrap();
                forward_to(stream, "203.0.113.9:443".parse().unwrap(), &hbone)
                    .await
                    .unwrap();
            });

            let mut client = tokio::net::TcpStream::connect(relay_address).await.unwrap();
            client.write_all(b"captured tls bytes").await.unwrap();
            client.shutdown().await.unwrap();
            let mut response = Vec::new();
            client.read_to_end(&mut response).await.unwrap();

            assert_eq!(response, b"gateway tls bytes");
            relay.await.unwrap();
            server.await.unwrap();
        })
        .await
        .expect("capture relay exchange timed out");
    }
}
