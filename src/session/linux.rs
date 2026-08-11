use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::os::fd::AsFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use netlink_packet_core::{NLM_F_REQUEST, NetlinkHeader, NetlinkMessage, NetlinkPayload};
use netlink_packet_sock_diag::{
    SockDiagMessage,
    constants::{AF_INET, AF_INET6, IPPROTO_TCP},
    inet::{ExtensionFlags, InetRequest, SocketId, StateFlags},
};
use netlink_sys::{Socket, SocketAddr as NetlinkSocketAddr, protocols::NETLINK_SOCK_DIAG};
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey as P256SigningKey};
use p256::pkcs8::DecodePrivateKey;
use rustls::pki_types::CertificateDer;
use rustls::sign::{Signer, SigningKey};
use rustls::{Error as TlsError, SignatureAlgorithm, SignatureScheme as TlsSignatureScheme};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::sync::{RwLock, watch};
use tokio::task::JoinSet;

use crate::service::hbone::{ExternalClientIdentity, HboneClient, RotatingClientIdentity};
use crate::session_protocol::{
    AgentResponse, ForwarderRequest, Registration, SignatureScheme, read_frame,
    read_frame_blocking, write_frame, write_frame_blocking,
};

pub struct SessionConnection {
    uid: u32,
    registration: Registration,
    stream: UnixStream,
}

#[derive(Clone)]
pub struct SessionRegistry {
    endpoint: SocketAddr,
    server_name: String,
    connect_timeout: Duration,
    #[cfg(test)]
    roots: Option<rustls::RootCertStore>,
    clients: Arc<RwLock<HashMap<u32, RegisteredClient>>>,
}

#[derive(Clone)]
struct RegisteredClient {
    certificate_generation: u64,
    client: HboneClient,
}

impl SessionRegistry {
    pub fn new(endpoint: SocketAddr, server_name: String, connect_timeout: Duration) -> Self {
        Self {
            endpoint,
            server_name,
            connect_timeout,
            #[cfg(test)]
            roots: None,
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    fn with_roots(mut self, roots: rustls::RootCertStore) -> Self {
        self.roots = Some(roots);
        self
    }

    pub async fn register(&self, connection: SessionConnection) -> Result<()> {
        let uid = connection.uid();
        let generation = connection.registration().certificate_generation;
        let certificates = connection
            .registration()
            .certificate_chain_der_base64
            .iter()
            .map(|certificate| {
                base64::engine::general_purpose::STANDARD
                    .decode(certificate)
                    .map(CertificateDer::from)
                    .context("decode registered certificate")
            })
            .collect::<Result<Vec<_>>>()?;
        let (signing_key, monitor) = connection.into_signing_key(self.connect_timeout)?;
        let identity = RotatingClientIdentity::new_external(
            ExternalClientIdentity {
                certificates,
                signing_key,
            },
            generation,
        );
        #[cfg(test)]
        let client = if let Some(roots) = self.roots.clone() {
            HboneClient::connect_mtls_with_roots(
                self.endpoint,
                self.server_name.clone(),
                identity,
                self.connect_timeout,
                roots,
            )
            .await?
        } else {
            HboneClient::connect_mtls(
                self.endpoint,
                self.server_name.clone(),
                identity,
                self.connect_timeout,
            )
            .await?
        };
        #[cfg(not(test))]
        let client = HboneClient::connect_mtls(
            self.endpoint,
            self.server_name.clone(),
            identity,
            self.connect_timeout,
        )
        .await?;
        let mut clients = self.clients.write().await;
        if clients
            .get(&uid)
            .is_some_and(|current| current.certificate_generation >= generation)
        {
            anyhow::bail!("session certificate generation is not newer for uid {uid}");
        }
        clients.insert(
            uid,
            RegisteredClient {
                certificate_generation: generation,
                client,
            },
        );
        let registry = self.clone();
        tokio::task::spawn_blocking(move || {
            monitor_disconnect(monitor);
            tokio::runtime::Handle::current().spawn(async move {
                registry.remove_generation(uid, generation).await;
            });
        });
        Ok(())
    }

    pub async fn client_for_uid(&self, uid: u32) -> Result<HboneClient> {
        self.clients
            .read()
            .await
            .get(&uid)
            .map(|registered| registered.client.clone())
            .with_context(|| format!("no registered user session for uid {uid}"))
    }

    pub async fn client_for_native(&self, stream: &TcpStream) -> Result<HboneClient> {
        self.client_for_uid(native_peer_uid(stream).await?).await
    }

    pub async fn remove(&self, uid: u32) {
        self.clients.write().await.remove(&uid);
    }

    async fn remove_generation(&self, uid: u32, generation: u64) {
        let mut clients = self.clients.write().await;
        if clients
            .get(&uid)
            .is_some_and(|registered| registered.certificate_generation == generation)
        {
            clients.remove(&uid);
        }
    }
}

pub struct SessionSocket {
    path: PathBuf,
    listener: UnixListener,
}

impl SessionSocket {
    pub fn bind(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            anyhow::bail!("session socket path must be absolute");
        }
        let parent = path.parent().context("session socket has no parent")?;
        fs::create_dir_all(parent).context("create session socket directory")?;
        if let Ok(metadata) = fs::symlink_metadata(path) {
            if !metadata.file_type().is_socket()
                || metadata.uid() != rustix::process::geteuid().as_raw()
            {
                anyhow::bail!("refusing to replace unowned session socket path");
            }
            fs::remove_file(path).context("remove stale session socket")?;
        }
        let listener = UnixListener::bind(path).context("bind user session socket")?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o666))
            .context("set user session socket permissions")?;
        Ok(Self {
            path: path.to_owned(),
            listener,
        })
    }

    pub fn listener(&self) -> &UnixListener {
        &self.listener
    }
}

