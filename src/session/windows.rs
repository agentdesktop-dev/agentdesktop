use std::collections::HashMap;
use std::ffi::c_void;
use std::net::SocketAddr;
use std::os::windows::io::{
    AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle as StdOwnedHandle,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey as P256SigningKey};
use p256::pkcs8::DecodePrivateKey;
use rustls::pki_types::CertificateDer;
use rustls::sign::{Signer, SigningKey};
use rustls::{Error as TlsError, SignatureAlgorithm, SignatureScheme as TlsSignatureScheme};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::sync::RwLock;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio::sync::watch;
use tokio::task::JoinSet;
use windows_sys::Win32::Foundation::{
    CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    CopySid, GetLengthSid, GetTokenInformation, PSECURITY_DESCRIPTOR, RevertToSelf,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentThread, OpenThreadToken};

use crate::service::hbone::{ExternalClientIdentity, HboneClient, RotatingClientIdentity};
use crate::session_protocol::{
    AgentResponse, ForwarderRequest, Registration, SignatureScheme, read_frame, write_frame,
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct UserSid(Vec<u8>);

impl UserSid {
    pub(crate) fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let expected_len = bytes
            .get(1)
            .map(|sub_authorities| 8 + usize::from(*sub_authorities) * size_of::<u32>());
        if bytes.first() != Some(&1)
            || expected_len != Some(bytes.len())
            || unsafe { windows_sys::Win32::Security::IsValidSid(bytes.as_ptr().cast_mut().cast()) }
                == 0
        {
            anyhow::bail!("WFP redirect context contains an invalid user SID");
        }
        Ok(Self(bytes))
    }
}

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
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let sddl = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("build machine session pipe ACL");
    }
    let descriptor = LocalAllocation(descriptor);
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    unsafe {
        ServerOptions::new()
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                path,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
    }
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

pub async fn run_user_agent(
    path: &str,
    identity: RotatingClientIdentity,
    reconnect_delay: Duration,
) -> Result<()> {
    loop {
        let result = run_user_agent_session(path, &identity).await;
        if let Err(error) = result {
            tracing::warn!(event = "session_agent_disconnected", reason = %error);
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(reconnect_delay) => {}
        }
    }
}

async fn run_user_agent_session(path: &str, identity: &RotatingClientIdentity) -> Result<()> {
    let mut pipe = ClientOptions::new()
        .open(path)
        .context("connect to machine forwarder session pipe")?;
    let (generation, client_identity) = identity.pem_snapshot()?;
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
        &mut pipe,
        &Registration {
            version: crate::session_protocol::VERSION,
            certificate_generation: generation,
            certificate_chain_der_base64: certificates,
            local_gateway: None,
        },
    )
    .await
    .context("register user certificate")?;
    let signing_key = P256SigningKey::from_pkcs8_pem(&client_identity.private_key_pem)
        .context("parse user signing key")?;
    serve_signing_requests(&mut pipe, identity, generation, &signing_key).await
}

async fn serve_signing_requests(
    pipe: &mut NamedPipeClient,
    identity: &RotatingClientIdentity,
    generation: u64,
    signing_key: &P256SigningKey,
) -> Result<()> {
    let mut current_generation = identity.subscribe_generation();
    loop {
        tokio::select! {
            changed = current_generation.changed() => {
                changed.context("managed identity generation closed")?;
                if *current_generation.borrow() != generation {
                    return Ok(());
                }
            }
            request = read_frame::<ForwarderRequest, _>(&mut *pipe) => {
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
                write_frame(&mut *pipe, &response)
                    .await
                    .context("write signing response")?;
            }
        }
    }
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
        let (signing_key, worker) = connection.into_signing_key(self.connect_timeout)?;
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

pub async fn serve_registrations(
    path: String,
    registry: SessionRegistry,
    registration_timeout: Duration,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let mut registrations = JoinSet::new();
    'serving: loop {
        let accepted = accept(create_server(&path)?);
        tokio::pin!(accepted);
        let connection = loop {
            tokio::select! {
                _ = shutdown.wait_for(|stopping| *stopping) => break 'serving,
                Some(result) = registrations.join_next(), if !registrations.is_empty() => {
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::warn!(event = "session_registration_failed", reason = %error),
                        Err(error) => tracing::warn!(event = "session_registration_task_failed", reason = %error),
                    }
                }
                accepted = &mut accepted => break accepted?,
            }
        };
        let registry = registry.clone();
        registrations.spawn(async move {
            tokio::time::timeout(registration_timeout, registry.register(connection))
                .await
                .context("user session registration timed out")??;
            Ok::<_, anyhow::Error>(())
        });
    }
    registrations.abort_all();
    Ok(())
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

    pub fn into_signing_key(self, timeout: Duration) -> Result<SigningKeyWorker> {
        let (requests, receiver) = unbounded_channel();
        let pipe = detach_pipe(self.pipe)?;
        let worker = super::spawn_runtime_worker("agentdesktop-session-signer", move || {
            let pipe = unsafe { NamedPipeServer::from_raw_handle(pipe.into_raw_handle()) }
                .context("attach user session pipe to signing runtime")?;
            Ok(run_signing_worker(pipe, receiver, timeout))
        })?;
        Ok((
            Arc::new(SessionSigningKey {
                requests,
                next_request_id: AtomicU64::new(1),
                timeout,
            }),
            worker,
        ))
    }
}

