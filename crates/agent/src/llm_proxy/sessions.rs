//! Per-session pairing for the `/sandbox` route and the local API that manages
//! sessions.
//!
//! A session is minted over the local API by whoever starts a sandboxed
//! program (the launcher, the CLI, a test). It binds a one-off pairing secret
//! to a credential-policy client id; the proxy accepts that secret on the
//! `/sandbox` route only and presents the session's client id to the gateway.
//! Sessions live in memory: a daemon restart drops them all.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use agentdesktop_core::model::{SessionCreated, SessionInfo, SessionRequest};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    routing::{delete, get},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};

use super::constant_time_eq;

/// Idle timeout a session gets when the request names none.
pub(crate) const DEFAULT_IDLE_TTL: Duration = Duration::from_secs(28_800);
/// Bounds for a requested idle timeout, in seconds.
const MIN_TTL_SECONDS: u64 = 60;
const MAX_TTL_SECONDS: u64 = 86_400;
/// Unexpired sessions a daemon holds at most: a launcher that leaks sessions
/// fails loudly instead of growing the registry.
pub(crate) const MAX_SESSIONS: usize = 64;
/// Longest a label may be, in Unicode scalar values.
const MAX_LABEL_CHARS: usize = 128;

/// Why a session could not be registered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionError {
    InvalidClientId,
    InvalidLabel,
    Full,
}

/// A registered session as returned by `register`; the only place the
/// plaintext pairing exists.
#[derive(Clone)]
pub(crate) struct Session {
    pub id: String,
    pub pairing: Arc<str>,
    pub client_id: String,
    pub label: String,
    pub created_at_unix_seconds: u64,
    pub expires_at_unix_seconds: u64,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("pairing", &"<redacted>")
            .field("client_id", &self.client_id)
            .field("label", &self.label)
            .field("created_at_unix_seconds", &self.created_at_unix_seconds)
            .field("expires_at_unix_seconds", &self.expires_at_unix_seconds)
            .finish()
    }
}

/// What the proxy learns about a request's session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionRef {
    pub id: String,
    pub client_id: String,
}

/// One registered session. Only the SHA-256 digest of the pairing is kept, so
/// a memory dump or a `Debug` print never yields a usable secret.
struct Entry {
    id: String,
    pairing_hash: [u8; 32],
    client_id: String,
    label: String,
    idle_ttl: Duration,
    created_at_unix_seconds: u64,
    expires_at_unix_seconds: u64,
}

impl Entry {
    fn expired(&self, now: u64) -> bool {
        self.expires_at_unix_seconds <= now
    }

    fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            client_id: self.client_id.clone(),
            label: self.label.clone(),
            created_at_unix_seconds: self.created_at_unix_seconds,
            expires_at_unix_seconds: self.expires_at_unix_seconds,
        }
    }
}

/// The sessions the proxy accepts on the `/sandbox` route. Clones share one
/// registry. The lock is held for plain memory work only, never across an
/// `.await`.
#[derive(Clone, Default)]
pub(crate) struct SessionRegistry {
    entries: Arc<Mutex<Vec<Entry>>>,
}

fn pairing_hash(pairing: &str) -> [u8; 32] {
    Sha256::digest(pairing.as_bytes()).into()
}

fn expiry(now: u64, idle_ttl: Duration) -> u64 {
    now.saturating_add(idle_ttl.as_secs())
}

impl SessionRegistry {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Registers a session that expires `idle_ttl` after its last request.
    /// Expired entries are swept first; the bound counts unexpired ones.
    pub(crate) fn register(
        &self,
        client_id: &str,
        label: &str,
        idle_ttl: Duration,
        now: u64,
    ) -> Result<Session, SessionError> {
        if !agentdesktop_core::config::valid_client_id(client_id) {
            return Err(SessionError::InvalidClientId);
        }
        if !valid_label(label) {
            return Err(SessionError::InvalidLabel);
        }
        let id = format!("{:016x}", rand::random::<u64>());
        let secret: [u8; 32] = rand::random();
        let pairing: Arc<str> = URL_SAFE_NO_PAD.encode(secret).into();
        let mut entries = self.lock();
        entries.retain(|entry| !entry.expired(now));
        if entries.len() >= MAX_SESSIONS {
            return Err(SessionError::Full);
        }
        let expires_at_unix_seconds = expiry(now, idle_ttl);
        entries.push(Entry {
            id: id.clone(),
            pairing_hash: pairing_hash(&pairing),
            client_id: client_id.to_owned(),
            label: label.to_owned(),
            idle_ttl,
            created_at_unix_seconds: now,
            expires_at_unix_seconds,
        });
        Ok(Session {
            id,
            pairing,
            client_id: client_id.to_owned(),
            label: label.to_owned(),
            created_at_unix_seconds: now,
            expires_at_unix_seconds,
        })
    }

    /// The session a pairing belongs to, with its idle timeout restarted
    /// (`expires_at = max(expires_at, now + idle_ttl)`). The digest is compared
    /// against every entry without an early return, so the time a lookup takes
    /// does not depend on which entry matches. An expired pairing is removed on
    /// observation, so it cannot become valid again even if the system clock
    /// steps backward.
    pub(crate) fn authorize(&self, pairing: &str, now: u64) -> Option<SessionRef> {
        let offered = pairing_hash(pairing);
        let mut entries = self.lock();
        let mut found = None;
        let mut expired_index = None;
        for (index, entry) in entries.iter().enumerate() {
            let expired = entry.expired(now);
            let matches = constant_time_eq(&offered, &entry.pairing_hash) & !expired;
            if matches {
                found = Some(index);
            }
            // When the offered pairing matches an expired entry, remember it so
            // it can be removed after the constant-time loop, preventing revival
            // on a backward clock step.
            if constant_time_eq(&offered, &entry.pairing_hash) & expired {
                expired_index = Some(index);
            }
        }
        if let Some(index) = expired_index {
            entries.remove(index);
            return None;
        }
        let entry = &mut entries[found?];
        entry.expires_at_unix_seconds = entry
            .expires_at_unix_seconds
            .max(expiry(now, entry.idle_ttl));
        Some(SessionRef {
            id: entry.id.clone(),
            client_id: entry.client_id.clone(),
        })
    }

