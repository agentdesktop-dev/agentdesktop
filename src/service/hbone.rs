use std::cmp;
use std::io::{self, Cursor};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, Weak};
use std::task::{Context, Poll, ready};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use bytes::{Buf, Bytes};
use h2::client::SendRequest;
use http::HeaderMap;
use http::uri::Authority;
use http::{Method, Request, StatusCode, Uri};
use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::ServerName;
use rustls::sign::{CertifiedKey, SigningKey, SingleCertAndKey};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Notify, watch};
use tokio_rustls::TlsConnector;

use crate::identity::enrollment::ClientIdentity;

trait TunnelIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> TunnelIo for T where T: AsyncRead + AsyncWrite + Unpin + Send {}
type BoxedIo = Box<dyn TunnelIo>;

#[derive(Clone)]
pub struct RotatingClientIdentity {
    state: Arc<StdMutex<IdentityState>>,
    generation: watch::Sender<u64>,
}

struct IdentityState {
    generation: u64,
    identity: ClientIdentitySource,
}

#[derive(Clone)]
enum ClientIdentitySource {
    Pem(ClientIdentity),
    External(ExternalClientIdentity),
}

#[derive(Clone)]
pub(crate) struct ExternalClientIdentity {
    pub certificates: Vec<CertificateDer<'static>>,
    pub signing_key: Arc<dyn SigningKey>,
}

impl RotatingClientIdentity {
    pub fn new(identity: ClientIdentity) -> Self {
        let (generation, _) = watch::channel(1);
        Self {
            state: Arc::new(StdMutex::new(IdentityState {
                generation: 1,
                identity: ClientIdentitySource::Pem(identity),
            })),
            generation,
        }
    }

    pub(crate) fn new_external(identity: ExternalClientIdentity, generation: u64) -> Self {
        let (generation_tx, _) = watch::channel(generation);
        Self {
            state: Arc::new(StdMutex::new(IdentityState {
                generation,
                identity: ClientIdentitySource::External(identity),
            })),
            generation: generation_tx,
        }
    }

    pub fn replace_if_changed(&self, identity: ClientIdentity) -> Result<bool> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("managed client identity lock poisoned"))?;
        if matches!(
            &state.identity,
            ClientIdentitySource::Pem(current) if current == &identity
        ) {
            return Ok(false);
        }
        state.identity = ClientIdentitySource::Pem(identity);
        state.generation = state.generation.wrapping_add(1);
        self.generation.send_replace(state.generation);
        Ok(true)
    }

    fn snapshot(&self) -> Result<(u64, ClientIdentitySource)> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("managed client identity lock poisoned"))?;
        Ok((state.generation, state.identity.clone()))
    }

    pub(crate) fn pem_snapshot(&self) -> Result<(u64, ClientIdentity)> {
        let (generation, identity) = self.snapshot()?;
        match identity {
            ClientIdentitySource::Pem(identity) => Ok((generation, identity)),
            ClientIdentitySource::External(_) => {
                bail!("external client identity cannot be exported")
            }
        }
    }

    pub(crate) fn subscribe_generation(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }
}

#[derive(Clone)]
enum Transport {
    Plain(SocketAddr),
    Tls {
        endpoint: TlsEndpoint,
        server_name: String,
        identity: RotatingClientIdentity,
        roots: TlsRoots,
    },
}

#[derive(Clone)]
enum TlsEndpoint {
    Resolved(SocketAddr),
    Host { host: String, port: u16 },
}

#[derive(Clone)]
pub enum TlsRoots {
    Native,
    Custom(RootCertStore),
}

#[derive(Clone)]
pub struct HboneClient {
    inner: Arc<HboneInner>,
}

struct HboneInner {
    transport: Transport,
    connect_headers: HeaderMap,
    connect_timeout: Duration,
    connection: Mutex<ConnectionState>,
    connection_changed: Notify,
}

#[derive(Default)]
struct ConnectionState {
    generation: u64,
    identity_generation: u64,
    sender: Option<SendRequest<Bytes>>,
}

impl HboneClient {
    pub async fn connect(endpoint: SocketAddr) -> Result<Self> {
        Self::connect_with_headers(endpoint, HeaderMap::new(), Duration::from_secs(5)).await
    }