impl Drop for SessionSocket {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub async fn serve_registrations(
    socket: Arc<SessionSocket>,
    registry: SessionRegistry,
    registration_timeout: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut registrations = JoinSet::new();
    loop {
        tokio::select! {
            _ = shutdown.wait_for(|stopping| *stopping) => break,
            Some(result) = registrations.join_next(), if !registrations.is_empty() => {
                match result {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(event = "session_registration_failed", reason = %error),
                    Err(error) => tracing::warn!(event = "session_registration_task_failed", reason = %error),
                }
            }
            accepted = accept(socket.listener()) => {
                let connection = accepted?;
                let registry = registry.clone();
                registrations.spawn(async move {
                    tokio::time::timeout(registration_timeout, registry.register(connection))
                        .await
                        .context("user session registration timed out")??;
                    Ok::<_, anyhow::Error>(())
                });
            }
        }
    }
    registrations.abort_all();
    Ok(())
}

pub async fn run_user_agent(
    path: &Path,
    identity: RotatingClientIdentity,
    reconnect_delay: Duration,
) -> Result<()> {
    loop {
        let result = async {
            let (generation, client_identity) = identity.pem_snapshot()?;
            let mut stream = UnixStream::connect(path)
                .await
                .context("connect to machine forwarder session socket")?;
            let certificates = rustls_pemfile::certs(&mut std::io::Cursor::new(
                &client_identity.certificate_chain_pem,
            ))
            .map(|certificate| {
                certificate
                    .map(|certificate| {
                        base64::engine::general_purpose::STANDARD.encode(certificate.as_ref())
                    })
                    .context("parse user certificate chain")
            })
            .collect::<Result<Vec<_>>>()?;
            write_frame(
                &mut stream,
                &Registration {
                    version: crate::session_protocol::VERSION,
                    certificate_generation: generation,
                    certificate_chain_der_base64: certificates,
                },
            )
            .await
            .context("register user certificate")?;
            let signing_key = P256SigningKey::from_pkcs8_pem(&client_identity.private_key_pem)
                .context("parse user signing key")?;
            serve_signing_requests(&mut stream, &identity, generation, &signing_key).await
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(event = "session_agent_disconnected", reason = %error);
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(reconnect_delay) => {}
        }
    }
}

async fn serve_signing_requests(
    stream: &mut UnixStream,
    identity: &RotatingClientIdentity,
    generation: u64,
    signing_key: &P256SigningKey,
) -> Result<()> {
    let mut generation_check = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = generation_check.tick() => {
                if identity.pem_snapshot()?.0 != generation {
                    return Ok(());
                }
            }
            request = read_frame::<ForwarderRequest, _>(&mut *stream) => {
                let request = request.context("read signing request")?;
                let ForwarderRequest::Sign { request_id, scheme, message_base64 } = request;
                let response = match scheme {
                    SignatureScheme::EcdsaP256Sha256 => {
                        let message = base64::engine::general_purpose::STANDARD
                            .decode(message_base64)
                            .context("decode signing input")?;
                        let signature: Signature = signing_key.sign(&message);
                        AgentResponse::Signature {
                            request_id,
                            signature_base64: base64::engine::general_purpose::STANDARD
                                .encode(signature.to_der().as_bytes()),
                        }
                    }
                };
                write_frame(&mut *stream, &response)
                    .await
                    .context("write signing response")?;
            }
        }
    }
}

