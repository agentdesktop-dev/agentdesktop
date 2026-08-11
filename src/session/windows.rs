use std::collections::HashMap;
use std::ffi::c_void;
use std::net::SocketAddr;
use std::os::windows::io::AsRawHandle;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use rustls::pki_types::CertificateDer;
use rustls::sign::{Signer, SigningKey};
use rustls::{Error as TlsError, SignatureAlgorithm, SignatureScheme as TlsSignatureScheme};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::RwLock;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Security::{
    CopySid, GetLengthSid, GetTokenInformation, RevertToSelf, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

use crate::service::hbone::{ExternalClientIdentity, HboneClient, RotatingClientIdentity};
use crate::session_protocol::{
    AgentResponse, ForwarderRequest, Registration, SignatureScheme, read_frame, write_frame,
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct UserSid(Vec<u8>);

pub struct SessionConnection {
    sid: UserSid,
    registration: Registration,
    pipe: NamedPipeServer,
}

#[derive(Clone)]
pub struct SessionRegistry {
    endpoint: SocketAddr,
    server_name: String,
    connect_timeout: Duration,
    clients: Arc<RwLock<HashMap<UserSid, RegisteredClient>>>,
}

#[derive(Clone)]
struct RegisteredClient {
    certificate_generation: u64,
    client: HboneClient,
}

pub fn create_server(path: &str) -> Result<NamedPipeServer> {
    ServerOptions::new()
        .reject_remote_clients(true)
        .create(path)
        .context("create machine session named pipe")
}

pub async fn accept(mut pipe: NamedPipeServer) -> Result<SessionConnection> {
    pipe.connect().await.context("connect user session pipe")?;
    let registration: Registration = read_frame(&mut pipe)
        .await
        .context("read user session registration")?;
    registration
        .validate()
        .context("validate user session registration")?;
    let handle = pipe.as_raw_handle() as usize;
    let sid = tokio::task::spawn_blocking(move || user_sid_from_pipe(handle as HANDLE))
        .await
        .context("join named-pipe user authentication")??;
    Ok(SessionConnection {
        sid,
        registration,
        pipe,
    })
}

impl SessionRegistry {
    pub fn new(endpoint: SocketAddr, server_name: String, connect_timeout: Duration) -> Self {
        Self {
            endpoint,
            server_name,
            connect_timeout,
            clients: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn register(&self, connection: SessionConnection) -> Result<()> {
        if connection.registration().local_gateway.is_some() {
            anyhow::bail!("self-managed Windows session registration is not implemented");
        }
        let sid = connection.sid().clone();
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
        let (signing_key, worker) = connection.into_signing_key(self.connect_timeout);
        let identity = RotatingClientIdentity::new_external(
            ExternalClientIdentity {
                certificates,
                signing_key,
            },
            generation,
        );
        let client = HboneClient::connect_mtls(
            self.endpoint,
            self.server_name.clone(),
            identity,
            self.connect_timeout,
        )
        .await?;
        let mut clients = self.clients.write().await;
        if clients
            .get(&sid)
            .is_some_and(|current| current.certificate_generation >= generation)
        {
            anyhow::bail!("session certificate generation is not newer for Windows user");
        }
        clients.insert(
            sid.clone(),
            RegisteredClient {
                certificate_generation: generation,
                client,
            },
        );
        drop(clients);
        let registry = self.clone();
        tokio::spawn(async move {
            let _ = worker.await;
            registry.remove_generation(&sid, generation).await;
        });
        Ok(())
    }

    pub async fn client_for_sid(&self, sid: &UserSid) -> Result<HboneClient> {
        self.clients
            .read()
            .await
            .get(sid)
            .map(|registered| registered.client.clone())
            .context("no registered user session for Windows user")
    }

    async fn remove_generation(&self, sid: &UserSid, generation: u64) {
        let mut clients = self.clients.write().await;
        if clients
            .get(sid)
            .is_some_and(|registered| registered.certificate_generation == generation)
        {
            clients.remove(sid);
        }
    }
}

impl SessionConnection {
    pub fn sid(&self) -> &UserSid {
        &self.sid
    }

    pub fn registration(&self) -> &Registration {
        &self.registration
    }

    pub fn into_pipe(self) -> NamedPipeServer {
        self.pipe
    }

    pub fn into_signing_key(
        self,
        timeout: Duration,
    ) -> (Arc<dyn SigningKey>, tokio::task::JoinHandle<Result<()>>) {
        let (requests, receiver) = unbounded_channel();
        let worker = tokio::spawn(run_signing_worker(self.pipe, receiver, timeout));
        (
            Arc::new(SessionSigningKey {
                requests,
                next_request_id: AtomicU64::new(1),
                timeout,
            }),
            worker,
        )
    }
}

struct SigningWork {
    request: ForwarderRequest,
    response: Sender<Result<AgentResponse, String>>,
}

async fn run_signing_worker(
    mut pipe: NamedPipeServer,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<SigningWork>,
    timeout: Duration,
) -> Result<()> {
    loop {
        let work = tokio::select! {
            work = requests.recv() => match work {
                Some(work) => work,
                None => return Ok(()),
            },
            incoming = read_frame::<AgentResponse, _>(&mut pipe) => {
                incoming.context("monitor user session pipe")?;
                anyhow::bail!("user session sent an unsolicited signing response");
            }
        };
        let response = tokio::time::timeout(timeout, async {
            write_frame(&mut pipe, &work.request)
                .await
                .context("send signing request")?;
            read_frame(&mut pipe).await.context("read signing response")
        })
        .await
        .context("user session signing timed out")??;
        let _ = work.response.send(Ok(response));
    }
}

#[derive(Debug)]
struct SessionSigningKey {
    requests: UnboundedSender<SigningWork>,
    next_request_id: AtomicU64,
    timeout: Duration,
}

impl SigningKey for SessionSigningKey {
    fn choose_scheme(&self, offered: &[TlsSignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&TlsSignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| {
                Box::new(SessionSigner {
                    requests: self.requests.clone(),
                    request_id: self.next_request_id.fetch_add(1, Ordering::Relaxed),
                    timeout: self.timeout,
                }) as Box<dyn Signer>
            })
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

#[derive(Debug)]
struct SessionSigner {
    requests: UnboundedSender<SigningWork>,
    request_id: u64,
    timeout: Duration,
}

impl Signer for SessionSigner {
    fn sign(&self, message: &[u8]) -> std::result::Result<Vec<u8>, TlsError> {
        let (response, receiver) = channel();
        self.requests
            .send(SigningWork {
                request: ForwarderRequest::Sign {
                    request_id: self.request_id,
                    scheme: SignatureScheme::EcdsaP256Sha256,
                    message_base64: base64::engine::general_purpose::STANDARD.encode(message),
                },
                response,
            })
            .map_err(|_| TlsError::General("user session signer is unavailable".to_owned()))?;
        let response = receiver
            .recv_timeout(self.timeout)
            .map_err(|error| TlsError::General(format!("wait for user session signer: {error}")))?
            .map_err(TlsError::General)?;
        match response {
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

fn user_sid_from_pipe(pipe: HANDLE) -> Result<UserSid> {
    // Impersonation is confined to this blocking thread and always reverted.
    unsafe {
        if ImpersonateNamedPipeClient(pipe) == 0 {
            return Err(std::io::Error::last_os_error()).context("impersonate named-pipe client");
        }
    }
    let result = user_sid_from_thread_token();
    let reverted = unsafe { RevertToSelf() };
    if reverted == 0 {
        std::process::abort();
    }
    result
}

fn user_sid_from_thread_token() -> Result<UserSid> {
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, 0, &mut token) } == 0 {
        return Err(std::io::Error::last_os_error()).context("open impersonation token");
    }
    let token = OwnedHandle(token);
    let mut required = 0_u32;
    unsafe {
        GetTokenInformation(token.0, TokenUser, std::ptr::null_mut(), 0, &mut required);
    }
    if required < size_of::<TOKEN_USER>() as u32 {
        anyhow::bail!("impersonation token returned no user SID");
    }
    let words = (required as usize).div_ceil(size_of::<usize>());
    let mut buffer = vec![0_usize; words];
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast::<c_void>(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("read impersonated user SID");
    }
    let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    let sid_length = unsafe { GetLengthSid(token_user.User.Sid) };
    if sid_length == 0 {
        anyhow::bail!("impersonation token user SID is invalid");
    }
    let mut sid = vec![0_u8; sid_length as usize];
    if unsafe { CopySid(sid_length, sid.as_mut_ptr().cast(), token_user.User.Sid) } == 0 {
        return Err(std::io::Error::last_os_error()).context("copy impersonated user SID");
    }
    Ok(UserSid(sid))
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use tokio::net::windows::named_pipe::ClientOptions;

    fn pipe_name(label: &str) -> String {
        format!(r"\\.\pipe\agentdesktop-test-{}-{label}", std::process::id())
    }

    fn registration() -> Registration {
        let CertifiedKey { cert, .. } =
            generate_simple_self_signed(["agentdesktop.test".to_owned()]).unwrap();
        Registration {
            version: crate::session_protocol::VERSION,
            certificate_generation: 1,
            certificate_chain_der_base64: vec![
                base64::engine::general_purpose::STANDARD.encode(cert.der()),
            ],
            local_gateway: None,
        }
    }

    #[tokio::test]
    async fn derives_user_sid_from_named_pipe_client() {
        let name = pipe_name("sid");
        let server = create_server(&name).unwrap();
        let accepted = tokio::spawn(accept(server));
        let mut client = ClientOptions::new().open(&name).unwrap();
        write_frame(&mut client, &registration()).await.unwrap();

        let connection = accepted.await.unwrap().unwrap();

        assert!(!connection.sid().0.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn signing_key_uses_named_pipe_channel() {
        let name = pipe_name("signer");
        let server = create_server(&name).unwrap();
        let accepted = tokio::spawn(accept(server));
        let mut client = ClientOptions::new().open(&name).unwrap();
        write_frame(&mut client, &registration()).await.unwrap();
        let connection = accepted.await.unwrap().unwrap();
        let (key, worker) = connection.into_signing_key(Duration::from_secs(1));
        let client_task = tokio::spawn(async move {
            let request: ForwarderRequest = read_frame(&mut client).await.unwrap();
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
                &mut client,
                &AgentResponse::Signature {
                    request_id,
                    signature_base64: base64::engine::general_purpose::STANDARD
                        .encode(b"DER signature"),
                },
            )
            .await
            .unwrap();
        });
        let signature = tokio::task::spawn_blocking(move || {
            key.choose_scheme(&[TlsSignatureScheme::ECDSA_NISTP256_SHA256])
                .unwrap()
                .sign(b"certificate verify")
                .unwrap()
        })
        .await
        .unwrap();

        assert_eq!(signature, b"DER signature");
        client_task.await.unwrap();
        worker.abort();
    }
}
