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
use tokio::net::UnixStream;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use super::SessionConnection;
use super::SessionRegistry;
use crate::service::hbone::{ExternalClientIdentity, HboneClient, RotatingClientIdentity};
use crate::session_protocol::managed;
use crate::session_protocol::{
    AgentResponse, ForwarderRequest, Registration, SignatureScheme, read_frame, write_frame,
};

pub(super) async fn register(
    registry: &SessionRegistry,
    connection: SessionConnection,
    generation: u64,
    registration: managed::Registration,
) -> Result<(HboneClient, tokio::task::JoinHandle<Result<()>>)> {
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
    let (signing_key, monitor) = connection.into_signing_key(registry.connect_timeout)?;
    let identity = RotatingClientIdentity::new_external(
        ExternalClientIdentity {
            certificates,
            signing_key,
        },
        generation,
    );
    #[cfg(test)]
    let client = if let Some(roots) = registry.roots.clone() {
        HboneClient::connect_mtls_with_roots(
            registry.endpoint,
            registry.server_name.clone(),
            identity,
            registry.connect_timeout,
            roots,
        )
        .await?
    } else {
        HboneClient::connect_mtls(
            registry.endpoint,
            registry.server_name.clone(),
            identity,
            registry.connect_timeout,
        )
        .await?
    };
    #[cfg(not(test))]
    let client = HboneClient::connect_mtls(
        registry.endpoint,
        registry.server_name.clone(),
        identity,
        registry.connect_timeout,
    )
    .await?;
    Ok((client, monitor))
}

pub async fn run_user_agent(
    path: &std::path::Path,
    identity: RotatingClientIdentity,
    reconnect_delay: Duration,
) -> Result<()> {
    loop {
        let result: Result<()> = async {
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
                &Registration::managed(generation, certificates),
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

pub(super) async fn serve_signing_requests(
    stream: &mut UnixStream,
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
    pub(super) fn into_signing_key(self, timeout: Duration) -> Result<SigningKeyWorker> {
        let (requests, receiver) = unbounded_channel();
        let stream = self
            .stream
            .into_std()
            .context("detach user session socket from service runtime")?;
        let worker =
            super::super::spawn_runtime_worker("agentdesktop-session-signer", move || {
                let stream = UnixStream::from_std(stream)
                    .context("attach user session socket to signing runtime")?;
                Ok(run_signing_worker(stream, receiver, timeout))
            })?;
        Ok((
            Arc::new(SessionSigningKey {
                requests,
                next_request_id: AtomicU64::new(1),
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
    mut stream: UnixStream,
    mut requests: tokio::sync::mpsc::UnboundedReceiver<SigningWork>,
    timeout: Duration,
) -> Result<()> {
    loop {
        let work = tokio::select! {
            work = requests.recv() => match work {
                Some(work) => work,
                None => return Ok(()),
            },
            incoming = read_frame::<AgentResponse, _>(&mut stream) => {
                incoming.context("monitor user session socket")?;
                anyhow::bail!("user session sent an unsolicited signing response");
            }
        };
        let response = tokio::time::timeout(timeout, async {
            write_frame(&mut stream, &work.request)
                .await
                .context("send signing request")?;
            read_frame(&mut stream)
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
