use std::fs;
use std::net::SocketAddr;
use std::os::fd::AsFd;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use netlink_packet_core::{NLM_F_REQUEST, NetlinkHeader, NetlinkMessage, NetlinkPayload};
use netlink_packet_sock_diag::{
    SockDiagMessage,
    constants::{AF_INET, AF_INET6, IPPROTO_TCP},
    inet::{ExtensionFlags, InetRequest, SocketId, StateFlags},
};
use netlink_sys::{Socket, SocketAddr as NetlinkSocketAddr, protocols::NETLINK_SOCK_DIAG};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::config::DeploymentMode;
use crate::service::hbone::{HboneClient, TlsRoots};
use crate::session::registry::{ClientRegistry, RegistrationVersion};
use crate::session_protocol::{Registration, RegistrationIdentity, read_frame};

mod managed;
mod self_managed;

pub use managed::run_user_agent;
pub use self_managed::run_local_user_agent;

/// An authenticated user-agent registration and its still-open IPC lease.
///
/// Registration consumes this value into exactly one socket owner. In
/// remote-managed mode the socket carries private-key signing requests. In
/// self-managed local mode it is only a lease proving that the user agent still
/// owns the registered local Agent Gateway.
pub struct SessionConnection {
    uid: u32,
    registration: Registration,
    stream: UnixStream,
}

#[derive(Clone)]
pub struct SessionRegistry {
    mode: DeploymentMode,
    endpoint: SocketAddr,
    server_name: String,
    connect_timeout: Duration,
    roots: TlsRoots,
    clients: ClientRegistry<u32>,
}

impl SessionRegistry {
    pub fn new(
        mode: DeploymentMode,
        endpoint: SocketAddr,
        server_name: String,
        connect_timeout: Duration,
        roots: TlsRoots,
    ) -> Self {
        Self {
            mode,
            endpoint,
            server_name,
            connect_timeout,
            roots,
            clients: ClientRegistry::default(),
        }
    }

    pub async fn register(&self, connection: SessionConnection) -> Result<()> {
        let uid = connection.uid();
        let generation = connection.registration().certificate_generation;
        let identity = connection.registration().identity.clone();
        let (client, monitor, version) = match (self.mode, identity) {
            (DeploymentMode::Standalone, RegistrationIdentity::SelfManaged(registration)) => {
                let (client, monitor) =
                    self_managed::register(self, connection, registration).await?;
                (client, monitor, RegistrationVersion::SelfManaged)
            }
            (DeploymentMode::Managed, RegistrationIdentity::Managed(registration)) => {
                let (client, monitor) =
                    managed::register(self, connection, generation, registration).await?;
                (client, monitor, RegistrationVersion::Managed(generation))
            }
            (DeploymentMode::Standalone, RegistrationIdentity::Managed(_)) => {
                anyhow::bail!("managed identity cannot register with a standalone forwarder")
            }
            (DeploymentMode::Managed, RegistrationIdentity::SelfManaged(_)) => {
                anyhow::bail!("self-managed identity cannot register with a managed forwarder")
            }
        };
        self.clients.install(uid, version, client, monitor).await
    }

    pub async fn client_for_uid(&self, uid: u32) -> Result<HboneClient> {
        self.clients
            .client(&uid)
            .await
            .with_context(|| format!("no registered user session for uid {uid}"))
    }

    pub async fn client_for_native(&self, stream: &TcpStream) -> Result<HboneClient> {
        self.client_for_uid(native_peer_uid(stream).await?).await
    }

    pub async fn client_for_capture(
        &self,
        stream: &TcpStream,
        original_destination: SocketAddr,
    ) -> Result<HboneClient> {
        self.client_for_uid(captured_peer_uid(stream, original_destination).await?)
            .await
    }

