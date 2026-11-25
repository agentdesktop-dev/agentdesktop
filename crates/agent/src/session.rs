//! The OIDC session of the user signed in on this device.
//!
//! Every device keeps its user's session in this one store, apart from the
//! device identity, and goes through one lifecycle: a user signs in
//! ([`oidc::enroll`] or [`oidc::attach_user`]), [`current`] refreshes the
//! session, and [`sign_out`] or a refresh token the identity provider
//! rejects ends it. How the device enrolled ([`Enrollment`]) decides only
//! what the session is for:
//!
//! - A device a user enrolled by signing in is that user's. Every controller
//!   call carries the session's access token, and ending the session also
//!   removes the device identity, so the device enrolls again.
//! - A device enrolled with an enrollment token authenticates with its
//!   certificate alone. Users sign in and out on it afterwards; only
//!   `GetLlmGatewayCredential` carries the access token, and the device stays
//!   enrolled when the session ends.

use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use agentdesktop_core::{config::ControllerConnectionConfig, model::EnrollmentStatus};
use agentdesktop_proto::fleet::DetachUserRequest;
use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{Mutex, oneshot},
    task::JoinHandle,
};

use crate::{
    enrollment::EnrollmentState,
    identity::{self, Enrollment, Identity},
    oidc, remote,
    secret_store::SecretStore,
    secure_fs,
};

/// Where the session's OAuth credentials are kept in the secret store, under
/// the device ID.
pub(crate) const OAUTH_SERVICE: &str = "dev.agentdesktop.device-oauth";
const METADATA_FILE: &str = "session.json";
/// How long starting a sign-in waits for the page the user signs in at.
const SIGN_IN_PROMPT_TIMEOUT: Duration = Duration::from_secs(15);
const SIGN_OUT_CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Serializes refreshing and ending the session, so a rotated refresh token
/// is used once and a refresh never stores a session that just ended.
static REFRESH: Mutex<()> = Mutex::const_new(());

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct OAuthCredentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at_unix_seconds: u64,
}

/// A signed-in user's OIDC session.
#[derive(Clone, Debug)]
pub struct Session {
    /// Names this sign-in; a later one, by anyone, gets another ID.
    pub id: String,
    pub oauth: OAuthCredentials,
    pub token_endpoint: String,
    pub client_id: String,
}

/// What is kept outside the secret store: enough to tell that a user is
/// signed in, and on which device identity, without opening the store.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredSession {
    device_id: String,
    session_id: String,
    token_endpoint: String,
    client_id: String,
}

/// Marks a stored session whose OAuth credentials cannot be read, for example
/// a Keychain item the app lost access to. It cannot be refreshed either.
#[derive(Debug)]
struct SessionUnreadable;

impl std::fmt::Display for SessionUnreadable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stored OIDC session is unreadable")
    }
}

/// Runs `work`, which uses the secret store or writes files, off the async
/// runtime's threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .context("session store task panicked")?
}

async fn load_enrollment(state_dir: &Path) -> anyhow::Result<Option<(String, Enrollment)>> {
    let path = state_dir.join("identity.json");
    blocking(move || identity::load_enrollment(&path)).await
}

/// Whether a user is signed in on this device: `signedIn` or `signedOut`.
/// `None` unless the device is enrolled with an enrollment token; any other
/// device has its user signed in for as long as it is enrolled.
pub async fn user_status(state_dir: &Path) -> Option<&'static str> {
    let (device_id, enrollment) = load_enrollment(state_dir).await.ok().flatten()?;
    if enrollment.belongs_to_user() {
        return None;
    }
    match load_metadata(state_dir, &device_id).await {
        Ok(Some(_)) => Some("signedIn"),
        Ok(None) | Err(_) => Some("signedOut"),
    }
}

/// Whom the device's LLM gateway credentials are issued to, for telling a
/// cached credential's owner from the current one: the enrolled device and
/// the user's sign-in. `None` when the device is not enrolled. Neither the
/// secret store nor the identity provider is involved.
pub(crate) async fn credential_owner(state_dir: &Path) -> Option<String> {
    let (device_id, _) = load_enrollment(state_dir).await.ok().flatten()?;
    Some(match load_metadata(state_dir, &device_id).await {
        Ok(Some(session)) => format!("{device_id}\0{}", session.session_id),
        Ok(None) | Err(_) => device_id,
    })
}

