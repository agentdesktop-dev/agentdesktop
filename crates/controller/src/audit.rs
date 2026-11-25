//! Controller audit log, shared by the management API and the fleet API.
//!
//! Each event is appended to the append-only `admin_audit_events` table and then
//! written as one JSON line to stdout, where the deployment's log shipper forwards
//! it to a SIEM. Lines are recognised by `"schema": "agentdesktop.audit.v1"`.
//!
//! Each line also carries `stream: { id, seq }`. Every controller process picks a
//! random stream `id` at startup and numbers its lines 1, 2, 3, ... with `seq`, in
//! the order it writes them. A number is used up even when its line fails to
//! write, so a gap within one stream means a line was lost between the controller
//! and the SIEM. Detecting those gaps is the SIEM's job. A restart starts a new
//! stream at 1.
//!
//! The router records authorization denials and every mutation (an
//! `action.attempted` event before the handler acts and an `action.completed`
//! event with its outcome). Compiled-in authentication providers record their own
//! session events (sign-in, sign-out, reauthentication) with [`AuditLog::record`].
//! The fleet API records device enrollment and certificate renewal.

use std::{
    io::{self, Write},
    net::SocketAddr,
};

use anyhow::Context as _;
use axum::http::{HeaderMap, Method, header};
use serde::Serialize;
use serde_json::{Map, Value};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::database::{Database, audit::AuditColumns};

pub const SCHEMA: &str = "agentdesktop.audit.v1";

/// Every kind of audit event. The names are part of the `agentdesktop.audit.v1`
/// schema that SIEM rules match on, so adding or renaming one changes the schema.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// A permission check failed. Recorded for reads too: repeated denials are
    /// the signal of probing or of a stolen low-privilege credential.
    AuthorizationDenied,
    /// Written before a mutation runs, and the request is refused if it cannot be
    /// written (fail closed). It is the only trace of a change whose process dies
    /// before `action.completed` is written. Only mutations produce it, and those
    /// are rare operator actions, so it adds one line per change. Streamed to the
    /// SIEM only, not stored: the completion commits with the change itself.
    ActionAttempted,
    /// The mutation's outcome, linked to its attempt by `request.id`. Only an
    /// `unaudited` completion, recorded by the router, carries the HTTP status.
    ActionCompleted,
    /// Interactive sign-in. Recorded by providers, not by the controller.
    AuthLogin,
    AuthLogout,
    /// A fresh interactive sign-in the user makes when a mutation requires recent
    /// authentication (`/auth/login?reauthenticate=true`). Silent session or token
    /// refreshes are not reauthentication and must not be recorded as such.
    AuthReauthenticate,
    /// An enrollment was refused after the user's ID token was verified, for
    /// example a nonce or token subject mismatch or an invalid certificate
    /// request. Failures before that name nobody and are not recorded, because
    /// anyone can cause them on the public fleet API.
    DeviceEnrollmentDenied,
    /// A device renewed its certificate. Written before the certificate is
    /// returned; if it cannot be written, the renewal is refused (fail closed).
    DeviceCertificateRenewed,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AuthorizationDenied => "authorization.denied",
            Self::ActionAttempted => "action.attempted",
            Self::ActionCompleted => "action.completed",
            Self::AuthLogin => "auth.login",
            Self::AuthLogout => "auth.logout",
            Self::AuthReauthenticate => "auth.reauthenticate",
            Self::DeviceEnrollmentDenied => "device.enrollment_denied",
            Self::DeviceCertificateRenewed => "device.certificate_renewed",
        }
    }
}

impl Serialize for EventKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

const MAX_HEADER_LENGTH: usize = 512;
const MAX_PATH_LENGTH: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Permission granted for a mutation that is about to run.
    Authorized,
    Denied,
    Succeeded,
    Failed,
    NotFound,
    /// A mutating request finished without any mutation permission check.
    Unaudited,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authorized => "authorized",
            Self::Denied => "denied",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::NotFound => "not_found",
            Self::Unaudited => "unaudited",
        }
    }
}