    pub async fn remove(&self, uid: u32) {
        self.clients.remove(&uid).await;
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

impl SessionConnection {
    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn registration(&self) -> &Registration {
        &self.registration
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

pub async fn captured_peer_uid(
    stream: &TcpStream,
    original_destination: SocketAddr,
) -> Result<u32> {
    let client = stream.peer_addr().context("read captured client address")?;
    tokio::task::spawn_blocking(move || query_uid(client, original_destination))
        .await
        .context("join captured user lookup")?
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
    use crate::service::hbone::RotatingClientIdentity;
    use crate::session::managed::serve_signing_requests;
    use crate::session_protocol::{AgentResponse, ForwarderRequest, write_frame};
    use base64::Engine;
    use bytes::Bytes;
    use http::{Method, Response};
    use p256::ecdsa::SigningKey as P256SigningKey;
    use p256::pkcs8::DecodePrivateKey;
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, CertifiedKey, IsCa, KeyPair,
        PKCS_ECDSA_P256_SHA256, generate_simple_self_signed,
    };
    use rustls::SignatureScheme as TlsSignatureScheme;
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
            write_frame(&mut stream, &Registration::managed(4, vec![certificate]))
                .await
                .unwrap();
        });

        let connection = accept(&listener).await.unwrap();

        client.await.unwrap();
        assert_eq!(connection.uid(), rustix::process::geteuid().as_raw());
        assert_eq!(connection.registration().certificate_generation, 4);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn signing_key_keeps_operation_on_user_session_channel() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let CertifiedKey { cert, .. } =
            generate_simple_self_signed(["agentdesktop.test".to_owned()]).unwrap();
        let certificate = base64::engine::general_purpose::STANDARD.encode(cert.der());
        let agent = tokio::spawn(async move {
            let mut stream = UnixStream::connect(path).await.unwrap();
            write_frame(&mut stream, &Registration::managed(1, vec![certificate]))
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

        let signature = signing_key
            .choose_scheme(&[TlsSignatureScheme::ECDSA_NISTP256_SHA256])
            .unwrap()
            .sign(b"certificate verify")
            .unwrap();

        agent.await.unwrap();
        assert_eq!(signature, b"DER signature");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signing_key_times_out_through_async_worker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let CertifiedKey { cert, .. } =
            generate_simple_self_signed(["agentdesktop.test".to_owned()]).unwrap();
        let certificate = base64::engine::general_purpose::STANDARD.encode(cert.der());
        let agent = tokio::spawn(async move {
            let mut stream = UnixStream::connect(path).await.unwrap();
            write_frame(&mut stream, &Registration::managed(1, vec![certificate]))
                .await
                .unwrap();
            let _: ForwarderRequest = read_frame(&mut stream).await.unwrap();
            std::future::pending::<()>().await;
        });
        let connection = accept(&listener).await.unwrap();
        let (signing_key, worker) = connection
            .into_signing_key(Duration::from_millis(50))
            .unwrap();

        let error = tokio::task::spawn_blocking(move || {
            signing_key
                .choose_scheme(&[TlsSignatureScheme::ECDSA_NISTP256_SHA256])
                .unwrap()
                .sign(b"certificate verify")
                .unwrap_err()
        })
        .await
        .unwrap();

        assert!(error.to_string().contains("user session signing timed out"));
        assert!(worker.await.unwrap().is_err());
        agent.abort();
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
            write_frame(&mut stream, &Registration::managed(1, vec![certificate]))
                .await
                .unwrap();
            let _ = released.await;
        });
        let connection = accept(&listener).await.unwrap();
        let uid = connection.uid();
        let registry = SessionRegistry::new(
            DeploymentMode::Managed,
            "127.0.0.1:9".parse().unwrap(),
            "gateway.test".to_owned(),
            Duration::from_secs(1),
            TlsRoots::Native,
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

    #[tokio::test]
    async fn registry_rejects_registration_from_other_deployment_mode() {
        let (stream, _) = UnixStream::pair().unwrap();
        let managed_registry = SessionRegistry::new(
            DeploymentMode::Managed,
            "127.0.0.1:9".parse().unwrap(),
            "gateway.test".to_owned(),
            Duration::from_secs(1),
            TlsRoots::Native,
        );
        let self_managed = SessionConnection {
            uid: 1000,
            registration: Registration::self_managed(
                1,
                crate::session_protocol::self_managed::Registration {
                    endpoint: "127.0.0.1:15008".parse().unwrap(),
                    tunnel_token: "local-secret".to_owned(),
                },
            ),
            stream,
        };
        assert!(managed_registry.register(self_managed).await.is_err());

        let (stream, _) = UnixStream::pair().unwrap();
        let standalone_registry = SessionRegistry::new(
            DeploymentMode::Standalone,
            "127.0.0.1:9".parse().unwrap(),
            "gateway.test".to_owned(),
            Duration::from_secs(1),
            TlsRoots::Native,
        );
        let managed = SessionConnection {
            uid: 1000,
            registration: Registration::managed(1, Vec::new()),
            stream,
        };
        assert!(standalone_registry.register(managed).await.is_err());
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
        let (response_read, mut response_was_read) = oneshot::channel();
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
            loop {
                tokio::select! {
                    result = &mut response_was_read => {
                        result.unwrap();
                        break;
                    }
                    accepted = connection.accept() => {
                        assert!(accepted.is_some(), "H2 connection closed before response delivery");
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
                &Registration::managed(
                    generation,
                    vec![base64::engine::general_purpose::STANDARD.encode(client_cert.der())],
                ),
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
            DeploymentMode::Managed,
            gateway_address,
            "gateway.test".to_owned(),
            Duration::from_secs(2),
            TlsRoots::Custom(server_roots),
        );
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
        let mut response = [0_u8; 8];
        tunnel.read_exact(&mut response).await.unwrap();

        assert_eq!(&response, b"response");
        response_read.send(()).unwrap();
        gateway_task.await.unwrap();
        agent.abort();
    }

    #[tokio::test]
    async fn registry_routes_to_registered_local_gateway_with_capability() {
        let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway_address = gateway.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = gateway.accept().await.unwrap();
            let mut connection = h2::server::handshake(stream).await.unwrap();
            let (request, mut respond) = connection.accept().await.unwrap().unwrap();
            assert_eq!(
                request.headers()[crate::local_gateway::TUNNEL_TOKEN_HEADER],
                "local-secret"
            );
            respond.send_response(Response::new(()), true).unwrap();
            let _ = tokio::time::timeout(Duration::from_millis(20), connection.accept()).await;
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sessions.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let agent = tokio::spawn(async move {
            let mut stream = UnixStream::connect(path).await.unwrap();
            write_frame(
                &mut stream,
                &Registration::self_managed(
                    1,
                    crate::session_protocol::self_managed::Registration {
                        endpoint: gateway_address,
                        tunnel_token: "local-secret".to_owned(),
                    },
                ),
            )
            .await
            .unwrap();
            std::future::pending::<()>().await;
        });
        let connection = accept(&listener).await.unwrap();
        let uid = connection.uid();
        let registry = SessionRegistry::new(
            DeploymentMode::Standalone,
            "127.0.0.1:9".parse().unwrap(),
            "unused.invalid".to_owned(),
            Duration::from_secs(1),
            TlsRoots::Native,
        );
        registry.register(connection).await.unwrap();

        registry
            .client_for_uid(uid)
            .await
            .unwrap()
            .open_tunnel("provider.test:443".parse().unwrap())
            .await
            .unwrap();

        server.await.unwrap();
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
    async fn resolves_captured_peer_uid_from_original_tuple() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination = listener.local_addr().unwrap();
        let client = TcpStream::connect(destination).await.unwrap();
        let (accepted, _) = listener.accept().await.unwrap();

        assert_eq!(
            captured_peer_uid(&accepted, destination).await.unwrap(),
            rustix::process::geteuid().as_raw()
        );
        drop(client);
    }

    #[tokio::test]
    async fn resolves_ipv6_native_loopback_peer_uid_when_available() {
        if TcpListener::bind("[::1]:0").await.is_ok() {
            assert_native_loopback_peer_uid("[::1]:0").await;
        }
    }
}