impl SessionConnection {
    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn registration(&self) -> &Registration {
        &self.registration
    }

    pub fn into_stream(self) -> UnixStream {
        self.stream
    }

    pub fn into_signing_key(
        self,
        timeout: Duration,
    ) -> Result<(Arc<dyn SigningKey>, std::os::unix::net::UnixStream)> {
        let stream = self
            .stream
            .into_std()
            .context("detach user session socket")?;
        stream
            .set_nonblocking(false)
            .context("configure blocking user session socket")?;
        stream
            .set_read_timeout(Some(timeout))
            .context("configure user session read timeout")?;
        stream
            .set_write_timeout(Some(timeout))
            .context("configure user session write timeout")?;
        let monitor = stream
            .try_clone()
            .context("clone user session socket for lifecycle monitoring")?;
        Ok((
            Arc::new(SessionSigningKey {
                stream: Arc::new(Mutex::new(stream)),
                next_request_id: Arc::new(AtomicU64::new(1)),
            }),
            monitor,
        ))
    }
}

fn monitor_disconnect(stream: std::os::unix::net::UnixStream) {
    loop {
        let mut byte = [0_u8; 1];
        match rustix::net::recv(&stream, &mut byte, rustix::net::RecvFlags::PEEK) {
            Ok((_, 0)) | Err(_) => return,
            Ok(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

#[derive(Debug)]
struct SessionSigningKey {
    stream: Arc<Mutex<std::os::unix::net::UnixStream>>,
    next_request_id: Arc<AtomicU64>,
}

impl SigningKey for SessionSigningKey {
    fn choose_scheme(&self, offered: &[TlsSignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&TlsSignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| {
                Box::new(SessionSigner {
                    stream: Arc::clone(&self.stream),
                    request_id: self.next_request_id.fetch_add(1, Ordering::Relaxed),
                }) as Box<dyn Signer>
            })
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

#[derive(Debug)]
struct SessionSigner {
    stream: Arc<Mutex<std::os::unix::net::UnixStream>>,
    request_id: u64,
}

impl Signer for SessionSigner {
    fn sign(&self, message: &[u8]) -> std::result::Result<Vec<u8>, TlsError> {
        let request = ForwarderRequest::Sign {
            request_id: self.request_id,
            scheme: SignatureScheme::EcdsaP256Sha256,
            message_base64: base64::engine::general_purpose::STANDARD.encode(message),
        };
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| TlsError::General("user session signer lock poisoned".to_owned()))?;
        write_frame_blocking(&mut *stream, &request)
            .map_err(|error| TlsError::General(format!("send signing request: {error}")))?;
        match read_frame_blocking(&mut *stream)
            .map_err(|error| TlsError::General(format!("read signing response: {error}")))?
        {
            AgentResponse::Signature {
                request_id,
                signature_base64,
            } if request_id == self.request_id => base64::engine::general_purpose::STANDARD
                .decode(signature_base64)
                .map_err(|error| TlsError::General(format!("decode signing response: {error}"))),
            AgentResponse::Error { request_id, reason } if request_id == self.request_id => Err(
                TlsError::General(format!("user session refused signing: {reason}")),
            ),
            _ => Err(TlsError::General(
                "user session returned a mismatched signing response".to_owned(),
            )),
        }
    }

    fn scheme(&self) -> TlsSignatureScheme {
        TlsSignatureScheme::ECDSA_NISTP256_SHA256
    }
}

pub async fn accept(listener: &UnixListener) -> Result<SessionConnection> {
    let (mut stream, _) = listener.accept().await.context("accept user session")?;
    let credentials = rustix::net::sockopt::socket_peercred(stream.as_fd())
        .context("read user session peer credentials")?;
    let registration: Registration = read_frame(&mut stream)
        .await
        .context("read user session registration")?;
    registration
        .validate()
        .context("validate user session registration")?;
    Ok(SessionConnection {
        uid: credentials.uid.as_raw(),
        registration,
        stream,
    })
}

pub async fn native_peer_uid(stream: &TcpStream) -> Result<u32> {
    let server = stream
        .local_addr()
        .context("read native listener address")?;
    let client = stream.peer_addr().context("read native client address")?;
    if !server.ip().is_loopback() || !client.ip().is_loopback() {
        anyhow::bail!("native connection is not loopback");
    }
    tokio::task::spawn_blocking(move || query_uid(client, server))
        .await
        .context("join native user lookup")?
}

fn query_uid(client: SocketAddr, server: SocketAddr) -> Result<u32> {
    let family = match (client, server) {
        (SocketAddr::V4(_), SocketAddr::V4(_)) => AF_INET,
        (SocketAddr::V6(_), SocketAddr::V6(_)) => AF_INET6,
        _ => anyhow::bail!("native connection address families differ"),
    };
    let mut socket = Socket::new(NETLINK_SOCK_DIAG).context("open SOCK_DIAG socket")?;
    socket.bind_auto().context("bind SOCK_DIAG socket")?;
    socket
        .connect(&NetlinkSocketAddr::new(0, 0))
        .context("connect SOCK_DIAG socket")?;

    let mut header = NetlinkHeader::default();
    header.flags = NLM_F_REQUEST;
    header.sequence_number = 1;
    let socket_id = SocketId {
        source_port: client.port(),
        destination_port: server.port(),
        source_address: client.ip(),
        destination_address: server.ip(),
        interface_id: 0,
        cookie: [0xff; 8],
    };
    let mut request = NetlinkMessage::new(
        header,
        SockDiagMessage::InetRequest(InetRequest {
            family,
            protocol: IPPROTO_TCP,
            extensions: ExtensionFlags::empty(),
            states: StateFlags::ESTABLISHED,
            socket_id: socket_id.clone(),
        })
        .into(),
    );
    request.finalize();
    let mut outgoing = vec![0; request.buffer_len()];
    request.serialize(&mut outgoing);
    socket
        .send(&outgoing, 0)
        .context("send SOCK_DIAG request")?;

    let mut incoming = vec![0; 8192];
    let received = socket
        .recv(&mut &mut incoming[..], 0)
        .context("receive SOCK_DIAG response")?;
    let mut offset = 0;
    let mut uid = None;
    while offset < received {
        let response = NetlinkMessage::<SockDiagMessage>::deserialize(&incoming[offset..received])
            .context("decode SOCK_DIAG response")?;
        if response.header.sequence_number != 1 {
            anyhow::bail!("SOCK_DIAG response sequence does not match request");
        }
        match response.payload {
            NetlinkPayload::InnerMessage(SockDiagMessage::InetResponse(response)) => {
                let returned = response.header.socket_id;
                if returned.source_port != socket_id.source_port
                    || returned.destination_port != socket_id.destination_port
                    || returned.source_address != socket_id.source_address
                    || returned.destination_address != socket_id.destination_address
                {
                    anyhow::bail!("SOCK_DIAG returned a different connection");
                }
                if uid.replace(response.header.uid).is_some() {
                    anyhow::bail!("SOCK_DIAG returned multiple connection owners");
                }
            }
            NetlinkPayload::Done(_) | NetlinkPayload::Noop => {}
            payload => anyhow::bail!("SOCK_DIAG lookup failed: {payload:?}"),
        }
        let length = response.header.length as usize;
        if length == 0 {
            anyhow::bail!("SOCK_DIAG returned an empty message");
        }
        offset += (length + 3) & !3;
    }
    uid.context("SOCK_DIAG found no matching native connection")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::enrollment::ClientIdentity;
    use crate::session_protocol::{VERSION, write_frame};
    use base64::Engine;
    use bytes::Bytes;
    use http::{Method, Response};
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, CertifiedKey, IsCa, KeyPair,
        PKCS_ECDSA_P256_SHA256, generate_simple_self_signed,
    };
    use rustls::pki_types::PrivateKeyDer;
    use rustls::server::WebPkiClientVerifier;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_rustls::TlsAcceptor;

    #[tokio::test]
    async fn derives_uid_from_peer_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let CertifiedKey { cert, .. } =
            generate_simple_self_signed(["agentdesktop.test".to_owned()]).unwrap();
        let certificate = base64::engine::general_purpose::STANDARD.encode(cert.der());
        let client = tokio::spawn(async move {
            let mut stream = UnixStream::connect(path).await.unwrap();
            write_frame(
                &mut stream,
                &Registration {
                    version: VERSION,
                    certificate_generation: 4,
                    certificate_chain_der_base64: vec![certificate],
                },
            )
            .await
            .unwrap();
        });

        let connection = accept(&listener).await.unwrap();

        client.await.unwrap();
        assert_eq!(connection.uid(), rustix::process::geteuid().as_raw());
        assert_eq!(connection.registration().certificate_generation, 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signing_key_keeps_operation_on_user_session_channel() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let CertifiedKey { cert, .. } =
            generate_simple_self_signed(["agentdesktop.test".to_owned()]).unwrap();
        let certificate = base64::engine::general_purpose::STANDARD.encode(cert.der());
        let agent = tokio::spawn(async move {
            let mut stream = UnixStream::connect(path).await.unwrap();
            write_frame(
                &mut stream,
                &Registration {
                    version: VERSION,
                    certificate_generation: 1,
                    certificate_chain_der_base64: vec![certificate],
                },
            )
            .await
            .unwrap();
            let request: ForwarderRequest = read_frame(&mut stream).await.unwrap();
            let ForwarderRequest::Sign {
                request_id,
                message_base64,
                ..
            } = request;
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(message_base64)
                    .unwrap(),
                b"certificate verify"
            );
            write_frame(
                &mut stream,
                &AgentResponse::Signature {
                    request_id,
                    signature_base64: base64::engine::general_purpose::STANDARD
                        .encode(b"DER signature"),
                },
            )
            .await
            .unwrap();
        });
        let connection = accept(&listener).await.unwrap();
        let (signing_key, _monitor) = connection.into_signing_key(Duration::from_secs(1)).unwrap();

        let signature = tokio::task::spawn_blocking(move || {
            signing_key
                .choose_scheme(&[TlsSignatureScheme::ECDSA_NISTP256_SHA256])
                .unwrap()
                .sign(b"certificate verify")
                .unwrap()
        })
        .await
        .unwrap();

        agent.await.unwrap();
        assert_eq!(signature, b"DER signature");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registry_evicts_disconnected_session() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let CertifiedKey { cert, .. } =
            generate_simple_self_signed(["agentdesktop.test".to_owned()]).unwrap();
        let certificate = base64::engine::general_purpose::STANDARD.encode(cert.der());
        let (release, released) = oneshot::channel();
        let agent = tokio::spawn(async move {
            let mut stream = UnixStream::connect(path).await.unwrap();
            write_frame(
                &mut stream,
                &Registration {
                    version: VERSION,
                    certificate_generation: 1,
                    certificate_chain_der_base64: vec![certificate],
                },
            )
            .await
            .unwrap();
            let _ = released.await;
        });
        let connection = accept(&listener).await.unwrap();
        let uid = connection.uid();
        let registry = SessionRegistry::new(
            "127.0.0.1:9".parse().unwrap(),
            "gateway.test".to_owned(),
            Duration::from_secs(1),
        );
        registry.register(connection).await.unwrap();
        assert!(registry.client_for_uid(uid).await.is_ok());

        release.send(()).unwrap();
        agent.await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while registry.client_for_uid(uid).await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registry_client_authenticates_with_user_session_signer() {
        let ca_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let mut ca_params = CertificateParams::default();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca = CertifiedIssuer::self_signed(ca_params, ca_key).unwrap();

        let server_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let server_cert = CertificateParams::new(vec!["gateway.test".to_owned()])
            .unwrap()
            .signed_by(&server_key, &ca)
            .unwrap();
        let client_key = KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
        let client_cert = CertificateParams::new(vec!["user.test".to_owned()])
            .unwrap()
            .signed_by(&client_key, &ca)
            .unwrap();

        let mut client_roots = rustls::RootCertStore::empty();
        client_roots.add(ca.der().clone()).unwrap();
        let verifier = WebPkiClientVerifier::builder(Arc::new(client_roots))
            .build()
            .unwrap();
        let server_key_der = PrivateKeyDer::try_from(server_key.serialize_der()).unwrap();
        let mut server_config = rustls::ServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![server_cert.der().clone()], server_key_der)
            .unwrap();
        server_config.alpn_protocols = vec![b"h2".to_vec()];

        let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_address = gateway.local_addr().unwrap();
        let gateway_task = tokio::spawn(async move {
            let (stream, _) = gateway.accept().await.unwrap();
            let tls = TlsAcceptor::from(Arc::new(server_config))
                .accept(stream)
                .await
                .unwrap();
            assert_eq!(tls.get_ref().1.peer_certificates().unwrap().len(), 1);
            let mut connection = h2::server::handshake(tls).await.unwrap();
            let (request, mut respond) = connection.accept().await.unwrap().unwrap();
            let mut exchange = tokio::spawn(async move {
                assert_eq!(request.method(), Method::CONNECT);
                assert_eq!(request.uri().authority().unwrap(), "provider.test:443");
                let mut receive = request.into_body();
                let mut send = respond.send_response(Response::new(()), false).unwrap();
                let bytes = receive.data().await.unwrap().unwrap();
                receive
                    .flow_control()
                    .release_capacity(bytes.len())
                    .unwrap();
                assert_eq!(bytes, Bytes::from_static(b"request"));
                send.send_data(Bytes::from_static(b"response"), true)
                    .unwrap();
            });
            loop {
                tokio::select! {
                    result = &mut exchange => {
                        result.unwrap();
                        break;
                    }
                    accepted = connection.accept() => {
                        assert!(accepted.is_some(), "H2 connection closed before tunnel exchange");
                    }
                }
            }
        });

        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("sessions.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        let identity = RotatingClientIdentity::new(ClientIdentity {
            certificate_chain_pem: client_cert.pem(),
            private_key_pem: client_key.serialize_pem(),
        });
        let agent = tokio::spawn(async move {
            let mut stream = UnixStream::connect(socket_path).await.unwrap();
            let (generation, client_identity) = identity.pem_snapshot().unwrap();
            write_frame(
                &mut stream,
                &Registration {
                    version: VERSION,
                    certificate_generation: generation,
                    certificate_chain_der_base64: vec![
                        base64::engine::general_purpose::STANDARD.encode(client_cert.der()),
                    ],
                },
            )
            .await
            .unwrap();
            let signing_key =
                P256SigningKey::from_pkcs8_pem(&client_identity.private_key_pem).unwrap();
            serve_signing_requests(&mut stream, &identity, generation, &signing_key)
                .await
                .unwrap();
        });

        let connection = accept(&listener).await.unwrap();
        let uid = connection.uid();
        let mut server_roots = rustls::RootCertStore::empty();
        server_roots.add(ca.der().clone()).unwrap();
        let registry = SessionRegistry::new(
            gateway_address,
            "gateway.test".to_owned(),
            Duration::from_secs(2),
        )
        .with_roots(server_roots);
        registry.register(connection).await.unwrap();
        let mut tunnel = registry
            .client_for_uid(uid)
            .await
            .unwrap()
            .open_tunnel("provider.test:443".parse().unwrap())
            .await
            .unwrap();
        tunnel.write_all(b"request").await.unwrap();
        tunnel.shutdown().await.unwrap();
        let mut response = Vec::new();
        tunnel.read_to_end(&mut response).await.unwrap();

        assert_eq!(response, b"response");
        gateway_task.await.unwrap();
        agent.abort();
    }

    async fn assert_native_loopback_peer_uid(address: &str) {
        let listener = TcpListener::bind(address).await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();

        let uid = native_peer_uid(&server).await.unwrap();

        assert_eq!(uid, rustix::process::geteuid().as_raw());
        drop(client);
    }

    #[tokio::test]
    async fn resolves_ipv4_native_loopback_peer_uid() {
        assert_native_loopback_peer_uid("127.0.0.1:0").await;
    }

    #[tokio::test]
    async fn resolves_ipv6_native_loopback_peer_uid_when_available() {
        if TcpListener::bind("[::1]:0").await.is_ok() {
            assert_native_loopback_peer_uid("[::1]:0").await;
        }
    }
}
