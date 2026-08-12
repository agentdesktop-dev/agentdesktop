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
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::service::hbone::{
    ExternalClientIdentity, HboneClient, RotatingClientIdentity, TlsRoots,
};
use crate::session_protocol::managed;
use crate::session_protocol::{
    AgentResponse, ForwarderRequest, Registration, SignatureScheme, read_frame, write_frame,
};

pub(crate) type SigningKeyWorker = (Arc<dyn SigningKey>, tokio::task::JoinHandle<Result<()>>);

pub(crate) async fn connect_gateway(
    endpoint: std::net::SocketAddr,
    server_name: String,
    connect_timeout: Duration,
    roots: TlsRoots,
    generation: u64,
    registration: managed::Registration,
    signing_key: Arc<dyn SigningKey>,
) -> Result<HboneClient> {
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
    let identity = RotatingClientIdentity::new_external(
        ExternalClientIdentity {
            certificates,
            signing_key,
        },
        generation,
    );
    HboneClient::connect_mtls_endpoint(endpoint, server_name, identity, connect_timeout, roots).await
}

pub(crate) async fn run_agent_session<Transport>(
    transport: &mut Transport,
    identity: &RotatingClientIdentity,
) -> Result<()>
where
    Transport: AsyncRead + AsyncWrite + Unpin,
{
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
        &mut *transport,
        &Registration::managed(generation, certificates),
    )
    .await
    .context("register user certificate")?;
    let signing_key = P256SigningKey::from_pkcs8_pem(&client_identity.private_key_pem)
        .context("parse user signing key")?;
    serve_signing_requests(transport, identity, generation, &signing_key).await
}

pub(crate) async fn serve_signing_requests<Transport>(
    transport: &mut Transport,
    identity: &RotatingClientIdentity,
    generation: u64,
    signing_key: &P256SigningKey,
) -> Result<()>
where
    Transport: AsyncRead + AsyncWrite + Unpin,
{
    let mut current_generation = identity.subscribe_generation();
    loop {
        tokio::select! {
            changed = current_generation.changed() => {
                changed.context("managed identity generation closed")?;
                if *current_generation.borrow() != generation {
                    return Ok(());
                }
            }
            request = read_frame::<ForwarderRequest, _>(&mut *transport) => {
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
                write_frame(&mut *transport, &response)
                    .await
                    .context("write signing response")?;
            }
        }
    }
}

pub(crate) fn spawn_signing_key<Transport, Attach>(
    timeout: Duration,
    attach: Attach,
) -> Result<SigningKeyWorker>
where
    Transport: AsyncRead + AsyncWrite + Unpin + 'static,
    Attach: FnOnce() -> Result<Transport> + Send + 'static,
{
    let (requests, receiver) = unbounded_channel();
    let worker = super::spawn_runtime_worker("agentdesktop-session-signer", move || {
        let transport = attach()?;
        Ok(run_signing_worker(transport, receiver, timeout))
    })?;
    Ok((
        Arc::new(SessionSigningKey {
            requests,
            next_request_id: AtomicU64::new(1),
        }),
        worker,
    ))
}

struct SigningWork {
    request: ForwarderRequest,
    response: Sender<Result<AgentResponse, String>>,
}

async fn run_signing_worker<Transport>(
    mut transport: Transport,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<SigningWork>,
    timeout: Duration,
) -> Result<()>
where
    Transport: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let work = tokio::select! {
            work = requests.recv() => match work {
                Some(work) => work,
                None => return Ok(()),
            },
            incoming = read_frame::<AgentResponse, _>(&mut transport) => {
                incoming.context("monitor user session transport")?;
                anyhow::bail!("user session sent an unsolicited signing response");
            }
        };
        let response = tokio::time::timeout(timeout, async {
            write_frame(&mut transport, &work.request)
                .await
                .context("send signing request")?;
            read_frame(&mut transport)
                .await
                .context("read signing response")
        })
        .await
        .context("user session signing timed out")
        .and_then(|response| response);
        match response {
            Ok(response) => {
                let _ = work.response.send(Ok(response));
            }
            Err(error) => {
                let reason = error.to_string();
                let _ = work.response.send(Err(reason));
                return Err(error);
            }
        }
    }
}

#[derive(Debug)]
struct SessionSigningKey {
    requests: UnboundedSender<SigningWork>,
    next_request_id: AtomicU64,
}

impl SigningKey for SessionSigningKey {
    fn choose_scheme(&self, offered: &[TlsSignatureScheme]) -> Option<Box<dyn Signer>> {
        offered
            .contains(&TlsSignatureScheme::ECDSA_NISTP256_SHA256)
            .then(|| {
                Box::new(SessionSigner {
                    requests: self.requests.clone(),
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
    requests: UnboundedSender<SigningWork>,
    request_id: u64,
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
            .recv()
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