impl Serialize for Outcome {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Who acted, from verified credential evidence only.
///
/// This is the audit schema's view of a
/// [`Principal`](crate::management::auth::authentication::Principal), not a copy of it. It keeps
/// a stable field layout for SIEM parsers, independent of internal refactors
/// (`Principal` is not `Serialize`). It also keeps only the identifying claims
/// (email, name) rather than the full verified claim snapshot, which would put
/// tokens' group lists and other personal data into every record.
#[derive(Clone, Debug, Serialize)]
pub struct Actor {
    pub kind: ActorKind,
    pub issuer: String,
    pub subject: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// When a management authentication provider last verified the identity
    /// claims (`email`, `name`), as Unix seconds. Set only for human and service
    /// principals of providers that track it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claims_verified_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// What kind of credential identified an [`Actor`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActorKind {
    /// Unauthenticated loopback access in local management mode.
    Local,
    Human,
    /// An app-only credential; see [`Actor::client_id`].
    Service,
    /// A device, by the certificate the controller's device CA issued it.
    Device,
    /// An operator-signed enrollment token.
    EnrollmentToken,
}

impl ActorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Human => "human",
            Self::Service => "service",
            Self::Device => "device",
            Self::EnrollmentToken => "enrollment_token",
        }
    }
}

impl Serialize for ActorKind {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl Actor {
    pub fn new(kind: ActorKind, issuer: impl Into<String>, subject: impl Into<String>) -> Self {
        Self {
            kind,
            issuer: issuer.into(),
            subject: subject.into(),
            client_id: None,
            claims_verified_at: None,
            email: None,
            name: None,
        }
    }

    /// Copies the identifying `email` and `name` claims, when they are strings.
    pub fn with_identity_claims(mut self, claims: &Map<String, Value>) -> Self {
        let text = |name| claims.get(name).and_then(Value::as_str).map(str::to_owned);
        self.email = text("email");
        self.name = text("name");
        self
    }

    /// A person who signed in with the enrollment identity provider; `claims` are
    /// the verified ID token claims.
    pub fn oidc_user(issuer: &str, subject: &str, claims: &Map<String, Value>) -> Self {
        Self::new(ActorKind::Human, issuer, subject).with_identity_claims(claims)
    }

    /// A device, identified by the certificate the controller's device CA issued it.
    pub fn device(device_id: &str) -> Self {
        Self::new(ActorKind::Device, "device-ca", device_id)
    }

    /// An operator-signed enrollment token, identified by its issuer and token ID.
    pub fn enrollment_token(issuer: &str, id: &str) -> Self {
        Self::new(ActorKind::EnrollmentToken, issuer, id)
    }
}

/// What was acted on: a policy resource class and, unless the operation applies to
/// a collection, the concrete resource ID.
#[derive(Clone, Debug, Serialize)]
pub struct Target {
    pub class: String,
    pub id: Option<String>,
    /// Display name, e.g. the hostname a device reports for itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The HTTP request an event belongs to. `forwarded_for` is recorded as received
/// and is only meaningful when a trusted proxy sets it.
#[derive(Clone, Debug, Serialize)]
pub struct RequestInfo {
    pub id: String,
    pub method: String,
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forwarded_for: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

impl RequestInfo {
    /// `path` is the HTTP path, or the method path of a gRPC call.
    pub fn new(method: &Method, path: &str, headers: &HeaderMap, peer: Option<SocketAddr>) -> Self {
        let header = |name| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(|value| truncate(value, MAX_HEADER_LENGTH))
        };
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            method: method.as_str().to_owned(),
            path: truncate(path, MAX_PATH_LENGTH),
            peer: peer.map(|peer| peer.to_string()),
            forwarded_for: header(header::HeaderName::from_static("x-forwarded-for")),
            user_agent: header(header::USER_AGENT),
        }
    }
}

fn truncate(value: &str, limit: usize) -> String {
    match value.char_indices().nth(limit) {
        Some((end, _)) => value[..end].to_owned(),
        None => value.to_owned(),
    }
}

/// One audit event. Build it with [`AuditEvent::new`] and the setters, then pass it
/// to [`AuditLog::record`].
///
/// Every record answers the audit questions:
/// - **Who**: `actor` (credential kind, issuer, subject, and email/name if known).
/// - **What**: `event` (kind of record) and `action` (the permission exercised).
/// - **Which**: `target` (resource class and, unless it is a collection, its ID).
/// - **When**: `time`, set by [`AuditLog::record`]; `seq` orders events.
/// - **Where from**: `request.peer`, `request.forwarded_for` and `request.user_agent`.
/// - **How**: `request.method` and `request.path`; `request.id` links the
///   attempted and completed records of one request.
/// - **Result and why**: `outcome`, `status` and `reason`.
#[derive(Clone, Debug)]
pub struct AuditEvent {
    pub event: EventKind,
    pub actor: Actor,
    pub outcome: Outcome,
    /// The operation: for management requests, the permission it required (e.g.
    /// `devices.delete`); for device enrollment, `devices.enroll`.
    pub action: Option<String>,
    pub target: Option<Target>,
    /// Stable error code explaining a denial or failure.
    pub reason: Option<String>,
    /// HTTP response status, for completed actions.
    pub status: Option<u16>,
    pub request: Option<RequestInfo>,
}

impl AuditEvent {
    pub fn new(event: EventKind, actor: Actor, outcome: Outcome) -> Self {
        Self {
            event,
            actor,
            outcome,
            action: None,
            target: None,
            reason: None,
            status: None,
            request: None,
        }
    }