async fn load_metadata(state_dir: &Path, device_id: &str) -> anyhow::Result<Option<StoredSession>> {
    let path = state_dir.join(METADATA_FILE);
    let stored: StoredSession = match tokio::fs::read(&path).await {
        Ok(contents) => serde_json::from_slice(&contents).context("parse OIDC session")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read OIDC session {}", path.display()));
        }
    };
    // A session of an earlier identity of this device is not this device's.
    Ok(Some(stored).filter(|stored| stored.device_id == device_id))
}

async fn load(state_dir: &Path, device_id: &str) -> anyhow::Result<Option<Session>> {
    let Some(stored) = load_metadata(state_dir, device_id).await? else {
        return Ok(None);
    };
    let (store_dir, account) = (state_dir.to_owned(), device_id.to_owned());
    let oauth = blocking(move || {
        let oauth = SecretStore::new(&store_dir)?
            .get(OAUTH_SERVICE, &account)
            .context("read the OIDC session's OAuth credentials")
            .context(SessionUnreadable)?;
        serde_json::from_str(&oauth)
            .context("decode the OIDC session's OAuth credentials")
            .context(SessionUnreadable)
    })
    .await?;
    Ok(Some(Session {
        id: stored.session_id,
        oauth,
        token_endpoint: stored.token_endpoint,
        client_id: stored.client_id,
    }))
}

/// Stores `session` as the session of the user signed in on `device_id`.
pub(crate) async fn save(
    state_dir: &Path,
    device_id: &str,
    session: &Session,
) -> anyhow::Result<()> {
    let state_dir = state_dir.to_owned();
    let oauth = serde_json::to_string(&session.oauth)?;
    let stored = StoredSession {
        device_id: device_id.to_owned(),
        session_id: session.id.clone(),
        token_endpoint: session.token_endpoint.clone(),
        client_id: session.client_id.clone(),
    };
    blocking(move || {
        secure_fs::ensure_private_dir(&state_dir)?;
        identity::store_secret(
            &SecretStore::new(&state_dir)?,
            OAUTH_SERVICE,
            &stored.device_id,
            &oauth,
            "store the OIDC session's OAuth credentials",
        )?;
        write_metadata(&state_dir, &stored)
    })
    .await
}

fn write_metadata(state_dir: &Path, stored: &StoredSession) -> anyhow::Result<()> {
    secure_fs::atomic_write(
        &state_dir.join(METADATA_FILE),
        &serde_json::to_vec_pretty(stored)?,
        0o600,
    )
}

/// Takes over the session of an identity stored while identities held their
/// user's session. Its OAuth credentials are already where this store keeps
/// them; only the token endpoint and client ID move. Synchronous, for
/// [`identity::load`].
pub(crate) fn adopt_legacy(
    state_dir: &Path,
    device_id: &str,
    token_endpoint: &str,
    client_id: &str,
) -> anyhow::Result<()> {
    write_metadata(
        state_dir,
        &StoredSession {
            device_id: device_id.to_owned(),
            session_id: oidc::random_secret(),
            token_endpoint: token_endpoint.to_owned(),
            client_id: client_id.to_owned(),
        },
    )
    .context("move the OIDC session out of the device identity")
}

/// Forgets the session of the user signed in on `device_id`, on this device
/// alone.
pub async fn forget(state_dir: &Path, device_id: &str) -> anyhow::Result<()> {
    let _guard = REFRESH.lock().await;
    forget_locked(state_dir, device_id).await
}

async fn forget_locked(state_dir: &Path, device_id: &str) -> anyhow::Result<()> {
    let path = state_dir.join(METADATA_FILE);
    // Without the metadata no user is signed in, whatever happens to the secret.
    match tokio::fs::remove_file(&path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("remove OIDC session {}", path.display()));
        }
    }
    let (state_dir, device_id) = (state_dir.to_owned(), device_id.to_owned());
    blocking(move || {
        SecretStore::new(&state_dir)?
            .delete(OAUTH_SERVICE, &device_id)
            .context("delete the OIDC session's OAuth credentials")
    })
    .await
}