    pub async fn connect_with_headers(
        endpoint: SocketAddr,
        connect_headers: HeaderMap,
        connect_timeout: Duration,
    ) -> Result<Self> {
        Self::new(Transport::Plain(endpoint), connect_headers, connect_timeout).await
    }

    pub async fn connect_mtls(
        host: String,
        port: u16,
        identity: RotatingClientIdentity,
        connect_timeout: Duration,
        roots: TlsRoots,
    ) -> Result<Self> {
        Self::new(
            Transport::Tls {
                endpoint: TlsEndpoint::Host {
                    host: host.clone(),
                    port,
                },
                server_name: host,
                identity,
                roots,
            },
            HeaderMap::new(),
            connect_timeout,
        )
        .await
    }

    pub(crate) async fn connect_mtls_endpoint(
        endpoint: SocketAddr,
        server_name: String,
        identity: RotatingClientIdentity,
        connect_timeout: Duration,
        roots: TlsRoots,
    ) -> Result<Self> {
        Self::new(
            Transport::Tls {
                endpoint: TlsEndpoint::Resolved(endpoint),
                server_name,
                identity,
                roots,
            },
            HeaderMap::new(),
            connect_timeout,
        )
        .await
    }

    async fn new(
        transport: Transport,
        connect_headers: HeaderMap,
        connect_timeout: Duration,
    ) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(HboneInner {
                transport,
                connect_headers,
                connect_timeout,
                connection: Mutex::new(ConnectionState::default()),
                connection_changed: Notify::new(),
            }),
        })
    }

    async fn sender(&self) -> Result<(u64, SendRequest<Bytes>)> {
        let (identity_generation, identity) = match &self.inner.transport {
            Transport::Plain(_) => (0, None),
            Transport::Tls { identity, .. } => {
                let (generation, identity) = identity.snapshot()?;
                (generation, Some(identity))
            }
        };
        let mut state = self.inner.connection.lock().await;
        if state.identity_generation != identity_generation {
            state.sender = None;
        }
        if let Some(sender) = &state.sender {
            return Ok((state.generation, sender.clone()));
        }
        let (sender, connection) = tokio::time::timeout(self.inner.connect_timeout, async {
            let io: BoxedIo = match (&self.inner.transport, identity) {
                (Transport::Plain(endpoint), None) => Box::new(TcpStream::connect(endpoint).await?),
                (
                    Transport::Tls {
                        endpoint,
                        server_name,
                        roots,
                        ..
                    },
                    Some(identity),
                ) => Box::new(
                    connect_tls(endpoint, server_name, identity, roots.clone()).await?,
                ),
                _ => unreachable!("transport and identity snapshot must agree"),
            };
            h2::client::handshake(io)
                .await
                .context("HBONE HTTP/2 handshake failed")
        })
        .await
        .context("Agent Gateway connection timed out")??;
        state.generation = state.generation.wrapping_add(1);
        state.identity_generation = identity_generation;
        let generation = state.generation;
        state.sender = Some(sender.clone());
        tokio::spawn(drive_connection(
            Arc::downgrade(&self.inner),
            generation,
            connection,
        ));
        Ok((generation, sender))
    }

    pub async fn is_reachable(&self) -> bool {
        self.sender().await.is_ok()
    }

    async fn invalidate(&self, generation: u64) {
        let mut state = self.inner.connection.lock().await;
        if state.generation == generation {
            state.sender = None;
            self.inner.connection_changed.notify_waiters();
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
        let response = tokio::time::timeout(self.inner.connect_timeout, response)
            .await
            .context("HBONE CONNECT response timed out")?
            .context("HBONE CONNECT response failed")?;
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

async fn connect_tls(
    endpoint: &TlsEndpoint,
    server_name: &str,
    identity: ClientIdentitySource,
    roots: TlsRoots,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let roots = match roots {
        TlsRoots::Native => native_roots()?,
        TlsRoots::Custom(roots) => roots,
    };
    let builder = rustls::ClientConfig::builder().with_root_certificates(roots);
    let mut config = match identity {
        ClientIdentitySource::Pem(identity) => {
            let certificates =
                rustls_pemfile::certs(&mut Cursor::new(identity.certificate_chain_pem))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
            let private_key =
                rustls_pemfile::private_key(&mut Cursor::new(identity.private_key_pem))?
                    .context("managed client identity contains no private key")?;
            builder.with_client_auth_cert(certificates, private_key)?
        }
        ClientIdentitySource::External(identity) => {
            let certified_key = CertifiedKey::new(identity.certificates, identity.signing_key);
            builder.with_client_cert_resolver(Arc::new(SingleCertAndKey::from(certified_key)))
        }
    };
    config.alpn_protocols = vec![b"h2".to_vec()];
    let name = ServerName::try_from(server_name.to_owned())?;
    let stream = match endpoint {
        TlsEndpoint::Resolved(endpoint) => TcpStream::connect(endpoint).await?,
        TlsEndpoint::Host { host, port } => connect_tcp(host, *port).await?,
    };
    Ok(TlsConnector::from(Arc::new(config))
        .connect(name, stream)
        .await?)
}

fn native_roots() -> Result<RootCertStore> {
    crate::identity::tls::root_store()
}

async fn connect_tcp(host: &str, port: u16) -> io::Result<TcpStream> {
    TcpStream::connect((host, port)).await
}

async fn drive_connection(
    inner: Weak<HboneInner>,
    generation: u64,
    connection: h2::client::Connection<BoxedIo, Bytes>,
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
    use http::{Method, Response};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::time::{Duration, timeout};

    use super::{HboneClient, RotatingClientIdentity, connect_tcp};
    use crate::identity::enrollment::ClientIdentity;

    #[test]
    fn rotates_managed_identity_only_when_credentials_change() {
        let first = ClientIdentity {
            certificate_chain_pem: "first certificate".into(),
            private_key_pem: "first key".into(),
        };
        let identity = RotatingClientIdentity::new(first.clone());

        assert!(!identity.replace_if_changed(first).unwrap());
        assert_eq!(identity.snapshot().unwrap().0, 1);

        let second = ClientIdentity {
            certificate_chain_pem: "second certificate".into(),
            private_key_pem: "second key".into(),
        };
        assert!(identity.replace_if_changed(second.clone()).unwrap());
        let (generation, current) = identity.pem_snapshot().unwrap();
        assert_eq!(generation, 2);
        assert!(current == second);
    }

    #[tokio::test]
    async fn hostname_connection_uses_a_reachable_resolved_address() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let connection = tokio::spawn(async move { listener.accept().await.unwrap() });

        connect_tcp("localhost", port).await.unwrap();
        connection.await.unwrap();
    }

    #[tokio::test]
    async fn opens_authority_tunnel_and_preserves_bytes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = h2::server::handshake(stream).await.unwrap();
            if let Some(result) = connection.accept().await {
                let (request, mut respond) = result.unwrap();
                tokio::spawn(async move {
                    assert_eq!(request.method(), Method::CONNECT);
                    assert_eq!(request.uri().authority().unwrap(), "native.internal:18443");
                    let mut receive = request.into_body();
                    let mut send = respond.send_response(Response::new(()), false).unwrap();
                    let bytes = receive.data().await.unwrap().unwrap();
                    receive
                        .flow_control()
                        .release_capacity(bytes.len())
                        .unwrap();
                    assert_eq!(bytes, Bytes::from_static(b"native http bytes"));
                    send.send_data(Bytes::from_static(b"gateway bytes"), true)
                        .unwrap();
                });
            }
            while connection.accept().await.is_some() {}
        });

        let response = timeout(Duration::from_secs(2), async {
            let client = HboneClient::connect(address).await.unwrap();
            let mut tunnel = client
                .open_tunnel("native.internal:18443".parse().unwrap())
                .await
                .unwrap();
            tunnel.write_all(b"native http bytes").await.unwrap();
            tunnel.shutdown().await.unwrap();
            let mut response = Vec::new();
            tunnel.read_to_end(&mut response).await.unwrap();
            response
        })
        .await
        .expect("HBONE exchange timed out");

        assert_eq!(response, b"gateway bytes");
        server.await.unwrap();
    }
}
