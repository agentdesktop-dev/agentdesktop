//! The user signed in on a device that was enrolled with an enrollment token.
//!
//! Such a device has an identity of its own and no user. A user can sign in
//! on it afterwards, which is what makes an LLM gateway credential available.
//! The user's session is stored apart from the device identity, so signing
//! out, or a session the identity provider ends, never unenrolls the device.

use std::{
    fs,
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
    identity::{self, OAuthCredentials},
    oidc, remote,
    secret_store::SecretStore,
    secure_fs,
};

const OAUTH_SERVICE: &str = "dev.agentdesktop.device-user-oauth";
const METADATA_FILE: &str = "user-session.json";
/// How long starting a sign-in waits for the page the user signs in at.
const SIGN_IN_PROMPT_TIMEOUT: Duration = Duration::from_secs(15);
const SIGN_OUT_CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// Serializes refreshing the session, so a rotated refresh token is used once.
static REFRESH: Mutex<()> = Mutex::const_new(());

/// A signed-in user's OAuth session.
pub struct UserSession {
    /// Names this sign-in; a later one, by anyone, gets another ID.
    pub id: String,
    pub oauth: OAuthCredentials,
    pub oauth_token_endpoint: String,
    pub oauth_client_id: String,
}

/// What is kept outside the secret store: enough to tell that a user is
/// signed in, and on which device identity, without opening the store.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredSession {
    device_id: String,
    /// Names this sign-in; a later one, by anyone, gets another ID.
    session_id: String,
    oauth_token_endpoint: String,
    oauth_client_id: String,
}

/// Whether a user is signed in on this device: `signedIn` or `signedOut`.
/// `None` unless the device is enrolled with an enrollment token.
pub fn user_status(state_dir: &Path) -> Option<&'static str> {
    let device_id = identity::load_token_enrolled_device_id(&state_dir.join("identity.json"))
        .ok()
        .flatten()?;
    match load_metadata(state_dir, &device_id) {
        Ok(Some(_)) => Some("signedIn"),
        Ok(None) | Err(_) => Some("signedOut"),
    }
}

/// Whom the device's LLM gateway credentials are issued to, for telling a
/// cached credential's owner from the current one: the enrolled device and,
/// on a device enrolled with an enrollment token, the user's sign-in. `None`
/// when the device is not enrolled.
pub(crate) fn credential_owner(state_dir: &Path) -> Option<String> {
    let device_id = identity::load_device_id(&state_dir.join("identity.json"))
        .ok()
        .flatten()?;
    Some(match load_metadata(state_dir, &device_id) {
        Ok(Some(session)) => format!("{device_id}\0{}", session.session_id),
        Ok(None) | Err(_) => device_id,
    })
}

fn load_metadata(state_dir: &Path, device_id: &str) -> anyhow::Result<Option<StoredSession>> {
    let path = state_dir.join(METADATA_FILE);
    let stored: StoredSession = match fs::read(&path) {
        Ok(contents) => serde_json::from_slice(&contents).context("parse user session")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read user session {}", path.display()));
        }
    };
    // A session of an earlier identity of this device is not this device's.
    Ok(Some(stored).filter(|stored| stored.device_id == device_id))
}

fn load(state_dir: &Path, device_id: &str) -> anyhow::Result<Option<UserSession>> {
    let Some(stored) = load_metadata(state_dir, device_id)? else {
        return Ok(None);
    };
    let oauth = SecretStore::new(state_dir)?
        .get(OAUTH_SERVICE, device_id)
        .context("read the user's OAuth credentials")?;
    Ok(Some(UserSession {
        id: stored.session_id,
        oauth: serde_json::from_str(&oauth).context("decode the user's OAuth credentials")?,
        oauth_token_endpoint: stored.oauth_token_endpoint,
        oauth_client_id: stored.oauth_client_id,
    }))
}

fn save(state_dir: &Path, device_id: &str, session: &UserSession) -> anyhow::Result<()> {
    secure_fs::ensure_private_dir(state_dir)?;
    SecretStore::new(state_dir)?
        .set(
            OAUTH_SERVICE,
            device_id,
            &serde_json::to_string(&session.oauth)?,
        )
        .context("store the user's OAuth credentials")?;
    let stored = StoredSession {
        device_id: device_id.to_owned(),
        session_id: session.id.clone(),
        oauth_token_endpoint: session.oauth_token_endpoint.clone(),
        oauth_client_id: session.oauth_client_id.clone(),
    };
    secure_fs::atomic_write(
        &state_dir.join(METADATA_FILE),
        &serde_json::to_vec_pretty(&stored)?,
        0o600,
    )
}