    pub fn action(mut self, action: impl Into<String>) -> Self {
        self.action = Some(action.into());
        self
    }

    pub fn target(mut self, class: impl Into<String>, id: Option<&str>) -> Self {
        self.target = Some(Target {
            class: class.into(),
            id: id.map(str::to_owned),
            name: None,
        });
        self
    }

    /// Sets the display name of the target set by [`AuditEvent::target`].
    pub fn target_name(mut self, name: impl Into<String>) -> Self {
        if let Some(target) = &mut self.target {
            target.name = Some(name.into());
        }
        self
    }

    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }

    pub fn status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub fn request(mut self, request: RequestInfo) -> Self {
        self.request = Some(request);
        self
    }

    /// The `action.completed` event of this attempt: the same actor, action,
    /// target and request, with the given outcome.
    pub fn completed(&self, outcome: Outcome) -> Self {
        Self {
            event: EventKind::ActionCompleted,
            outcome,
            ..self.clone()
        }
    }
}

/// The hashed form of an event, stored verbatim as `event_json`.
#[derive(Serialize)]
struct StoredEvent<'a> {
    schema: &'static str,
    id: &'a str,
    time: String,
    event: EventKind,
    actor: &'a Actor,
    action: Option<&'a str>,
    target: Option<&'a Target>,
    outcome: Outcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request: Option<&'a RequestInfo>,
}

/// Numbers the lines of one stream. The counter is held while the line is
/// written, so line order always matches number order.
struct Numbering {
    id: String,
    last: std::sync::Mutex<u64>,
}

impl Numbering {
    fn new() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            last: std::sync::Mutex::new(0),
        }
    }

    /// Takes the next number, adds `stream` to `event` and writes it as one line.
    /// The number is used up even if `write` fails, which leaves a visible gap.
    fn write(
        &self,
        mut event: Map<String, Value>,
        write: impl FnOnce(&[u8]) -> io::Result<()>,
    ) -> anyhow::Result<()> {
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *last += 1;
        event.insert(
            "stream".into(),
            serde_json::json!({ "id": self.id, "seq": *last }),
        );
        let mut line = serde_json::to_vec(&event).context("serialize admin audit stream line")?;
        line.push(b'\n');
        write(&line).context("write admin audit stream")
    }
}

/// Every process writes one stdout stream, shared by all `AuditLog`s.
static STDOUT: std::sync::LazyLock<Numbering> = std::sync::LazyLock::new(Numbering::new);

#[derive(Clone)]
enum Stream {
    Stdout,
    #[cfg(test)]
    Memory(
        std::sync::Arc<Numbering>,
        std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    ),
    #[cfg(test)]
    Broken(std::sync::Arc<Numbering>),
}

impl Stream {
    async fn write(&self, event: Map<String, Value>) -> anyhow::Result<()> {
        match self {
            // Written synchronously rather than through the lossy non-blocking
            // tracing writer; one locked write keeps the line whole.
            Self::Stdout => tokio::task::spawn_blocking(move || {
                STDOUT.write(event, |line| {
                    let mut stdout = io::stdout().lock();
                    stdout.write_all(line)?;
                    stdout.flush()
                })
            })
            .await
            .context("join admin audit stream writer")?,
            #[cfg(test)]
            Self::Memory(numbering, lines) => numbering.write(event, |line| {
                lines
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(line.to_vec()).unwrap());
                Ok(())
            }),
            #[cfg(test)]
            Self::Broken(numbering) => numbering.write(event, |_| {
                Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "admin audit stream is closed",
                ))
            }),
        }
    }
}

