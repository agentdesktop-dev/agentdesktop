use std::cmp;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll, ready};

use anyhow::{Context as _, Result, bail};
use bytes::{Buf, Bytes};
use h2::client::SendRequest;
use http::HeaderMap;
use http::uri::Authority;
use http::{Method, Request, StatusCode, Uri};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify};

#[derive(Clone)]
pub struct HboneClient {
    inner: Arc<HboneInner>,
}

struct HboneInner {
    endpoint: SocketAddr,
    connect_headers: HeaderMap,
    connection: Mutex<ConnectionState>,
    connection_changed: Notify,
}

#[derive(Default)]
struct ConnectionState {
    generation: u64,
    sender: Option<SendRequest<Bytes>>,
}

impl HboneClient {
    pub async fn connect(endpoint: SocketAddr) -> Result<Self> {
        Self::connect_with_headers(endpoint, HeaderMap::new()).await
    }

    pub async fn connect_with_headers(
        endpoint: SocketAddr,
        connect_headers: HeaderMap,
    ) -> Result<Self> {
        let client = Self {
            inner: Arc::new(HboneInner {
                endpoint,
                connect_headers,
                connection: Mutex::new(ConnectionState::default()),
                connection_changed: Notify::new(),
            }),
        };
        client.sender().await?;
        Ok(client)
    }

    async fn sender(&self) -> Result<(u64, SendRequest<Bytes>)> {
        let mut state = self.inner.connection.lock().await;
        if let Some(sender) = &state.sender {
            return Ok((state.generation, sender.clone()));
        }
        let stream = TcpStream::connect(self.inner.endpoint).await?;
        let (sender, connection) = h2::client::handshake(stream)
            .await
            .context("HBONE HTTP/2 handshake failed")?;
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        state.sender = Some(sender.clone());
        tokio::spawn(drive_connection(
            Arc::downgrade(&self.inner),
            generation,
            connection,
        ));
        Ok((generation, sender))
    }

    async fn invalidate(&self, generation: u64) {
        let mut state = self.inner.connection.lock().await;
        if state.generation == generation {
            state.sender = None;
            self.inner.connection_changed.notify_waiters();
        }
    }

    #[cfg(test)]
    async fn wait_until_disconnected(&self) {
        loop {
            let changed = self.inner.connection_changed.notified();
            if self.inner.connection.lock().await.sender.is_none() {
                return;
            }
            changed.await;
        }
    }

    pub async fn open_tunnel(&self, authority: Authority) -> Result<HboneTunnel> {
        if authority.port_u16().is_none() {
            bail!("HBONE destination authority requires an explicit port");
        }
        let uri = Uri::builder().authority(authority).build()?;
        let mut request = Request::builder()
            .method(Method::CONNECT)
            .uri(uri)
            .body(())?;
        request
            .headers_mut()
            .extend(self.inner.connect_headers.clone());
        let (generation, sender) = self.sender().await?;
        let mut sender = match sender.ready().await {
            Ok(sender) => sender,
            Err(error) => {
                self.invalidate(generation).await;
                return Err(error).context("HBONE connection is unavailable");
            }
        };
        let (response, send) = match sender.send_request(request, false) {
            Ok(stream) => stream,
            Err(error) => {
                self.invalidate(generation).await;
                return Err(error.into());
            }
        };
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

async fn drive_connection(
    inner: Weak<HboneInner>,
    generation: u64,
    connection: h2::client::Connection<TcpStream, Bytes>,
) {
    let result = connection.await;
    if let Some(inner) = inner.upgrade() {
        let mut state = inner.connection.lock().await;
        if state.generation == generation {
            state.sender = None;
            inner.connection_changed.notify_waiters();
        }
    }
    if let Err(error) = result {
        tracing::warn!(event = "hbone_connection_failed", reason = %error);
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
    use http::{HeaderMap, HeaderValue, Method, Response};
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
                    let token = request.headers().get("x-agentgateway-edge-token").unwrap();
                    assert_eq!(token, "test-token");
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
            let mut headers = HeaderMap::new();
            let mut token = HeaderValue::from_static("test-token");
            token.set_sensitive(true);
            headers.insert("x-agentgateway-edge-token", token);
            let client = HboneClient::connect_with_headers(address, headers)
                .await
                .unwrap();
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

    #[tokio::test]
    async fn reconnects_for_a_later_tunnel_after_connection_shutdown() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (closed_tx, closed_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (first, _) = listener.accept().await.unwrap();
            let first = h2::server::handshake(first).await.unwrap();
            drop(first);
            closed_tx.send(()).unwrap();

            let (second, _) = listener.accept().await.unwrap();
            let mut second = h2::server::handshake(second).await.unwrap();
            let handler = if let Some(result) = second.accept().await {
                let (request, mut respond) = result.unwrap();
                Some(tokio::spawn(async move {
                    assert_eq!(request.uri().authority().unwrap(), "203.0.113.11:443");
                    let mut receive = request.into_body();
                    let mut send = respond.send_response(Response::new(()), false).unwrap();
                    while receive.data().await.transpose().unwrap().is_some() {}
                    send.send_data(Bytes::from_static(b"after restart"), true)
                        .unwrap();
                }))
            } else {
                None
            };
            while second.accept().await.is_some() {}
            handler.unwrap().await.unwrap();
        });

        timeout(Duration::from_secs(1), async {
            let client = HboneClient::connect(address).await.unwrap();
            closed_rx.await.unwrap();
            client.wait_until_disconnected().await;
            let mut tunnel = client
                .open_tunnel("203.0.113.11:443".parse().unwrap())
                .await
                .unwrap();
            tunnel.shutdown().await.unwrap();
            let mut response = Vec::new();
            tunnel.read_to_end(&mut response).await.unwrap();
            assert_eq!(response, b"after restart");
            drop(tunnel);
            drop(client);
            server.await.unwrap();
        })
        .await
        .expect("HBONE reconnect timed out");
    }
}