/// The session of the user signed in on `device_id`, with its access token
/// refreshed when it is about to expire or is `rejected`: the token the
/// controller just turned down. `None` when no user is signed in.
///
/// A refresh token the identity provider rejects ends the session (the error
/// tells: [`remote::is_oauth_refresh_rejected`]), as does a session that can
/// no longer be read. Either way the session is forgotten; what else that
/// means depends on the device's [`Enrollment`] and is up to the caller.
pub(crate) async fn current(
    state_dir: &Path,
    device_id: &str,
    rejected: Option<&str>,
) -> anyhow::Result<Option<Session>> {
    let _guard = REFRESH.lock().await;
    let mut session = match load(state_dir, device_id).await {
        Ok(Some(session)) => session,
        Ok(None) => return Ok(None),
        Err(error) if error.downcast_ref::<SessionUnreadable>().is_some() => {
            tracing::warn!(
                device_id,
                error = %format!("{error:#}"),
                "the stored OIDC session is unreadable; signing the user out"
            );
            forget_locked(state_dir, device_id).await?;
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    let expiring = session.oauth.expires_at_unix_seconds <= unix_time_seconds().saturating_add(120);
    if !expiring && rejected != Some(session.oauth.access_token.as_str()) {
        return Ok(Some(session));
    }
    let refreshed = oidc::refresh_access_token(
        &session.token_endpoint,
        &session.client_id,
        &session.oauth.refresh_token,
    )
    .await;
    let response = match refreshed {
        Ok(response) => response,
        Err(error) if remote::is_oauth_refresh_rejected(&error) => {
            forget_locked(state_dir, device_id).await?;
            tracing::warn!(
                device_id,
                error = %format!("{error:#}"),
                "the OIDC refresh token was rejected; signed the user out"
            );
            return Err(error.context("the user's session ended; sign in again"));
        }
        Err(error) => return Err(error),
    };
    session.oauth = refreshed_credentials(session.oauth, response);
    save(state_dir, device_id, &session)
        .await
        .context("persist the rotated OIDC refresh token")?;
    tracing::info!(device_id, "refreshed OIDC access token");
    Ok(Some(session))
}

fn refreshed_credentials(
    current: OAuthCredentials,
    response: oidc::TokenResponse,
) -> OAuthCredentials {
    OAuthCredentials {
        access_token: response.access_token,
        refresh_token: response.refresh_token.unwrap_or(current.refresh_token),
        expires_at_unix_seconds: unix_time_seconds().saturating_add(response.expires_in),
    }
}

/// Signs the user out. On a device that is the user's this removes the device
/// identity too, so the device enrolls again. Any other device stays
/// enrolled, and the controller is told that the user left.
///
/// The controller is told on a best-effort basis. If it cannot be reached it
/// keeps naming the user on the device, but issues no LLM gateway credential:
/// that takes the user's access token, which is gone.
pub(crate) async fn sign_out(
    controller: &ControllerConnectionConfig,
    identity_path: &Path,
    identity: &Identity,
) -> anyhow::Result<()> {
    if identity.enrollment.belongs_to_user() {
        return identity::delete(identity_path, &identity.device_id).await;
    }
    let state_dir = identity_path
        .parent()
        .context("identity path has no parent")?;
    forget(state_dir, &identity.device_id).await?;
    tracing::info!(device_id = %identity.device_id, "signed out user on device");
    let detach = async {
        let mut client = remote::client(controller, Some(identity)).await?;
        client
            .detach_user(DetachUserRequest {})
            .await
            .context("sign out user on device")?;
        anyhow::Ok(())
    };
    match tokio::time::timeout(SIGN_OUT_CALL_TIMEOUT, detach).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            error = %format!("{error:#}"),
            "signed the user out locally, but could not tell the controller"
        ),
        Err(_) => {
            tracing::warn!("signed the user out locally, but the controller did not answer in time")
        }
    }
    Ok(())
}

