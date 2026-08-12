use std::collections::HashMap;
use std::ffi::c_void;
use std::net::SocketAddr;
use std::os::windows::io::AsRawHandle;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use rustls::pki_types::CertificateDer;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::RwLock;
use tokio::sync::watch;
use tokio::task::JoinSet;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{
    CopySid, GetLengthSid, GetTokenInformation, PSECURITY_DESCRIPTOR, RevertToSelf,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Pipes::ImpersonateNamedPipeClient;
use windows_sys::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

use crate::service::hbone::{ExternalClientIdentity, HboneClient, RotatingClientIdentity};
use crate::session_protocol::{Registration, RegistrationIdentity, read_frame};

mod managed;

pub use managed::run_user_agent;

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
        let sid = connection.sid().clone();
        let generation = connection.registration().certificate_generation;
        let RegistrationIdentity::Managed(registration) = &connection.registration().identity
        else {
            anyhow::bail!("self-managed Windows session registration is not implemented");
        };
        let certificates = registration
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
    use super::managed::run_user_agent_session;
    use super::*;
    use crate::identity::enrollment::ClientIdentity;
    use crate::session_protocol::{AgentResponse, ForwarderRequest, SignatureScheme, write_frame};
    use p256::ecdsa::signature::Verifier as _;
    use p256::ecdsa::{Signature, SigningKey as P256SigningKey};
    use p256::pkcs8::DecodePrivateKey;
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use rustls::SignatureScheme as TlsSignatureScheme;
    use tokio::net::windows::named_pipe::ClientOptions;

    fn pipe_name(label: &str) -> String {
        format!(r"\\.\pipe\agentdesktop-test-{}-{label}", std::process::id())
    }

    fn registration() -> Registration {
        let CertifiedKey { cert, .. } =
            generate_simple_self_signed(["agentdesktop.test".to_owned()]).unwrap();
        Registration::managed(
            1,
            vec![base64::engine::general_purpose::STANDARD.encode(cert.der())],
        )
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