    /// Removes the session; `false` when no session has that id.
    pub(crate) fn revoke(&self, id: &str) -> bool {
        let mut entries = self.lock();
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        entries.len() != before
    }

    /// Unexpired sessions, oldest first, without their pairings.
    pub(crate) fn list(&self, now: u64) -> Vec<SessionInfo> {
        let entries = self.lock();
        let mut listed: Vec<_> = entries
            .iter()
            .filter(|entry| !entry.expired(now))
            .map(Entry::info)
            .collect();
        listed.sort_by_key(|info| info.created_at_unix_seconds);
        listed
    }

    /// Drops expired sessions.
    pub(crate) fn sweep(&self, now: u64) {
        self.lock().retain(|entry| !entry.expired(now));
    }

    /// Number of unexpired sessions.
    pub(crate) fn len(&self, now: u64) -> usize {
        self.lock()
            .iter()
            .filter(|entry| !entry.expired(now))
            .count()
    }
}

/// A label is 1-128 Unicode scalar values with no control characters.
pub(crate) fn valid_label(label: &str) -> bool {
    let count = label.chars().count();
    (1..=MAX_LABEL_CHARS).contains(&count) && !label.chars().any(char::is_control)
}

/// Decides whether a client id may be registered as a session: the effective
/// configuration's credential policy, read when the session is created so a
/// refused id fails at `session create` and not at the first prompt.
#[async_trait::async_trait]
pub(crate) trait SessionPolicy: Send + Sync {
    async fn check(&self, client_id: &str) -> Result<(), (StatusCode, String)>;
}

/// The production policy: the effective configuration's gateway
/// authentication and program settings, reloaded on every check.
pub(crate) struct ConfigPolicy {
    pub config: agentdesktop_core::config::DaemonConfig,
    pub state_dir: std::path::PathBuf,
}

#[async_trait::async_trait]
impl SessionPolicy for ConfigPolicy {
    async fn check(&self, client_id: &str) -> Result<(), (StatusCode, String)> {
        let effective =
            crate::api::load_effective_config(&self.config, &self.state_dir).map_err(|error| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("could not read the applied configuration: {error:#}"),
                )
            })?;
        // With controller-issued credentials the gateway policy names the
        // client ids it serves; a session for another id would fail at its
        // first prompt. OIDC tokens carry no client id.
        if let Some(agentdesktop_core::config::LlmGatewayAuthentication::ControllerJwt {
            allowed_client_ids,
            ..
        }) = effective
            .llm_gateway
            .as_ref()
            .and_then(|gateway| gateway.authentication.as_ref())
            && !allowed_client_ids.contains(client_id)
        {
            return Err((
                StatusCode::CONFLICT,
                format!(
                    "client id {client_id} is not in llmGateway.authentication.allowedClientIds"
                ),
            ));
        }
        // The sandbox route presents the gateway identity only; a program in
        // subscription mode would silently switch billing path inside the box.
        if crate::api::program_uses_subscription(&effective, client_id) {
            return Err((
                StatusCode::CONFLICT,
                "subscription mode is not supported on the sandbox route".to_owned(),
            ));
        }
        Ok(())
    }
}

/// How often expired sessions are dropped from the registry.
pub(crate) const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Drops expired sessions every `SWEEP_INTERVAL`, for as long as the daemon runs.
pub(crate) async fn sweep_forever(registry: SessionRegistry) {
    let mut interval = tokio::time::interval(SWEEP_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        registry.sweep(super::now_unix());
    }
}

#[derive(Clone)]
pub(crate) struct SessionApiState {
    pub registry: SessionRegistry,
    /// The bound proxy listener; `None` when the proxy is off.
    pub proxy_address: Option<SocketAddr>,
    pub policy: Arc<dyn SessionPolicy>,
}

/// `POST`/`GET /v1/llm-proxy/sessions` and `DELETE /v1/llm-proxy/sessions/{id}`
/// on the local API. Errors are plain text like the rest of the local API.
pub(crate) fn session_router(state: SessionApiState) -> Router {
    Router::new()
        .route(
            "/v1/llm-proxy/sessions",
            get(list_sessions).post(create_session),
        )
        .route("/v1/llm-proxy/sessions/{id}", delete(revoke_session))
        .with_state(state)
}

type ApiError = (StatusCode, String);

