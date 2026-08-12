use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey as P256SigningKey};
use p256::pkcs8::DecodePrivateKey;
use rustls::sign::{Signer, SigningKey};
use rustls::{Error as TlsError, SignatureAlgorithm, SignatureScheme as TlsSignatureScheme};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient, NamedPipeServer};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::SessionConnection;
use crate::service::hbone::RotatingClientIdentity;
use crate::session_protocol::{
    AgentResponse, ForwarderRequest, Registration, SignatureScheme, read_frame, write_frame,
};

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

pub(super) async fn run_user_agent_session(
    path: &str,
    identity: &RotatingClientIdentity,
) -> Result<()> {
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
    write_frame(&mut pipe, &Registration::managed(generation, certificates))
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

impl SessionConnection {
    pub(super) fn into_signing_key(self, timeout: Duration) -> Result<SigningKeyWorker> {
        let (requests, receiver) = unbounded_channel();
        let pipe = detach_pipe(self.pipe)?;
        let worker =
            super::super::spawn_runtime_worker("agentdesktop-session-signer", move || {
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

fn detach_pipe(pipe: NamedPipeServer) -> Result<OwnedHandle> {
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
    Ok(unsafe { OwnedHandle::from_raw_handle(duplicate as _) })
}