type SigningKeyWorker = (Arc<dyn SigningKey>, tokio::task::JoinHandle<Result<()>>);

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

fn detach_pipe(pipe: NamedPipeServer) -> Result<StdOwnedHandle> {
    let process = unsafe { GetCurrentProcess() };
    let mut duplicate: HANDLE = std::ptr::null_mut();
    if unsafe {
        DuplicateHandle(
            process,
            pipe.as_raw_handle() as HANDLE,
            process,
            &mut duplicate,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("duplicate user session pipe");
    }
    drop(pipe);
    Ok(unsafe { StdOwnedHandle::from_raw_handle(duplicate as _) })
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

struct LocalAllocation(PSECURITY_DESCRIPTOR);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        unsafe {
            LocalFree(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::enrollment::ClientIdentity;
    use p256::ecdsa::signature::Verifier as _;
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn signing_key_uses_named_pipe_channel() {
        let name = pipe_name("signer");
        let server = create_server(&name).unwrap();
        let accepted = tokio::spawn(accept(server));
        let mut client = ClientOptions::new().open(&name).unwrap();
        write_frame(&mut client, &registration()).await.unwrap();
        let connection = accepted.await.unwrap().unwrap();
        let (key, worker) = connection.into_signing_key(Duration::from_secs(1)).unwrap();
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
        let signature = key
            .choose_scheme(&[TlsSignatureScheme::ECDSA_NISTP256_SHA256])
            .unwrap()
            .sign(b"certificate verify")
            .unwrap();

        assert_eq!(signature, b"DER signature");
        client_task.await.unwrap();
        worker.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn user_agent_registers_and_signs_on_named_pipe() {
        let name = pipe_name("user-agent");
        let server = create_server(&name).unwrap();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(["agentdesktop.test".to_owned()]).unwrap();
        let private_key_pem = signing_key.serialize_pem();
        let expected_key = P256SigningKey::from_pkcs8_pem(&private_key_pem).unwrap();
        let identity = RotatingClientIdentity::new(ClientIdentity {
            certificate_chain_pem: cert.pem(),
            private_key_pem,
        });
        let agent_name = name.clone();
        let mut agent =
            tokio::spawn(async move { run_user_agent_session(&agent_name, &identity).await });
        let connection = accept(server).await.unwrap();
        assert_eq!(connection.registration().certificate_generation, 1);
        let mut pipe = connection.into_pipe();
        write_frame(
            &mut pipe,
            &ForwarderRequest::Sign {
                request_id: 42,
                scheme: SignatureScheme::EcdsaP256Sha256,
                message_base64: base64::engine::general_purpose::STANDARD
                    .encode(b"certificate verify"),
            },
        )
        .await
        .unwrap();
        let response: AgentResponse = match read_frame(&mut pipe).await {
            Ok(response) => response,
            Err(error) => panic!(
                "read signing response: {error}; agent: {:?}",
                (&mut agent).await
            ),
        };
        let AgentResponse::Signature {
            request_id,
            signature_base64,
        } = response
        else {
            panic!("user agent rejected signing request");
        };
        assert_eq!(request_id, 42);
        let signature = Signature::from_der(
            &base64::engine::general_purpose::STANDARD
                .decode(signature_base64)
                .unwrap(),
        )
        .unwrap();
        expected_key
            .verifying_key()
            .verify(b"certificate verify", &signature)
            .unwrap();
        agent.abort();
    }
}