async fn create_session(
    State(state): State<SessionApiState>,
    Json(request): Json<SessionRequest>,
) -> Result<(StatusCode, Json<SessionCreated>), ApiError> {
    let Some(address) = state.proxy_address else {
        return Err((
            StatusCode::CONFLICT,
            "the local LLM proxy is not running (daemon.llmProxy.listen)".to_owned(),
        ));
    };
    if !agentdesktop_core::config::valid_client_id(&request.client_id) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "clientId {:?} is not a valid client id (1-64 characters from A-Z, a-z, 0-9, '.', '_' and '-')",
                request.client_id
            ),
        ));
    }
    if !valid_label(&request.label) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("label must be 1-{MAX_LABEL_CHARS} characters without control characters"),
        ));
    }
    let idle_ttl = match request.ttl_seconds {
        None => DEFAULT_IDLE_TTL,
        Some(seconds) if (MIN_TTL_SECONDS..=MAX_TTL_SECONDS).contains(&seconds) => {
            Duration::from_secs(seconds)
        }
        Some(_) => {
            return Err((
                StatusCode::BAD_REQUEST,
                format!("ttlSeconds must be between {MIN_TTL_SECONDS} and {MAX_TTL_SECONDS}"),
            ));
        }
    };
    state.policy.check(&request.client_id).await?;
    let session = state
        .registry
        .register(
            &request.client_id,
            &request.label,
            idle_ttl,
            super::now_unix(),
        )
        .map_err(|error| match error {
            SessionError::Full => (
                StatusCode::TOO_MANY_REQUESTS,
                format!("too many sessions ({MAX_SESSIONS}); revoke unused ones"),
            ),
            // Both were checked above; a race with the registry's own
            // validation would be a bug, reported as such.
            SessionError::InvalidClientId | SessionError::InvalidLabel => (
                StatusCode::BAD_REQUEST,
                "invalid session request".to_owned(),
            ),
        })?;
    Ok((
        StatusCode::CREATED,
        Json(SessionCreated {
            id: session.id,
            pairing: session.pairing.to_string(),
            client_id: session.client_id,
            label: session.label,
            created_at_unix_seconds: session.created_at_unix_seconds,
            expires_at_unix_seconds: session.expires_at_unix_seconds,
            base_url: sandbox_base_url(address),
        }),
    ))
}

/// What a client sets as its API base URL to reach the sandbox route.
pub(crate) fn sandbox_base_url(address: SocketAddr) -> String {
    format!("http://{address}{}", super::SANDBOX_ROUTE)
}

async fn list_sessions(State(state): State<SessionApiState>) -> Json<Vec<SessionInfo>> {
    Json(state.registry.list(super::now_unix()))
}

