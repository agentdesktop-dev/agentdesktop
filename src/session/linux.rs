use std::net::SocketAddr;
use std::os::fd::AsFd;
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
use rustls::sign::{Signer, SigningKey};
use rustls::{Error as TlsError, SignatureAlgorithm, SignatureScheme as TlsSignatureScheme};
use tokio::net::{TcpStream, UnixListener, UnixStream};

use crate::session_protocol::{
    AgentResponse, ForwarderRequest, Registration, SignatureScheme, read_frame,
    read_frame_blocking, write_frame_blocking,
};

pub struct SessionConnection {
    uid: u32,
    registration: Registration,
    stream: UnixStream,
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

    pub fn into_signing_key(self, timeout: Duration) -> Result<Arc<dyn SigningKey>> {
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
        Ok(Arc::new(SessionSigningKey {
            stream: Arc::new(Mutex::new(stream)),
            next_request_id: Arc::new(AtomicU64::new(1)),
        }))
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
    use crate::session_protocol::{VERSION, write_frame};
    use base64::Engine;
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use tokio::net::TcpListener;

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
        let signing_key = connection.into_signing_key(Duration::from_secs(1)).unwrap();

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