/// Removes the session of the user signed in on `device_id`, if there is one.
pub fn forget(state_dir: &Path, device_id: &str) -> anyhow::Result<()> {
    let path = state_dir.join(METADATA_FILE);
    // Without the metadata no user is signed in, whatever happens to the secret.
    if load_metadata(state_dir, device_id).is_ok_and(|stored| stored.is_none()) {
        return Ok(());
    }
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("remove user session {}", path.display()));
        }
    }
    SecretStore::new(state_dir)?
        .delete(OAUTH_SERVICE, device_id)
        .context("delete the user's OAuth credentials")
}

/// The access token of the user signed in on `device_id`, refreshed when it
/// is about to expire. `None` when no user is signed in.
///
/// A refresh token the identity provider rejects ends the session: the user
/// is signed out, and the device stays enrolled.
pub(crate) async fn access_token(
    state_dir: &Path,
    device_id: &str,
) -> anyhow::Result<Option<String>> {
    let _guard = REFRESH.lock().await;
    let Some(mut session) = load(state_dir, device_id)? else {
        return Ok(None);
    };
    if session.oauth.expires_at_unix_seconds <= unix_time_seconds().saturating_add(120) {
        let refreshed = oidc::refresh_access_token(
            &session.oauth_token_endpoint,
            &session.oauth_client_id,
            &session.oauth.refresh_token,
        )
        .await;
        let response = match refreshed {
            Ok(response) => response,
            Err(error) if remote::is_oauth_refresh_rejected(&error) => {
                forget(state_dir, device_id)?;
                tracing::warn!(
                    device_id,
                    error = %format!("{error:#}"),
                    "the user's OIDC refresh token was rejected; signed the user out"
                );
                return Err(error.context("the user's session ended; sign in again"));
            }
            Err(error) => return Err(error),
        };
        session.oauth = OAuthCredentials {
            access_token: response.access_token,
            refresh_token: response
                .refresh_token
                .unwrap_or(session.oauth.refresh_token),
            expires_at_unix_seconds: unix_time_seconds().saturating_add(response.expires_in),
        };
        save(state_dir, device_id, &session).context("persist the user's rotated refresh token")?;
        tracing::info!(device_id, "refreshed the user's OIDC access token");
    }
    Ok(Some(session.oauth.access_token))
}

