use std::cmp;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use anyhow::{Context as _, Result, bail};
use bytes::{Buf, Bytes};
use h2::client::SendRequest;
use http::uri::Authority;
use http::{Method, Request, StatusCode, Uri};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpStream, ToSocketAddrs};

#[derive(Clone)]
pub struct HboneClient {
    sender: SendRequest<Bytes>,
}

impl HboneClient {
    pub async fn connect(endpoint: impl ToSocketAddrs) -> Result<Self> {
        let stream = TcpStream::connect(endpoint).await?;
        let (sender, connection) = h2::client::handshake(stream)
            .await
            .context("HBONE HTTP/2 handshake failed")?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::warn!(event = "hbone_connection_failed", reason = %error);
            }
        });
        Ok(Self { sender })
    }

    pub async fn open_tunnel(&self, authority: Authority) -> Result<HboneTunnel> {
        if authority.port_u16().is_none() {
            bail!("HBONE destination authority requires an explicit port");
        }
        let uri = Uri::builder().authority(authority).build()?;
        let request = Request::builder()
            .method(Method::CONNECT)
            .uri(uri)
            .body(())?;
        let mut sender = self
            .sender
            .clone()
            .ready()
            .await
            .context("HBONE connection is unavailable")?;
        let (response, send) = sender.send_request(request, false)?;
        let response = response.await.context("HBONE CONNECT response failed")?;
        if response.status() != StatusCode::OK {
            bail!("HBONE CONNECT rejected with status {}", response.status());
        }
        Ok(HboneTunnel {
            receive: response.into_body(),
            send,
            buffered: Bytes::new(),
            write_closed: false,
        })
    }
}

pub struct HboneTunnel {
    receive: h2::RecvStream,
    send: h2::SendStream<Bytes>,
    buffered: Bytes,
    write_closed: bool,
}

impl AsyncRead for HboneTunnel {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if self.buffered.has_remaining() {
                let count = cmp::min(self.buffered.len(), output.remaining());
                output.put_slice(&self.buffered.split_to(count));
                return Poll::Ready(Ok(()));
            }
            match ready!(self.receive.poll_data(context)) {
                Some(Ok(bytes)) => {
                    self.receive
                        .flow_control()
                        .release_capacity(bytes.len())
                        .map_err(h2_error)?;
                    self.buffered = bytes;
                }
                Some(Err(error)) => return Poll::Ready(Err(h2_error(error))),
                None => return Poll::Ready(Ok(())),
            }
        }
    }
}

impl AsyncWrite for HboneTunnel {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        input: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.write_closed {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        if input.is_empty() {
            return Poll::Ready(Ok(0));
        }
        self.send.reserve_capacity(input.len());
        match ready!(self.send.poll_capacity(context)) {
            Some(Ok(0)) => Poll::Pending,
            Some(Ok(capacity)) => {
                let count = cmp::min(capacity, input.len());
                self.send
                    .send_data(Bytes::copy_from_slice(&input[..count]), false)
                    .map_err(h2_error)?;
                Poll::Ready(Ok(count))
            }
            Some(Err(error)) => Poll::Ready(Err(h2_error(error))),
            None => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
        if !self.write_closed {
            self.send.send_data(Bytes::new(), true).map_err(h2_error)?;
            self.write_closed = true;
        }
        Poll::Ready(Ok(()))
    }
}

fn h2_error(error: h2::Error) -> io::Error {
    if error.is_io() {
        error.into_io().expect("h2 I/O error contains io::Error")
    } else {
        io::Error::other(error)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use http::{Method, Response};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::time::{Duration, timeout};

    use super::HboneClient;

    #[tokio::test]
    async fn opens_authority_tunnel_and_preserves_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (verified_tx, verified_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = h2::server::handshake(stream).await.unwrap();
            if let Some(result) = connection.accept().await {
                let (request, mut respond) = result.unwrap();
                tokio::spawn(async move {
                    assert_eq!(request.method(), Method::CONNECT);
                    assert_eq!(request.uri().authority().unwrap(), "203.0.113.7:443");
                    let mut receive = request.into_body();
                    let mut send = respond.send_response(Response::new(()), false).unwrap();
                    let bytes = receive.data().await.unwrap().unwrap();
                    receive
                        .flow_control()
                        .release_capacity(bytes.len())
                        .unwrap();
                    assert_eq!(bytes, Bytes::from_static(b"client tls bytes"));
                    send.send_data(Bytes::from_static(b"gateway tls bytes"), true)
                        .unwrap();
                    verified_tx.send(()).unwrap();
                });
            }
            while connection.accept().await.is_some() {}
        });

        let response = timeout(Duration::from_secs(1), async {
            let client = HboneClient::connect(address).await.unwrap();
            let mut tunnel = client
                .open_tunnel("203.0.113.7:443".parse().unwrap())
                .await
                .unwrap();
            tunnel.write_all(b"client tls bytes").await.unwrap();
            tunnel.shutdown().await.unwrap();
            let mut response = Vec::new();
            tunnel.read_to_end(&mut response).await.unwrap();
            verified_rx.await.unwrap();
            response
        })
        .await
        .expect("HBONE byte exchange timed out");

        assert_eq!(response, b"gateway tls bytes");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_destination_without_port_before_opening_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = h2::server::handshake(stream).await.unwrap();
            timeout(Duration::from_millis(100), connection.accept())
                .await
                .expect_err("client unexpectedly opened an HTTP/2 stream");
        });
        let client = HboneClient::connect(address).await.unwrap();

        let error = match client.open_tunnel("example.com".parse().unwrap()).await {
            Ok(_) => panic!("destination without a port was accepted"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("requires an explicit port"));
        server.await.unwrap();
    }
}