/// Signs a user in on a device enrolled with an enrollment token, on behalf
/// of the local API. Signing out goes through the controller session loop,
/// like on any other device.
#[derive(Clone)]
pub struct SignIn {
    controller: ControllerConnectionConfig,
    state_dir: PathBuf,
    enrollment: EnrollmentState,
    callback_listen: Option<SocketAddr>,
    open_browser: bool,
    /// The sign-in the user has not completed yet.
    pending: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl SignIn {
    pub fn new(
        controller: ControllerConnectionConfig,
        state_dir: PathBuf,
        enrollment: EnrollmentState,
        callback_listen: Option<SocketAddr>,
        open_browser: bool,
    ) -> Self {
        Self {
            controller,
            state_dir,
            enrollment,
            callback_listen,
            open_browser,
            pending: Arc::default(),
        }
    }

    /// The device's enrollment status, with whether a user is signed in.
    pub async fn status(&self) -> EnrollmentStatus {
        let mut status = self.enrollment.get().await;
        if status.status == "enrolled" && status.user.is_none() {
            status.user = user_status(&self.state_dir).await.map(str::to_owned);
        }
        status
    }

    /// Starts a user's sign-in and returns once there is something to show:
    /// the page where the user signs in, or the outcome if the sign-in ended
    /// first. The sign-in itself continues in the background. Calling this
    /// again while one is pending returns the same page.
    pub async fn start(&self) -> anyhow::Result<EnrollmentStatus> {
        let status = self.status().await;
        match status.user.as_deref() {
            None if status.status != "enrolled" => bail!("the device is not enrolled yet"),
            None => bail!("the device is not enrolled with an enrollment token"),
            Some("signedIn") => return Ok(status),
            Some(_) => {}
        }
        let mut pending = self.pending.lock().await;
        let mut outcome = None;
        if pending.as_ref().is_none_or(JoinHandle::is_finished) {
            let (sender, receiver) = oneshot::channel();
            outcome = Some(receiver);
            let sign_in = self.clone();
            *pending = Some(tokio::spawn(async move {
                let result = sign_in.complete().await;
                sign_in.enrollment.user_sign_in_ended().await;
                if let Err(error) = &result {
                    tracing::warn!(error = %format!("{error:#}"), "user sign-in failed");
                }
                let _ = sender.send(result);
            }));
        }
        drop(pending);

        let prompt = async {
            loop {
                let status = self.status().await;
                if status.authorization_url.is_some() || status.user.as_deref() != Some("signedOut")
                {
                    return status;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        let ended = async {
            match outcome {
                Some(outcome) => outcome.await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            status = prompt => Ok(status),
            result = ended => {
                result.context("user sign-in stopped")??;
                Ok(self.status().await)
            }
            () = tokio::time::sleep(SIGN_IN_PROMPT_TIMEOUT) => Ok(self.status().await),
        }
    }

    async fn complete(&self) -> anyhow::Result<()> {
        let path = self.state_dir.join("identity.json");
        let identity = blocking(move || identity::load(&path))
            .await?
            .context("device is not enrolled")?;
        if identity.enrollment.belongs_to_user() {
            bail!("the device is not enrolled with an enrollment token");
        }
        let session = oidc::attach_user(
            &self.controller,
            &identity,
            &self.enrollment,
            self.callback_listen,
            self.open_browser,
        )
        .await?;
        save(&self.state_dir, &identity.device_id, &session).await?;
        tracing::info!(device_id = %identity.device_id, "signed in user on device");
        Ok(())
    }

    /// Cancels the sign-in the user has not completed yet, if any.
    pub async fn cancel(&self) {
        if let Some(pending) = self.pending.lock().await.take() {
            pending.abort();
        }
        self.enrollment.user_sign_in_ended().await;
    }
}

fn unix_time_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod credential_tests {
    use super::{OAuthCredentials, oidc::TokenResponse, refreshed_credentials};

    #[test]
    fn refresh_replaces_rotated_oauth_credentials() {
        let current = OAuthCredentials {
            access_token: "old-access".to_owned(),
            refresh_token: "old-refresh".to_owned(),
            expires_at_unix_seconds: 0,
        };
        let refreshed = refreshed_credentials(
            current.clone(),
            TokenResponse {
                access_token: "new-access".to_owned(),
                id_token: None,
                refresh_token: Some("new-refresh".to_owned()),
                expires_in: 600,
                token_type: "Bearer".to_owned(),
            },
        );
        assert_eq!(refreshed.access_token, "new-access");
        assert_eq!(refreshed.refresh_token, "new-refresh");
        assert!(refreshed.expires_at_unix_seconds > 0);

        let kept = refreshed_credentials(
            current,
            TokenResponse {
                access_token: "new-access".to_owned(),
                id_token: None,
                refresh_token: None,
                expires_in: 600,
                token_type: "Bearer".to_owned(),
            },
        );
        assert_eq!(
            kept.refresh_token, "old-refresh",
            "a refresh token that was not rotated is kept"
        );
    }
}

/// Linux uses a temporary file-backed secret store, not a user's Keychain.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use agentdesktop_proto::fleet::{
        fleet_agent_server::{FleetAgent, FleetAgentServer},
        *,
    };
    use axum::{Form, Json, Router, http::StatusCode, routing::post};
    use tokio::sync::Mutex;
    use tokio_stream::wrappers::ReceiverStream;
    use tonic::Request;

    use super::*;

    type Reply<T> = Result<tonic::Response<T>, tonic::Status>;

    /// A controller that records who signed in and how often anyone signed out.
    #[derive(Clone)]
    struct Controller {
        login: LoginConfiguration,
        /// The access token and ID token of each `AttachUser`.
        attached: Arc<Mutex<Vec<(String, String)>>>,
        detached: Arc<AtomicUsize>,
    }

    #[tonic::async_trait]
    impl FleetAgent for Controller {
        async fn get_login_configuration(
            &self,
            _: Request<LoginConfigurationRequest>,
        ) -> Reply<LoginConfiguration> {
            Ok(tonic::Response::new(self.login.clone()))
        }
        async fn attach_user(
            &self,
            request: Request<AttachUserRequest>,
        ) -> Reply<AttachUserResponse> {
            let access_token = request
                .metadata()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_owned();
            let mut attached = self.attached.lock().await;
            attached.push((access_token, request.into_inner().id_token));
            if attached.len() == 1 {
                return Err(tonic::Status::unavailable("simulated lost response"));
            }
            Ok(tonic::Response::new(AttachUserResponse {}))
        }
        async fn detach_user(&self, _: Request<DetachUserRequest>) -> Reply<DetachUserResponse> {
            self.detached.fetch_add(1, Ordering::SeqCst);
            Ok(tonic::Response::new(DetachUserResponse {}))
        }
        async fn enroll_device(&self, _: Request<EnrollDeviceRequest>) -> Reply<EnrollResponse> {
            Err(tonic::Status::unimplemented("not used"))
        }
        async fn renew_device_certificate(
            &self,
            _: Request<RenewDeviceCertificateRequest>,
        ) -> Reply<DeviceCertificateResponse> {
            Err(tonic::Status::unimplemented("not used"))
        }
        async fn get_llm_gateway_credential(
            &self,
            _: Request<LlmGatewayCredentialRequest>,
        ) -> Reply<LlmGatewayCredentialResponse> {
            Err(tonic::Status::unimplemented("not used"))
        }
        async fn get_device_usage(
            &self,
            _: Request<DeviceUsageRequest>,
        ) -> Reply<DeviceUsageResponse> {
            Err(tonic::Status::unimplemented("not used"))
        }
        type ConnectStream = ReceiverStream<Result<ControllerMessage, tonic::Status>>;
        async fn connect(
            &self,
            _: Request<tonic::Streaming<AgentMessage>>,
        ) -> Reply<Self::ConnectStream> {
            Err(tonic::Status::unimplemented("not used"))
        }
    }

    struct Fixture {
        directory: tempfile::TempDir,
        sign_in: SignIn,
        controller: ControllerConnectionConfig,
        attached: Arc<Mutex<Vec<(String, String)>>>,
        detached: Arc<AtomicUsize>,
        tasks: Vec<JoinHandle<()>>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            for task in &self.tasks {
                task.abort();
            }
        }
    }

    impl Fixture {
        /// A device enrolled with an enrollment token, an identity provider
        /// that approves device authorization at once, and a controller.
        /// The identity provider rejects every refresh token but the one it
        /// issued.
        async fn start() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let idp = Router::new()
                .route(
                    "/device",
                    post(|| async {
                        Json(serde_json::json!({
                            "device_code": "device-code", "user_code": "CODE",
                            "verification_uri": "https://idp.example/activate",
                            "expires_in": 600, "interval": 1,
                        }))
                    }),
                )
                .route(
                    "/token",
                    post(
                        |Form(form): Form<std::collections::HashMap<String, String>>| async move {
                            let tokens = |access_token: &str, expires_in: u64| {
                                Json(serde_json::json!({
                                    "access_token": access_token, "refresh_token": "refresh-token",
                                    "id_token": "id-token", "expires_in": expires_in,
                                    "token_type": "Bearer",
                                }))
                            };
                            match form["grant_type"].as_str() {
                                "urn:ietf:params:oauth:grant-type:device_code" => {
                                    Ok(tokens("access-token", 600))
                                }
                                "refresh_token" if form["refresh_token"] == "refresh-token" => {
                                    Ok(tokens("refreshed-access-token", 3600))
                                }
                                _ => Err((
                                    StatusCode::BAD_REQUEST,
                                    Json(serde_json::json!({"error": "invalid_grant"})),
                                )),
                            }
                        },
                    ),
                );
            let idp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let idp_address = idp_listener.local_addr().unwrap();
            let idp_task = tokio::spawn(async move {
                axum::serve(idp_listener, idp).await.unwrap();
            });

            let attached = Arc::default();
            let detached = Arc::default();
            let controller = Controller {
                login: LoginConfiguration {
                    issuer: "https://idp.example".to_owned(),
                    token_endpoint: format!("http://{idp_address}/token"),
                    client_id: "client".to_owned(),
                    device_authorization_endpoint: format!("http://{idp_address}/device"),
                    scopes: vec!["openid".to_owned(), "offline_access".to_owned()],
                    ..Default::default()
                },
                attached: Arc::clone(&attached),
                detached: Arc::clone(&detached),
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let controller_task = tokio::spawn(async move {
                tonic::transport::Server::builder()
                    .add_service(FleetAgentServer::new(controller))
                    .serve_with_incoming(tonic::transport::server::TcpIncoming::from(listener))
                    .await
                    .unwrap();
            });

            let certificate =
                rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
            let state_dir = directory.path().to_owned();
            std::fs::write(state_dir.join("ca.pem"), certificate.cert.pem()).unwrap();
            identity::save(
                &state_dir.join("identity.json"),
                &Identity {
                    device_id: "token-device".to_owned(),
                    client_certificate_pem: certificate.cert.pem(),
                    client_private_key_pem: certificate.signing_key.serialize_pem(),
                    client_certificate_expires_at_unix_seconds: unix_time_seconds() + 86400,
                    enrollment: Enrollment::EnrollmentToken,
                },
            )
            .unwrap();
            let enrollment = EnrollmentState::new(true);
            enrollment.set("enrolled").await;
            let controller = ControllerConnectionConfig {
                address: format!("http://{address}"),
                ca_certificate_path: state_dir.join("ca.pem"),
                heartbeat_interval: Duration::from_secs(30),
                device_authorization: true,
                enrollment_token_path: Some(state_dir.join("enrollment-token")),
            };
            let sign_in = SignIn::new(controller.clone(), state_dir, enrollment, None, false);
            Self {
                directory,
                sign_in,
                controller,
                attached,
                detached,
                tasks: vec![idp_task, controller_task],
            }
        }

        async fn signed_in(&self) -> EnrollmentStatus {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let status = self.sign_in.status().await;
                    if status.user.as_deref() == Some("signedIn") {
                        return status;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the user must be signed in")
        }

        /// Makes the signed-in user's access token due for a refresh, with
        /// `refresh_token` to refresh it.
        async fn expire_access_token(&self, refresh_token: &str) {
            let state_dir = self.directory.path();
            let mut session = load(state_dir, "token-device").await.unwrap().unwrap();
            session.oauth.expires_at_unix_seconds = 0;
            session.oauth.refresh_token = refresh_token.to_owned();
            save(state_dir, "token-device", &session).await.unwrap();
        }

        async fn access_token(&self) -> anyhow::Result<Option<String>> {
            Ok(current(self.directory.path(), "token-device", None)
                .await?
                .map(|session| session.oauth.access_token))
        }

        async fn sign_out(&self) {
            let identity_path = self.directory.path().join("identity.json");
            let identity = identity::load(&identity_path).unwrap().unwrap();
            sign_out(&self.controller, &identity_path, &identity)
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn a_user_signs_in_and_out_while_the_device_stays_enrolled() {
        let fixture = Fixture::start().await;
        let state_dir = fixture.directory.path();
        let identity_path = state_dir.join("identity.json");
        let status = fixture.sign_in.status().await;
        assert_eq!(status.status, "enrolled");
        assert_eq!(status.user.as_deref(), Some("signedOut"));
        assert_eq!(fixture.access_token().await.unwrap(), None);

        let prompt = fixture.sign_in.start().await.unwrap();
        assert_eq!(prompt.status, "enrolled", "signing in is not enrolling");
        assert_eq!(prompt.user.as_deref(), Some("awaitingAuthentication"));
        assert_eq!(
            prompt.authorization_url.as_deref(),
            Some("https://idp.example/activate")
        );
        assert_eq!(prompt.user_code.as_deref(), Some("CODE"));

        let status = fixture.signed_in().await;
        assert_eq!(status.authorization_url, None);
        assert_eq!(
            *fixture.attached.lock().await,
            vec![("Bearer access-token".to_owned(), "id-token".to_owned()); 2],
            "an interrupted call is retried with the same sign-in"
        );
        assert_eq!(
            fixture.sign_in.start().await.unwrap().user.as_deref(),
            Some("signedIn"),
            "signing in again starts nothing"
        );
        assert_eq!(fixture.attached.lock().await.len(), 2);
        assert_eq!(
            fixture.access_token().await.unwrap().as_deref(),
            Some("access-token")
        );
        fixture.expire_access_token("refresh-token").await;
        assert_eq!(
            fixture.access_token().await.unwrap().as_deref(),
            Some("refreshed-access-token")
        );
        let stored_identity = std::fs::read_to_string(&identity_path).unwrap();
        assert!(
            !stored_identity.contains("oauth"),
            "the user's session is not part of the device identity: {stored_identity}"
        );

        fixture.sign_out().await;
        assert_eq!(fixture.detached.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture.sign_in.status().await.user.as_deref(),
            Some("signedOut")
        );
        assert_eq!(fixture.access_token().await.unwrap(), None);
        assert!(identity::load(&identity_path).unwrap().is_some());
    }

    #[tokio::test]
    async fn a_rejected_refresh_token_signs_the_user_out_and_keeps_the_device() {
        let fixture = Fixture::start().await;
        let state_dir = fixture.directory.path();
        fixture.sign_in.start().await.unwrap();
        fixture.signed_in().await;
        fixture.expire_access_token("ended").await;

        let error = fixture.access_token().await.unwrap_err();
        assert!(remote::is_oauth_refresh_rejected(&error), "{error:#}");
        assert!(format!("{error:#}").contains("sign in again"), "{error:#}");
        assert_eq!(
            fixture.sign_in.status().await.user.as_deref(),
            Some("signedOut")
        );
        assert!(
            identity::load(&state_dir.join("identity.json"))
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn an_unreadable_session_signs_the_user_out() {
        let fixture = Fixture::start().await;
        let state_dir = fixture.directory.path();
        fixture.sign_in.start().await.unwrap();
        fixture.signed_in().await;
        SecretStore::new(state_dir)
            .unwrap()
            .set(OAUTH_SERVICE, "token-device", "not json")
            .unwrap();

        assert_eq!(fixture.access_token().await.unwrap(), None);
        assert_eq!(
            fixture.sign_in.status().await.user.as_deref(),
            Some("signedOut")
        );
    }

    #[tokio::test]
    async fn removing_the_device_identity_removes_its_users_session() {
        let fixture = Fixture::start().await;
        let state_dir = fixture.directory.path();
        fixture.sign_in.start().await.unwrap();
        fixture.signed_in().await;

        identity::delete(&state_dir.join("identity.json"), "token-device")
            .await
            .unwrap();
        assert!(!state_dir.join(METADATA_FILE).exists());
        assert_eq!(fixture.sign_in.status().await.user, None);
        assert!(load(state_dir, "token-device").await.unwrap().is_none());
    }
}