/// Signs a user in and out on this device, on behalf of the local API.
#[derive(Clone)]
pub struct UserSessions {
    controller: ControllerConnectionConfig,
    state_dir: PathBuf,
    enrollment: EnrollmentState,
    callback_listen: Option<SocketAddr>,
    open_browser: bool,
    /// The sign-in the user has not completed yet.
    pending: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl UserSessions {
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
            status.user = user_status(&self.state_dir).map(str::to_owned);
        }
        status
    }

    /// Starts a user's sign-in and returns once there is something to show:
    /// the page where the user signs in, or the outcome if the sign-in ended
    /// first. The sign-in itself continues in the background. Calling this
    /// again while one is pending returns the same page.
    pub async fn sign_in(&self) -> anyhow::Result<EnrollmentStatus> {
        match self.status().await.user.as_deref() {
            None => bail!("the device is not enrolled with an enrollment token"),
            Some("signedIn") => return Ok(self.status().await),
            Some(_) => {}
        }
        let mut pending = self.pending.lock().await;
        let mut outcome = None;
        if pending.as_ref().is_none_or(JoinHandle::is_finished) {
            let (sender, receiver) = oneshot::channel();
            outcome = Some(receiver);
            let sessions = self.clone();
            *pending = Some(tokio::spawn(async move {
                let result = sessions.complete_sign_in().await;
                sessions.enrollment.user_sign_in_ended().await;
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

    async fn complete_sign_in(&self) -> anyhow::Result<()> {
        let identity = identity::load(&self.state_dir.join("identity.json"))?
            .context("device is not enrolled")?;
        if identity.oauth.is_some() {
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
        save(&self.state_dir, &identity.device_id, &session)?;
        tracing::info!(device_id = %identity.device_id, "signed in user on device");
        Ok(())
    }

    /// Signs the user out and cancels a pending sign-in. The device stays
    /// enrolled.
    ///
    /// The controller is told on a best-effort basis. If it cannot be reached
    /// it keeps naming the user on the device, but issues no LLM gateway
    /// credential: that takes the user's access token, which is gone.
    pub async fn sign_out(&self) -> anyhow::Result<()> {
        if let Some(pending) = self.pending.lock().await.take() {
            pending.abort();
        }
        self.enrollment.user_sign_in_ended().await;
        let identity = identity::load(&self.state_dir.join("identity.json"))?
            .context("device is not enrolled")?;
        if identity.oauth.is_some() {
            bail!("the device is not enrolled with an enrollment token");
        }
        {
            let _guard = REFRESH.lock().await;
            forget(&self.state_dir, &identity.device_id)?;
        }
        tracing::info!(device_id = %identity.device_id, "signed out user on device");
        let detach = async {
            let mut client = remote::client(&self.controller, Some(&identity)).await?;
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
            Err(_) => tracing::warn!(
                "signed the user out locally, but the controller did not answer in time"
            ),
        }
        Ok(())
    }
}

fn unix_time_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
    use crate::identity::Identity;

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
        sessions: UserSessions,
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
                    oauth: None,
                    oauth_token_endpoint: String::new(),
                    oauth_client_id: String::new(),
                },
            )
            .unwrap();
            let enrollment = EnrollmentState::new(true);
            enrollment.set("enrolled").await;
            let sessions = UserSessions::new(
                ControllerConnectionConfig {
                    address: format!("http://{address}"),
                    ca_certificate_path: state_dir.join("ca.pem"),
                    heartbeat_interval: Duration::from_secs(30),
                    device_authorization: true,
                    enrollment_token_path: Some(state_dir.join("enrollment-token")),
                },
                state_dir,
                enrollment,
                None,
                false,
            );
            Self {
                directory,
                sessions,
                attached,
                detached,
                tasks: vec![idp_task, controller_task],
            }
        }

        async fn signed_in(&self) -> EnrollmentStatus {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let status = self.sessions.status().await;
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
        fn expire_access_token(&self, refresh_token: &str) {
            let state_dir = self.directory.path();
            let mut session = load(state_dir, "token-device").unwrap().unwrap();
            session.oauth.expires_at_unix_seconds = 0;
            session.oauth.refresh_token = refresh_token.to_owned();
            save(state_dir, "token-device", &session).unwrap();
        }
    }

    #[tokio::test]
    async fn a_user_signs_in_and_out_while_the_device_stays_enrolled() {
        let fixture = Fixture::start().await;
        let state_dir = fixture.directory.path();
        let identity_path = state_dir.join("identity.json");
        let status = fixture.sessions.status().await;
        assert_eq!(status.status, "enrolled");
        assert_eq!(status.user.as_deref(), Some("signedOut"));
        assert_eq!(access_token(state_dir, "token-device").await.unwrap(), None);

        let prompt = fixture.sessions.sign_in().await.unwrap();
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
            fixture.sessions.sign_in().await.unwrap().user.as_deref(),
            Some("signedIn"),
            "signing in again starts nothing"
        );
        assert_eq!(fixture.attached.lock().await.len(), 2);
        assert_eq!(
            access_token(state_dir, "token-device")
                .await
                .unwrap()
                .as_deref(),
            Some("access-token")
        );
        fixture.expire_access_token("refresh-token");
        assert_eq!(
            access_token(state_dir, "token-device")
                .await
                .unwrap()
                .as_deref(),
            Some("refreshed-access-token")
        );
        let identity = identity::load(&identity_path).unwrap().unwrap();
        assert!(
            identity.oauth.is_none(),
            "the user's session is not part of the device identity"
        );

        fixture.sessions.sign_out().await.unwrap();
        assert_eq!(fixture.detached.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture.sessions.status().await.user.as_deref(),
            Some("signedOut")
        );
        assert_eq!(access_token(state_dir, "token-device").await.unwrap(), None);
        assert!(identity::load(&identity_path).unwrap().is_some());
    }

    #[tokio::test]
    async fn a_rejected_refresh_token_signs_the_user_out_and_keeps_the_device() {
        let fixture = Fixture::start().await;
        let state_dir = fixture.directory.path();
        fixture.sessions.sign_in().await.unwrap();
        fixture.signed_in().await;
        fixture.expire_access_token("ended");

        let error = access_token(state_dir, "token-device").await.unwrap_err();
        assert!(format!("{error:#}").contains("sign in again"), "{error:#}");
        assert_eq!(
            fixture.sessions.status().await.user.as_deref(),
            Some("signedOut")
        );
        assert!(
            identity::load(&state_dir.join("identity.json"))
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn removing_the_device_identity_removes_its_users_session() {
        let fixture = Fixture::start().await;
        let state_dir = fixture.directory.path();
        fixture.sessions.sign_in().await.unwrap();
        fixture.signed_in().await;

        identity::delete(&state_dir.join("identity.json"), "token-device").unwrap();
        assert!(!state_dir.join(METADATA_FILE).exists());
        assert_eq!(fixture.sessions.status().await.user, None);
        assert!(load(state_dir, "token-device").unwrap().is_none());
    }
}