/// Durable audit sink shared by the router and compiled-in providers. Instances
/// built from the same database share one `seq` numbering.
#[derive(Clone)]
pub struct AuditLog {
    database: Database,
    stream: Stream,
}

/// Why [`AuditLog::mutate`] made no change.
#[derive(Debug)]
pub enum ChangeError {
    /// The attempt could not be streamed, so the change did not run.
    AuditUnavailable(anyhow::Error),
    /// The change, its completion record or the commit failed; nothing was kept.
    Failed(anyhow::Error),
}

impl AuditLog {
    pub fn new(database: Database) -> Self {
        Self {
            database,
            stream: Stream::Stdout,
        }
    }

    #[cfg(test)]
    pub(crate) fn in_memory(
        database: Database,
    ) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
        let lines = std::sync::Arc::default();
        (
            Self {
                database,
                stream: Stream::Memory(
                    std::sync::Arc::new(Numbering::new()),
                    std::sync::Arc::clone(&lines),
                ),
            },
            lines,
        )
    }

    #[cfg(test)]
    pub(crate) fn broken_stream(database: Database) -> Self {
        Self {
            database,
            stream: Stream::Broken(std::sync::Arc::new(Numbering::new())),
        }
    }

    pub fn database(&self) -> &Database {
        &self.database
    }

    /// Persists the event, then streams it. An error means the event may not have
    /// reached both sinks; callers about to act must not proceed. The work runs in
    /// its own task, so a cancelled caller cannot leave a stored event unstreamed.
    pub async fn record(&self, event: AuditEvent) -> anyhow::Result<i64> {
        let log = self.clone();
        // Spawned so a cancelled caller cannot stop it between the insert and the stream.
        tokio::spawn(async move { log.write(event).await })
            .await
            .context("join admin audit writer")?
    }

    async fn write(&self, event: AuditEvent) -> anyhow::Result<i64> {
        let prepared = Prepared::new(&event)?;
        let seq = Database::append_audit_event(
            &mut *self.database.pool().acquire().await?,
            &prepared.columns(&event),
            &prepared.event_json,
        )
        .await?;
        self.stream.write(prepared.line(Some(seq))).await?;
        Ok(seq)
    }

    /// Runs an audited change: streams `attempt` (`action.attempted`), then runs
    /// `change` in a database transaction that also stores the attempt's
    /// `action.completed`, so the change and its record commit together or not at
    /// all. The completion's line is streamed after the commit.
    ///
    /// The attempt is only streamed, not stored: the stored completion is the
    /// table's record of the change, and the SIEM gets a line before the change
    /// runs. If that line cannot be written, nothing runs (fail closed).
    ///
    /// Everything runs in its own task, so once called it finishes even if the
    /// caller is cancelled (for example by a request timeout); an attempt is
    /// never left without its completion. `change` receives the open transaction
    /// and hands it back with its result and the outcome to record. If it fails
    /// or the transaction cannot commit, nothing it did is kept, and the
    /// completion is recorded as `failed` on its own (best effort).
    pub async fn mutate<T, F, Fut>(&self, attempt: AuditEvent, change: F) -> Result<T, ChangeError>
    where
        F: FnOnce(sqlx::Transaction<'static, sqlx::Any>) -> Fut + Send + 'static,
        Fut: Future<Output = anyhow::Result<(sqlx::Transaction<'static, sqlx::Any>, T, Outcome)>>
            + Send,
        T: Send + 'static,
    {
        let log = self.clone();
        // Spawned so a cancelled caller cannot stop it between the attempt, change, commit and stream.
        tokio::spawn(async move {
            let streamed = match Prepared::new(&attempt) {
                Ok(prepared) => log.stream.write(prepared.line(None)).await,
                Err(error) => Err(error),
            };
            streamed.map_err(ChangeError::AuditUnavailable)?;
            match log.commit_with_completion(&attempt, change).await {
                Ok((value, line)) => {
                    if let Err(error) = log.stream.write(line).await {
                        tracing::error!(
                            error = format!("{error:#}"),
                            "failed to stream a committed admin audit completion"
                        );
                    }
                    Ok(value)
                }
                Err(error) => {
                    if let Err(error) = log.record(attempt.completed(Outcome::Failed)).await {
                        tracing::error!(
                            error = format!("{error:#}"),
                            "failed to record a failed admin audit completion"
                        );
                    }
                    Err(ChangeError::Failed(error))
                }
            }
        })
        .await
        .map_err(|error| {
            ChangeError::Failed(anyhow::Error::new(error).context("join admin audit change"))
        })?
    }

    async fn commit_with_completion<T, F, Fut>(
        &self,
        attempt: &AuditEvent,
        change: F,
    ) -> anyhow::Result<(T, Map<String, Value>)>
    where
        F: FnOnce(sqlx::Transaction<'static, sqlx::Any>) -> Fut,
        Fut: Future<Output = anyhow::Result<(sqlx::Transaction<'static, sqlx::Any>, T, Outcome)>>,
    {
        let transaction = self.database.pool().begin().await?;
        let (mut transaction, value, outcome) = change(transaction).await?;
        let completion = attempt.completed(outcome);
        let prepared = Prepared::new(&completion)?;
        let seq = Database::append_audit_event(
            &mut transaction,
            &prepared.columns(&completion),
            &prepared.event_json,
        )
        .await?;
        transaction
            .commit()
            .await
            .context("commit admin audit transaction")?;
        Ok((value, prepared.line(Some(seq))))
    }
}

/// An event serialized for storage, with the ID and time it is recorded under.
struct Prepared {
    id: String,
    time_ms: i64,
    /// The stored form, which every streamed line starts from.
    event: Map<String, Value>,
    /// `event` serialized for the `event_json` column.
    event_json: String,
}

impl Prepared {
    fn new(event: &AuditEvent) -> anyhow::Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = OffsetDateTime::now_utc();
        let time = now.format(&Rfc3339).context("format admin audit time")?;
        let Value::Object(stored) = serde_json::to_value(StoredEvent {
            schema: SCHEMA,
            id: &id,
            time,
            event: event.event,
            actor: &event.actor,
            action: event.action.as_deref(),
            target: event.target.as_ref(),
            outcome: event.outcome,
            reason: event.reason.as_deref(),
            status: event.status,
            request: event.request.as_ref(),
        })
        .context("serialize admin audit event")?
        else {
            anyhow::bail!("admin audit event did not serialize to a JSON object");
        };
        let event_json = serde_json::to_string(&stored).context("serialize admin audit event")?;
        Ok(Self {
            id,
            time_ms: (now.unix_timestamp_nanos() / 1_000_000)
                .try_into()
                .unwrap_or(i64::MAX),
            event: stored,
            event_json,
        })
    }

    fn columns<'a>(&'a self, event: &'a AuditEvent) -> AuditColumns<'a> {
        AuditColumns {
            id: &self.id,
            time_ms: self.time_ms,
            event: event.event.as_str(),
            actor_subject: &event.actor.subject,
            action: event.action.as_deref(),
            target_id: event
                .target
                .as_ref()
                .and_then(|target| target.id.as_deref()),
            outcome: event.outcome.as_str(),
        }
    }

    /// The streamed line: the stored form plus the row's `seq`, or without one for
    /// an event that is streamed but not stored.
    fn line(self, seq: Option<i64>) -> Map<String, Value> {
        let mut line = self.event;
        if let Some(seq) = seq {
            line.insert("seq".into(), seq.into());
        }
        line
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use serde_json::{Map, Value, json};

    use super::Numbering;

    #[test]
    fn a_line_that_fails_to_write_leaves_a_gap_in_its_stream() {
        let numbering = Numbering::new();
        let mut lines = Vec::new();
        let mut capture = |line: &[u8]| {
            lines.push(serde_json::from_slice::<Value>(line).unwrap());
            Ok(())
        };
        numbering.write(Map::new(), &mut capture).unwrap();
        numbering
            .write(Map::new(), |_| Err(io::Error::other("closed")))
            .unwrap_err();
        numbering.write(Map::new(), &mut capture).unwrap();
        assert_eq!(lines[0]["stream"], json!({ "id": numbering.id, "seq": 1 }));
        assert_eq!(lines[1]["stream"], json!({ "id": numbering.id, "seq": 3 }));
        // Each process has its own stream.
        assert_ne!(Numbering::new().id, numbering.id);
    }
}