async fn revoke_session(
    State(state): State<SessionApiState>,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    // A session that has idled out is gone for the caller even before the
    // sweep has dropped it: it is neither listed nor revocable.
    state.registry.sweep(super::now_unix());
    if state.registry.revoke(&id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((StatusCode::NOT_FOUND, format!("no session {id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentdesktop_core::model::{SessionCreated, SessionInfo};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const TTL: Duration = Duration::from_secs(600);

    fn register_ok(
        registry: &SessionRegistry,
        client_id: &str,
        label: &str,
        ttl: Duration,
        now: u64,
    ) -> Session {
        match registry.register(client_id, label, ttl, now) {
            Ok(session) => session,
            Err(_) => panic!("register({client_id:?}, {label:?}) failed"),
        }
    }

    fn register_err(
        registry: &SessionRegistry,
        client_id: &str,
        label: &str,
        now: u64,
    ) -> Option<SessionError> {
        registry.register(client_id, label, TTL, now).err()
    }

    fn expires_of(registry: &SessionRegistry, id: &str, now: u64) -> u64 {
        registry
            .list(now)
            .into_iter()
            .find(|info| info.id == id)
            .unwrap_or_else(|| panic!("session {id} not listed"))
            .expires_at_unix_seconds
    }

    // --- AC 1: the registry.

    #[test]
    fn register_returns_a_well_formed_session() {
        let registry = SessionRegistry::default();
        let session = register_ok(&registry, "claude-code", "lab", TTL, 1_000);
        assert_eq!(session.id.len(), 16);
        assert!(
            session
                .id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{}",
            session.id
        );
        assert_eq!(session.pairing.len(), 43);
        assert!(
            session
                .pairing
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "pairing must be URL-safe unpadded base64"
        );
        assert_eq!(URL_SAFE_NO_PAD.decode(&*session.pairing).unwrap().len(), 32);
        assert_eq!(session.client_id, "claude-code");
        assert_eq!(session.label, "lab");
        assert_eq!(session.created_at_unix_seconds, 1_000);
        assert_eq!(session.expires_at_unix_seconds, 1_600);

        let other = register_ok(&registry, "claude-code", "lab", TTL, 1_000);
        assert_ne!(session.id, other.id);
        assert_ne!(session.pairing, other.pairing);
    }

    #[test]
    fn authorize_accepts_the_registered_pairing_and_nothing_else() {
        let registry = SessionRegistry::default();
        let session = register_ok(&registry, "claude-code", "lab", TTL, 1_000);
        let other = register_ok(&registry, "my_tool", "other", TTL, 1_000);

        let found = registry.authorize(&session.pairing, 1_001).unwrap();
        assert_eq!(found.id, session.id);
        assert_eq!(found.client_id, "claude-code");
        let found = registry.authorize(&other.pairing, 1_001).unwrap();
        assert_eq!(found.id, other.id);
        assert_eq!(found.client_id, "my_tool");

        // Same length, one character different.
        let last = session.pairing.chars().last().unwrap();
        let swapped = if last == 'A' { 'B' } else { 'A' };
        let wrong = format!("{}{swapped}", &session.pairing[..session.pairing.len() - 1]);
        assert_eq!(wrong.len(), session.pairing.len());
        assert!(registry.authorize(&wrong, 1_001).is_none());
        assert!(registry.authorize("", 1_001).is_none());
        assert!(registry.authorize(&session.id, 1_001).is_none());
        assert!(
            registry
                .authorize(&format!("{}x", session.pairing), 1_001)
                .is_none()
        );
    }

    #[test]
    fn revoke_removes_one_session_and_reports_whether_it_existed() {
        let registry = SessionRegistry::default();
        let first = register_ok(&registry, "claude-code", "one", TTL, 1_000);
        let second = register_ok(&registry, "claude-code", "two", TTL, 1_000);
        assert!(!registry.revoke("0000000000000000"));
        assert!(registry.revoke(&first.id));
        assert!(!registry.revoke(&first.id));
        assert!(registry.authorize(&first.pairing, 1_001).is_none());
        assert!(registry.authorize(&second.pairing, 1_001).is_some());
        assert_eq!(registry.len(1_001), 1);
    }

    #[test]
    fn a_session_expires_after_its_idle_timeout() {
        let registry = SessionRegistry::default();
        let session = register_ok(
            &registry,
            "claude-code",
            "lab",
            Duration::from_secs(60),
            1_000,
        );
        assert!(registry.authorize(&session.pairing, 1_059).is_some());
        // The use at 1_059 moved the expiry to 1_119.
        assert!(registry.authorize(&session.pairing, 1_118).is_some());
        assert!(registry.authorize(&session.pairing, 1_300).is_none());
        // An expired session stays expired, also for a later caller.
        assert!(registry.authorize(&session.pairing, 1_301).is_none());
        assert_eq!(registry.len(1_300), 0);
        assert!(registry.list(1_300).is_empty());
    }

    #[test]
    fn an_expired_pairing_cannot_revive_on_a_backward_clock_step() {
        let registry = SessionRegistry::default();
        let session = register_ok(
            &registry,
            "claude-code",
            "lab",
            Duration::from_secs(60),
            1_000,
        );
        // Session expires at 1_060. At 1_061 it is expired and rejected.
        assert!(registry.authorize(&session.pairing, 1_061).is_none());
        // The rejection removed the entry. Even if the clock steps back to
        // 1_059 (before the sweep at ≤1_121), the pairing is gone.
        assert!(registry.authorize(&session.pairing, 1_059).is_none());
        assert_eq!(registry.len(1_059), 0);
    }

    #[test]
    fn authorize_slides_the_expiry_to_now_plus_the_idle_timeout_and_never_backwards() {
        let registry = SessionRegistry::default();
        let session = register_ok(&registry, "claude-code", "lab", TTL, 1_000);
        assert_eq!(expires_of(&registry, &session.id, 1_000), 1_600);

        assert!(registry.authorize(&session.pairing, 1_100).is_some());
        assert_eq!(expires_of(&registry, &session.id, 1_100), 1_700);

        // Two authorizations in the same second do not add twice.
        assert!(registry.authorize(&session.pairing, 1_100).is_some());
        assert!(registry.authorize(&session.pairing, 1_100).is_some());
        assert_eq!(expires_of(&registry, &session.id, 1_100), 1_700);

        // A clock that reads earlier does not pull the expiry back.
        assert!(registry.authorize(&session.pairing, 900).is_some());
        assert_eq!(expires_of(&registry, &session.id, 1_100), 1_700);

        // Continuous use outlives the original lifetime.
        for now in (1_100..5_000).step_by(300) {
            assert!(registry.authorize(&session.pairing, now).is_some(), "{now}");
        }
    }

    #[test]
    fn the_bound_is_64_unexpired_sessions() {
        let registry = SessionRegistry::default();
        let mut sessions = Vec::new();
        for n in 0..64 {
            sessions.push(register_ok(
                &registry,
                "claude-code",
                &format!("s{n}"),
                TTL,
                1_000,
            ));
        }
        assert_eq!(registry.len(1_000), 64);
        assert_eq!(
            register_err(&registry, "claude-code", "one too many", 1_000),
            Some(SessionError::Full)
        );
        assert!(registry.revoke(&sessions[10].id));
        assert!(
            registry
                .register("claude-code", "fits again", TTL, 1_000)
                .is_ok()
        );
        assert_eq!(
            register_err(&registry, "claude-code", "full again", 1_000),
            Some(SessionError::Full)
        );
    }

    #[test]
    fn an_expired_but_unswept_session_does_not_count_towards_the_bound() {
        let registry = SessionRegistry::default();
        register_ok(
            &registry,
            "claude-code",
            "short",
            Duration::from_secs(60),
            1_000,
        );
        for n in 0..63 {
            register_ok(
                &registry,
                "claude-code",
                &format!("s{n}"),
                Duration::from_secs(100_000),
                1_000,
            );
        }
        // 64 entries are stored, one of them expired at 1_100 and never swept.
        assert_eq!(registry.len(1_100), 63);
        assert!(registry.register("claude-code", "fits", TTL, 1_100).is_ok());
        assert_eq!(registry.len(1_100), 64);
        assert_eq!(
            register_err(&registry, "claude-code", "full", 1_100),
            Some(SessionError::Full)
        );
    }

    #[test]
    fn sweep_removes_expired_sessions_and_keeps_live_ones() {
        let registry = SessionRegistry::default();
        let short = register_ok(
            &registry,
            "claude-code",
            "short",
            Duration::from_secs(60),
            1_000,
        );
        let long = register_ok(
            &registry,
            "claude-code",
            "long",
            Duration::from_secs(100_000),
            1_000,
        );
        registry.sweep(2_000);
        assert_eq!(registry.len(2_000), 1);
        assert!(registry.authorize(&short.pairing, 2_000).is_none());
        assert!(!registry.revoke(&short.id));
        assert!(registry.authorize(&long.pairing, 2_000).is_some());
    }

    #[test]
    fn list_returns_unexpired_sessions_oldest_first_without_pairings() {
        let registry = SessionRegistry::default();
        let late = register_ok(&registry, "claude-code", "late", TTL, 1_030);
        let early = register_ok(&registry, "my_tool", "early", TTL, 1_010);
        let middle = register_ok(&registry, "claude-code", "middle", TTL, 1_020);
        register_ok(
            &registry,
            "claude-code",
            "gone",
            Duration::from_secs(5),
            1_000,
        );

        let listed = registry.list(1_040);
        assert_eq!(
            listed,
            vec![
                SessionInfo {
                    id: early.id.clone(),
                    client_id: "my_tool".into(),
                    label: "early".into(),
                    created_at_unix_seconds: 1_010,
                    expires_at_unix_seconds: 1_610,
                },
                SessionInfo {
                    id: middle.id.clone(),
                    client_id: "claude-code".into(),
                    label: "middle".into(),
                    created_at_unix_seconds: 1_020,
                    expires_at_unix_seconds: 1_620,
                },
                SessionInfo {
                    id: late.id.clone(),
                    client_id: "claude-code".into(),
                    label: "late".into(),
                    created_at_unix_seconds: 1_030,
                    expires_at_unix_seconds: 1_630,
                },
            ]
        );
    }

    #[test]
    fn a_clone_shares_the_sessions_of_the_original() {
        let registry = SessionRegistry::default();
        let clone = registry.clone();
        let session = register_ok(&clone, "claude-code", "lab", TTL, 1_000);
        assert!(registry.authorize(&session.pairing, 1_001).is_some());
        assert!(registry.revoke(&session.id));
        assert!(clone.authorize(&session.pairing, 1_001).is_none());
    }

    #[test]
    fn client_ids_are_validated_like_credential_policy_ids() {
        let registry = SessionRegistry::default();
        for valid in ["my_tool", "a", "claude-code", "tool.v2", &"a".repeat(64)] {
            assert!(
                registry.register(valid, "lab", TTL, 1_000).is_ok(),
                "{valid}"
            );
        }
        for invalid in ["", "bad id", "a/b", &"a".repeat(65)] {
            assert_eq!(
                register_err(&registry, invalid, "lab", 1_000),
                Some(SessionError::InvalidClientId),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn labels_are_one_to_128_scalar_values_without_control_characters() {
        let registry = SessionRegistry::default();
        let max = "x".repeat(128);
        let multibyte = "é".repeat(128);
        for valid in ["a", "lab session", "Sitzung ü 日本", &max, &multibyte] {
            assert!(valid_label(valid), "{valid:?}");
            assert!(
                registry.register("claude-code", valid, TTL, 1_000).is_ok(),
                "{valid:?}"
            );
        }
        let too_long = "x".repeat(129);
        let too_long_multibyte = "é".repeat(129);
        for invalid in [
            "",
            "a\u{7}b",
            "line\nbreak",
            "tab\there",
            "\u{7f}",
            &too_long,
            &too_long_multibyte,
        ] {
            assert!(!valid_label(invalid), "{invalid:?}");
            assert_eq!(
                register_err(&registry, "claude-code", invalid, 1_000),
                Some(SessionError::InvalidLabel),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn debug_output_of_a_session_redacts_the_pairing() {
        let registry = SessionRegistry::default();
        let session = register_ok(&registry, "claude-code", "lab", TTL, 1_000);
        let debug = format!("{session:?}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(!debug.contains(&*session.pairing), "{debug}");
    }

    // --- AC 4: the session API on the local API.

    fn unix_now() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// Stub policy: records the ids it is asked about and answers `answer`.
    struct RecordingPolicy {
        seen: Mutex<Vec<String>>,
        answer: Result<(), (StatusCode, String)>,
    }

    impl RecordingPolicy {
        fn allowing() -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::default(),
                answer: Ok(()),
            })
        }

        fn refusing(status: StatusCode, message: &str) -> Arc<Self> {
            Arc::new(Self {
                seen: Mutex::default(),
                answer: Err((status, message.to_owned())),
            })
        }

        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl SessionPolicy for RecordingPolicy {
        async fn check(&self, client_id: &str) -> Result<(), (StatusCode, String)> {
            self.seen.lock().unwrap().push(client_id.to_owned());
            self.answer.clone()
        }
    }

    struct Api {
        registry: SessionRegistry,
        policy: Arc<RecordingPolicy>,
        app: Router,
    }

    fn api_with(
        proxy_address: Option<SocketAddr>,
        policy: Arc<RecordingPolicy>,
        registry: SessionRegistry,
    ) -> Api {
        let app = session_router(SessionApiState {
            registry: registry.clone(),
            proxy_address,
            policy: policy.clone(),
        });
        Api {
            registry,
            policy,
            app,
        }
    }

    fn api() -> Api {
        api_with(
            Some("127.0.0.1:4000".parse().unwrap()),
            RecordingPolicy::allowing(),
            SessionRegistry::default(),
        )
    }

    struct Reply {
        status: u16,
        headers: String,
        body: String,
    }

    impl Reply {
        fn json<T: serde::de::DeserializeOwned>(&self) -> T {
            serde_json::from_str(&self.body)
                .unwrap_or_else(|error| panic!("{error}: {}", self.body))
        }
    }

    /// One request over an in-memory connection served like the local API.
    async fn call(app: &Router, method: &str, path: &str, body: Option<(&str, &str)>) -> Reply {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let connection = tokio::spawn(crate::daemon::serve_local_connection(server, app.clone()));
        let mut request =
            format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
        match body {
            Some((content_type, body)) => request.push_str(&format!(
                "Content-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )),
            None => request.push_str("Content-Length: 0\r\n\r\n"),
        }
        client.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        tokio::time::timeout(
            Duration::from_secs(10),
            client.read_to_string(&mut response),
        )
        .await
        .expect("response in time")
        .unwrap();
        let _ = connection.await;
        assert!(!response.is_empty(), "no response (the handler panicked?)");
        let (head, body) = response.split_once("\r\n\r\n").expect("response head");
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|status| status.parse().ok())
            .unwrap_or_else(|| panic!("status line in {head}"));
        Reply {
            status,
            headers: head.to_ascii_lowercase(),
            body: body.to_owned(),
        }
    }

    async fn post(app: &Router, body: &str) -> Reply {
        call(
            app,
            "POST",
            "/v1/llm-proxy/sessions",
            Some(("application/json", body)),
        )
        .await
    }

    #[tokio::test]
    async fn post_registers_a_session_and_returns_it_with_the_pairing_and_base_url() {
        let api = api();
        let before = unix_now();
        let reply = post(&api.app, r#"{"clientId":"claude-code","label":"lab"}"#).await;
        assert_eq!(reply.status, 201, "{}", reply.body);
        let created: SessionCreated = reply.json();
        assert_eq!(created.id.len(), 16);
        assert_eq!(created.pairing.len(), 43);
        assert_eq!(created.client_id, "claude-code");
        assert_eq!(created.label, "lab");
        assert!(created.created_at_unix_seconds >= before);
        assert!(created.created_at_unix_seconds <= unix_now());
        assert_eq!(
            created.expires_at_unix_seconds - created.created_at_unix_seconds,
            28_800,
            "default idle timeout"
        );
        assert_eq!(created.base_url, "http://127.0.0.1:4000/sandbox");
        let json: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        assert!(json.get("pairing").is_some());
        assert!(json.get("baseUrl").is_some());
        assert_eq!(api.policy.seen(), vec!["claude-code".to_owned()]);
        // The registry the router was built with holds the session.
        let found = api
            .registry
            .authorize(&created.pairing, unix_now())
            .unwrap();
        assert_eq!(found.id, created.id);
        assert_eq!(found.client_id, "claude-code");
    }

    #[tokio::test]
    async fn post_brackets_an_ipv6_proxy_address_in_the_base_url() {
        let api = api_with(
            Some("[::1]:4000".parse().unwrap()),
            RecordingPolicy::allowing(),
            SessionRegistry::default(),
        );
        let reply = post(&api.app, r#"{"clientId":"claude-code","label":"lab"}"#).await;
        assert_eq!(reply.status, 201, "{}", reply.body);
        assert_eq!(
            reply.json::<SessionCreated>().base_url,
            "http://[::1]:4000/sandbox"
        );
    }

    #[tokio::test]
    async fn post_honours_ttl_seconds_within_60_and_86400_and_rejects_the_rest() {
        let api = api();
        for ttl in [60u64, 120, 86_400] {
            let reply = post(
                &api.app,
                &format!(r#"{{"clientId":"claude-code","label":"lab","ttlSeconds":{ttl}}}"#),
            )
            .await;
            assert_eq!(reply.status, 201, "{ttl}: {}", reply.body);
            let created: SessionCreated = reply.json();
            assert_eq!(
                created.expires_at_unix_seconds - created.created_at_unix_seconds,
                ttl
            );
        }
        for ttl in [0u64, 59, 86_401, 1_000_000] {
            let reply = post(
                &api.app,
                &format!(r#"{{"clientId":"claude-code","label":"lab","ttlSeconds":{ttl}}}"#),
            )
            .await;
            assert_eq!(reply.status, 400, "{ttl}");
            assert!(
                reply
                    .body
                    .contains("ttlSeconds must be between 60 and 86400"),
                "{}",
                reply.body
            );
        }
        assert_eq!(
            api.registry.len(unix_now()),
            3,
            "a rejected ttl registers nothing"
        );
    }

    #[tokio::test]
    async fn post_without_a_running_proxy_is_409_before_anything_else_is_checked() {
        let api = api_with(
            None,
            RecordingPolicy::allowing(),
            SessionRegistry::default(),
        );
        // Even an invalid client id gets the 409: the proxy check comes first.
        for body in [
            r#"{"clientId":"claude-code","label":"lab"}"#,
            r#"{"clientId":"bad id","label":"lab"}"#,
        ] {
            let reply = post(&api.app, body).await;
            assert_eq!(reply.status, 409, "{body}");
            assert!(
                reply
                    .body
                    .contains("the local LLM proxy is not running (daemon.llmProxy.listen)"),
                "{}",
                reply.body
            );
        }
        assert!(api.policy.seen().is_empty());
        assert_eq!(api.registry.len(unix_now()), 0);
    }

    #[tokio::test]
    async fn post_rejects_an_invalid_client_id_with_400_before_the_policy_is_asked() {
        let api = api_with(
            Some("127.0.0.1:4000".parse().unwrap()),
            RecordingPolicy::refusing(StatusCode::CONFLICT, "policy says no"),
            SessionRegistry::default(),
        );
        for client_id in ["", "bad id", &"a".repeat(65)] {
            let reply = post(
                &api.app,
                &format!(r#"{{"clientId":"{client_id}","label":"lab"}}"#),
            )
            .await;
            assert_eq!(reply.status, 400, "{client_id:?}: {}", reply.body);
        }
        assert!(api.policy.seen().is_empty());
    }

    #[tokio::test]
    async fn post_rejects_an_invalid_label_with_400_before_the_policy_is_asked() {
        let api = api_with(
            Some("127.0.0.1:4000".parse().unwrap()),
            RecordingPolicy::refusing(StatusCode::CONFLICT, "policy says no"),
            SessionRegistry::default(),
        );
        let long = "x".repeat(129);
        for label in ["", "bell\\u0007", "new\\nline", long.as_str()] {
            let reply = post(
                &api.app,
                &format!(r#"{{"clientId":"claude-code","label":"{label}"}}"#),
            )
            .await;
            assert_eq!(reply.status, 400, "{label:?}: {}", reply.body);
        }
        assert!(api.policy.seen().is_empty());
        assert_eq!(api.registry.len(unix_now()), 0);
    }

    #[tokio::test]
    async fn post_checks_the_ttl_before_the_policy() {
        let api = api_with(
            Some("127.0.0.1:4000".parse().unwrap()),
            RecordingPolicy::refusing(StatusCode::CONFLICT, "policy says no"),
            SessionRegistry::default(),
        );
        let reply = post(
            &api.app,
            r#"{"clientId":"claude-code","label":"lab","ttlSeconds":5}"#,
        )
        .await;
        assert_eq!(reply.status, 400);
        assert!(api.policy.seen().is_empty());
    }

    #[tokio::test]
    async fn post_passes_the_policy_refusal_through_and_registers_nothing() {
        for (status, message) in [
            (
                StatusCode::CONFLICT,
                "client id not-allowed is not in llmGateway.authentication.allowedClientIds",
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not read the configuration",
            ),
        ] {
            let api = api_with(
                Some("127.0.0.1:4000".parse().unwrap()),
                RecordingPolicy::refusing(status, message),
                SessionRegistry::default(),
            );
            let reply = post(&api.app, r#"{"clientId":"not-allowed","label":"lab"}"#).await;
            assert_eq!(reply.status, status.as_u16());
            assert!(reply.body.contains(message), "{}", reply.body);
            assert_eq!(api.policy.seen(), vec!["not-allowed".to_owned()]);
            assert_eq!(api.registry.len(unix_now()), 0);
        }
    }

    #[tokio::test]
    async fn post_is_429_when_64_sessions_exist_and_the_policy_is_asked_first() {
        let api = api();
        let now = unix_now();
        for n in 0..64 {
            register_ok(
                &api.registry,
                "claude-code",
                &format!("s{n}"),
                Duration::from_secs(3_600),
                now,
            );
        }
        let reply = post(
            &api.app,
            r#"{"clientId":"claude-code","label":"one too many"}"#,
        )
        .await;
        assert_eq!(reply.status, 429, "{}", reply.body);
        assert!(
            reply
                .body
                .contains("too many sessions (64); revoke unused ones"),
            "{}",
            reply.body
        );
        assert_eq!(api.policy.seen(), vec!["claude-code".to_owned()]);

        // With a refusing policy the refusal wins over the bound.
        let refusing = api_with(
            Some("127.0.0.1:4000".parse().unwrap()),
            RecordingPolicy::refusing(StatusCode::CONFLICT, "policy says no"),
            api.registry.clone(),
        );
        let reply = post(&refusing.app, r#"{"clientId":"claude-code","label":"x"}"#).await;
        assert_eq!(reply.status, 409);
    }

    #[tokio::test]
    async fn post_body_rejections_are_the_ones_axum_produces() {
        let api = api();
        let wrong_type = call(
            &api.app,
            "POST",
            "/v1/llm-proxy/sessions",
            Some(("text/plain", r#"{"clientId":"claude-code","label":"lab"}"#)),
        )
        .await;
        assert_eq!(wrong_type.status, 415);
        let malformed = post(&api.app, r#"{"clientId":"#).await;
        assert_eq!(malformed.status, 400);
        let missing_label = post(&api.app, r#"{"clientId":"claude-code"}"#).await;
        assert_eq!(missing_label.status, 422);
        let wrong_type_field = post(
            &api.app,
            r#"{"clientId":"claude-code","label":"x","ttlSeconds":"soon"}"#,
        )
        .await;
        assert_eq!(wrong_type_field.status, 422);
        assert!(api.policy.seen().is_empty());
        assert_eq!(api.registry.len(unix_now()), 0);
    }

    #[tokio::test]
    async fn post_ignores_unknown_fields() {
        let api = api();
        let reply = post(
            &api.app,
            r#"{"clientId":"claude-code","label":"lab","somethingNew":true}"#,
        )
        .await;
        assert_eq!(reply.status, 201, "{}", reply.body);
    }

    #[tokio::test]
    async fn get_lists_sessions_oldest_first_without_any_pairing() {
        let api = api();
        let empty = call(&api.app, "GET", "/v1/llm-proxy/sessions", None).await;
        assert_eq!(empty.status, 200);
        assert_eq!(empty.json::<Vec<SessionInfo>>(), vec![]);

        let now = unix_now();
        let ttl = Duration::from_secs(3_600);
        let newest = register_ok(&api.registry, "claude-code", "newest", ttl, now - 5);
        let oldest = register_ok(&api.registry, "my_tool", "oldest", ttl, now - 20);
        let middle = register_ok(&api.registry, "claude-code", "middle", ttl, now - 10);
        let created = post(&api.app, r#"{"clientId":"claude-code","label":"via api"}"#).await;
        assert_eq!(created.status, 201);
        let created: SessionCreated = created.json();

        let reply = call(&api.app, "GET", "/v1/llm-proxy/sessions", None).await;
        assert_eq!(reply.status, 200);
        assert!(
            reply.headers.contains("application/json"),
            "{}",
            reply.headers
        );
        let listed: Vec<SessionInfo> = reply.json();
        let ids: Vec<&str> = listed.iter().map(|info| info.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                oldest.id.as_str(),
                middle.id.as_str(),
                newest.id.as_str(),
                created.id.as_str()
            ]
        );
        assert_eq!(listed[0].client_id, "my_tool");
        assert_eq!(listed[0].label, "oldest");
        assert_eq!(listed[0].created_at_unix_seconds, now - 20);
        // No pairing value or key anywhere in the listing.
        let json: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
        for entry in json.as_array().unwrap() {
            assert!(entry.get("pairing").is_none(), "{entry}");
        }
        for session in [&newest, &oldest, &middle] {
            assert!(!reply.body.contains(&*session.pairing));
        }
        assert!(!reply.body.contains(&created.pairing));
    }

    #[tokio::test]
    async fn delete_revokes_with_204_and_answers_404_for_an_unknown_session() {
        let api = api();
        let session = register_ok(&api.registry, "claude-code", "lab", TTL, unix_now());
        let other = register_ok(&api.registry, "claude-code", "kept", TTL, unix_now());

        let gone = call(
            &api.app,
            "DELETE",
            &format!("/v1/llm-proxy/sessions/{}", session.id),
            None,
        )
        .await;
        assert_eq!(gone.status, 204, "{}", gone.body);
        assert!(gone.body.is_empty());
        assert!(
            api.registry
                .authorize(&session.pairing, unix_now())
                .is_none()
        );
        assert!(api.registry.authorize(&other.pairing, unix_now()).is_some());

        let again = call(
            &api.app,
            "DELETE",
            &format!("/v1/llm-proxy/sessions/{}", session.id),
            None,
        )
        .await;
        assert_eq!(again.status, 404);
        assert!(
            again.body.contains(&format!("no session {}", session.id)),
            "{}",
            again.body
        );
        let unknown = call(
            &api.app,
            "DELETE",
            "/v1/llm-proxy/sessions/0000000000000000",
            None,
        )
        .await;
        assert_eq!(unknown.status, 404);
        assert!(
            unknown.body.contains("no session 0000000000000000"),
            "{}",
            unknown.body
        );
    }

    #[tokio::test]
    async fn other_methods_on_the_session_routes_are_405() {
        let api = api();
        assert_eq!(
            call(&api.app, "PUT", "/v1/llm-proxy/sessions", None)
                .await
                .status,
            405
        );
        assert_eq!(
            call(&api.app, "GET", "/v1/llm-proxy/sessions/abc", None)
                .await
                .status,
            405
        );
    }

    #[tokio::test]
    async fn the_pairing_is_in_the_post_body_only_and_never_in_debug_output() {
        let api = api();
        let reply = post(&api.app, r#"{"clientId":"claude-code","label":"lab"}"#).await;
        let created: SessionCreated = reply.json();
        let debug = format!("{created:?}");
        assert!(debug.contains("<redacted>"), "{debug}");
        assert!(!debug.contains(&created.pairing), "{debug}");
        let listing = call(&api.app, "GET", "/v1/llm-proxy/sessions", None).await;
        assert!(!listing.body.contains(&created.pairing));
    }

    // --- the production policy: the effective configuration's rules.

    fn config_policy(yaml: &str) -> (tempfile::TempDir, ConfigPolicy) {
        let dir = tempfile::tempdir().unwrap();
        let policy = ConfigPolicy {
            config: agentdesktop_core::config::parse_daemon(yaml).expect("test configuration"),
            state_dir: dir.path().to_owned(),
        };
        (dir, policy)
    }

    const CONTROLLER_JWT: &str = "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: controllerJwt\n    audience: agentgateway\n    allowedClientIds: [claude-code, copilot-cli]\n";
    const OIDC: &str = "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: oidc\n    issuer: https://login.example.com\n    clientId: agentdesktop\n";

    #[tokio::test]
    async fn config_policy_admits_a_client_id_the_controller_policy_allows() {
        let (_dir, policy) = config_policy(CONTROLLER_JWT);
        assert_eq!(policy.check("claude-code").await, Ok(()));
        assert_eq!(policy.check("copilot-cli").await, Ok(()));
    }

    #[tokio::test]
    async fn config_policy_refuses_a_client_id_missing_from_allowed_client_ids() {
        let (_dir, policy) = config_policy(CONTROLLER_JWT);
        let (status, message) = policy.check("codex").await.unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            message,
            "client id codex is not in llmGateway.authentication.allowedClientIds"
        );
    }

    #[tokio::test]
    async fn config_policy_does_not_check_client_ids_with_oidc_or_without_authentication() {
        let (_dir, policy) = config_policy(OIDC);
        assert_eq!(policy.check("anything-goes").await, Ok(()));
        let (_dir, policy) = config_policy("llmGateway:\n  url: https://gateway.example.com\n");
        assert_eq!(policy.check("anything-goes").await, Ok(()));
        let (_dir, policy) = config_policy("{}");
        assert_eq!(policy.check("anything-goes").await, Ok(()));
    }

    #[tokio::test]
    async fn config_policy_refuses_a_program_in_subscription_mode() {
        let (_dir, policy) = config_policy(&format!(
            "{OIDC}programs:\n  claudeCode:\n    auth: subscription\n  claudeDesktop:\n    auth: subscription\n"
        ));
        for client_id in ["claude-code", "claude-desktop"] {
            let (status, message) = policy.check(client_id).await.unwrap_err();
            assert_eq!(status, StatusCode::CONFLICT, "{client_id}");
            assert_eq!(
                message, "subscription mode is not supported on the sandbox route",
                "{client_id}"
            );
        }
        let (_dir, policy) = config_policy(&format!("{OIDC}programs:\n  claudeCode: {{}}\n"));
        assert_eq!(policy.check("claude-code").await, Ok(()));
    }
}
