//! SSH agent proxy for git over ssh in containment.
//!
//! The broker core speaks the ssh-agent wire protocol over any
//! `AsyncRead + AsyncWrite` transport (AC 2), so every policy path is
//! unit-testable off-Windows; the Windows named-pipe transport lives in
//! [`pipe`]. Only identity-list and sign requests are forwarded to the
//! user's agent (D3); everything else is answered `SSH_AGENT_FAILURE`
//! locally. The key filter (D4) is the blast-radius control: a brokered
//! signature is not destination-bound.

use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::Result;
use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::llm_proxy::REVOCATION_POLL;
use crate::llm_proxy::sessions::SessionRegistry;

pub mod pipe;

/// Maximum agent message size (framing length): 256 KiB.
pub const MAX_AGENT_MESSAGE: usize = 256 * 1024;

/// Maximum concurrent pipe connections per launch.
// Used by the Windows pipe server; off Windows only tests drive it.
#[cfg_attr(not(windows), allow(dead_code))]
pub const MAX_AGENT_CONNECTIONS: usize = 8;

/// Maximum concurrent connections per TCP leg.
pub const MAX_LEG_CONNECTIONS: usize = 8;

/// Bound on one upstream agent round trip. Generous on purpose: a sign can
/// wait on user presence (a FIDO touch, a confirm-on-use key).
pub(crate) const AGENT_ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(30);

/// SSH agent protocol message types.
pub const SSH_AGENT_FAILURE: u8 = 5;
pub const SSH_AGENTC_REQUEST_IDENTITIES: u8 = 11;
pub const SSH_AGENT_IDENTITIES_ANSWER: u8 = 12;
pub const SSH_AGENTC_SIGN_REQUEST: u8 = 13;
// A wire name pinned by the contract; the broker passes sign replies
// through without matching on the type, so only tests reference it.
#[allow(dead_code)]
pub const SSH_AGENT_SIGN_RESPONSE: u8 = 14;

/// Refused operation codes (add/remove, smartcard, lock/unlock, extensions).
pub const SSH_AGENTC_ADD_IDENTITY: u8 = 17;
pub const SSH_AGENTC_REMOVE_IDENTITY: u8 = 18;
pub const SSH_AGENTC_REMOVE_ALL_IDENTITIES: u8 = 19;
pub const SSH_AGENTC_ADD_SMARTCARD_KEY: u8 = 20;
pub const SSH_AGENTC_REMOVE_SMARTCARD_KEY: u8 = 21;
pub const SSH_AGENTC_LOCK: u8 = 22;
pub const SSH_AGENTC_UNLOCK: u8 = 23;
pub const SSH_AGENTC_ADD_ID_CONSTRAINED: u8 = 25;
pub const SSH_AGENTC_ADD_SMARTCARD_KEY_CONSTRAINED: u8 = 26;
pub const SSH_AGENTC_EXTENSION: u8 = 27;

/// Key filtering policy for the ssh-agent proxy.
#[derive(Debug, Clone)]
pub(crate) struct AgentPolicy {
    /// Allowed key fingerprints. None = all keys ("*"), Some(empty) = deny-by-default, Some(set) = filter to set.
    allowed_keys: Option<BTreeSet<String>>,
}

impl AgentPolicy {
    /// Create a policy from the config keys list.
    /// Empty keys → Some(∅) (deny all), ["*"] → None (all keys), otherwise Some(set).
    pub(crate) fn from_config_keys(keys: &[String]) -> Self {
        if keys.iter().any(|k| k == "*") {
            AgentPolicy { allowed_keys: None }
        } else if keys.is_empty() {
            AgentPolicy {
                allowed_keys: Some(BTreeSet::new()),
            }
        } else {
            AgentPolicy {
                allowed_keys: Some(keys.iter().cloned().collect()),
            }
        }
    }

    fn allows_key(&self, fingerprint: &str) -> bool {
        match &self.allowed_keys {
            None => true, // All keys allowed
            Some(set) => set.contains(fingerprint),
        }
    }
}

/// Upstream ssh-agent connection. `round_trip` takes ONE complete framed
/// request (4-byte length prefix included) and returns one complete framed
/// response (addendum G3); the broker validates the response frame length
/// before filtering or re-encoding.
#[async_trait]
pub(crate) trait AgentUpstream: Send + Sync {
    async fn round_trip(&self, request: &[u8]) -> Result<Vec<u8>>;
}

/// `SSH_AGENT_FAILURE` as one complete frame.
const FAILURE_FRAME: [u8; 5] = [0, 0, 0, 1, SSH_AGENT_FAILURE];

/// Seconds since the Unix epoch, for `SessionRegistry::live`.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// What a revocation-raced exact read ended with.
enum RevocableRead {
    Filled,
    /// EOF or any transport error: the connection closes silently.
    Closed,
    /// The session died while the read was pending (D7).
    Revoked,
}

/// Fills `buffer` from `client`, racing the read against the session
/// revocation poll so an idle connection closes within one
/// [`REVOCATION_POLL`] of a revoke.
async fn read_revocable<C>(
    client: &mut C,
    buffer: &mut [u8],
    sessions: &SessionRegistry,
    session_id: &str,
) -> RevocableRead
where
    C: AsyncRead + Unpin,
{
    let read = client.read_exact(buffer);
    tokio::pin!(read);
    let mut poll = tokio::time::interval(REVOCATION_POLL);
    loop {
        tokio::select! {
            result = &mut read => {
                return match result {
                    Ok(_) => RevocableRead::Filled,
                    Err(_) => RevocableRead::Closed,
                };
            }
            _ = poll.tick() => {
                if !sessions.live(session_id, now_unix()) {
                    return RevocableRead::Revoked;
                }
            }
        }
    }
}

/// What a revocation-raced write ended with.
enum RevocableWrite {
    Written,
    /// Any transport error: the connection closes silently.
    Closed,
    /// The session died before or while the write was pending (D7).
    Revoked,
}

/// Writes `bytes` to `client`, racing the write against the revocation
/// poll, so a box that stops reading cannot hold the connection past a
/// revoke. This does NOT gate the first byte on liveness: a write that
/// completes immediately is not interrupted — the pre-write liveness gate
/// for secret-carrying replies is the post-round-trip check at the call
/// site (M1 fix), which runs with no await between it and this write.
async fn write_revocable<C>(
    client: &mut C,
    bytes: &[u8],
    sessions: &SessionRegistry,
    session_id: &str,
) -> RevocableWrite
where
    C: AsyncWrite + Unpin,
{
    let write = client.write_all(bytes);
    tokio::pin!(write);
    let mut poll = tokio::time::interval(REVOCATION_POLL);
    loop {
        tokio::select! {
            biased;
            _ = poll.tick() => {
                if !sessions.live(session_id, now_unix()) {
                    return RevocableWrite::Revoked;
                }
            }
            result = &mut write => {
                return match result {
                    Ok(()) => RevocableWrite::Written,
                    Err(_) => RevocableWrite::Closed,
                };
            }
        }
    }
}

/// One per-request log line (AC 8). The emit shape deliberately differs from
/// the HTTP `SandboxRequest` one: this is not an HTTP surface.
#[allow(clippy::too_many_arguments)]
fn emit_agent_line(
    session_id: &str,
    launch_id: &str,
    op: u8,
    op_name: &str,
    decision: &str,
    key: &str,
    started: tokio::time::Instant,
    outcome: &str,
) {
    tracing::info!(
        session = %session_id,
        launch_id = %launch_id,
        op = u64::from(op),
        op_name = %op_name,
        decision = %decision,
        key = %key,
        duration_ms = started.elapsed().as_millis() as u64,
        outcome = %outcome,
        "ssh agent request"
    );
}

/// A big-endian `u32` at `cursor`, advancing it.
fn read_u32(payload: &[u8], cursor: &mut usize) -> Option<u32> {
    let bytes = payload.get(*cursor..*cursor + 4)?;
    *cursor += 4;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

/// An ssh `string` (length-prefixed bytes) at `cursor`, advancing it.
fn read_ssh_string<'p>(payload: &'p [u8], cursor: &mut usize) -> Option<&'p [u8]> {
    let length = read_u32(payload, cursor)? as usize;
    let bytes = payload.get(*cursor..*cursor + length)?;
    *cursor += length;
    Some(bytes)
}

fn push_ssh_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// The key blob of a `SIGN_REQUEST` payload (`[13][string blob]...`).
fn sign_request_key_blob(payload: &[u8]) -> Option<&[u8]> {
    let mut cursor = 1;
    read_ssh_string(payload, &mut cursor)
}

/// Re-encodes an `IDENTITIES_ANSWER` payload with only the allowed keys,
/// count field corrected (AC 2). `None` when the payload does not parse.
fn filter_identities_answer(payload: &[u8], allowed: &BTreeSet<String>) -> Option<Vec<u8>> {
    let mut cursor = 1;
    let count = read_u32(payload, &mut cursor)? as usize;
    let mut kept: Vec<(&[u8], &[u8])> = Vec::new();
    for _ in 0..count {
        let blob = read_ssh_string(payload, &mut cursor)?;
        let comment = read_ssh_string(payload, &mut cursor)?;
        if allowed.contains(&key_fingerprint(blob)) {
            kept.push((blob, comment));
        }
    }
    let mut out = vec![SSH_AGENT_IDENTITIES_ANSWER];
    out.extend_from_slice(&(kept.len() as u32).to_be_bytes());
    for (blob, comment) in kept {
        push_ssh_string(&mut out, blob);
        push_ssh_string(&mut out, comment);
    }
    Some(out)
}

/// Answers one refused op `SSH_AGENT_FAILURE` (D3) with its AC 8 log line;
/// `false` when the write failed and the connection should close.
async fn refuse_op<C>(
    client: &mut C,
    sessions: &SessionRegistry,
    session_id: &str,
    launch_id: &str,
    op: u8,
    started: tokio::time::Instant,
) -> bool
where
    C: AsyncWrite + Unpin,
{
    if !matches!(
        write_revocable(client, &FAILURE_FRAME, sessions, session_id).await,
        RevocableWrite::Written
    ) {
        return false;
    }
    emit_agent_line(
        session_id,
        launch_id,
        op,
        &format!("refused:{op}"),
        "refuse",
        "-",
        started,
        "complete",
    );
    true
}

/// A framed reply from the upstream, validated (addendum G3): the 4-byte
/// length prefix must match, be non-zero and within [`MAX_AGENT_MESSAGE`].
fn validated_reply_payload(reply: &[u8]) -> Option<&[u8]> {
    if reply.len() < 5 {
        return None;
    }
    let declared = u32::from_be_bytes(reply[..4].try_into().ok()?) as usize;
    if declared == 0 || declared > MAX_AGENT_MESSAGE || declared != reply.len() - 4 {
        return None;
    }
    Some(&reply[4..])
}

/// Broker a single agent connection between the box and the upstream agent:
/// framing per AC 2, the D3 op policy, the D4 key filter, the D7 per-request
/// session check and revocation teardown, one AC 8 log line per request.
/// A sign request whose key blob does not parse is refused locally under any
/// key policy, `["*"]` included.
// The production caller is the Windows pipe server; the broker core stays
// engine- and OS-neutral for the future unix-socket leg (AC 9).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) async fn broker_connection<C>(
    mut client: C,
    upstream: &dyn AgentUpstream,
    policy: &AgentPolicy,
    sessions: &SessionRegistry,
    session_id: &str,
    launch_id: &str,
) -> Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let mut header = [0u8; 4];
        match read_revocable(&mut client, &mut header, sessions, session_id).await {
            RevocableRead::Filled => {}
            // EOF, a truncated frame, or a dead session: close silently.
            RevocableRead::Closed | RevocableRead::Revoked => return Ok(()),
        }
        let declared = u32::from_be_bytes(header) as usize;
        if declared == 0 || declared > MAX_AGENT_MESSAGE {
            tracing::info!(
                session = %session_id,
                launch_id = %launch_id,
                length = declared as u64,
                "ssh agent frame length out of bounds; closing the connection"
            );
            return Ok(());
        }
        let mut payload = vec![0u8; declared];
        match read_revocable(&mut client, &mut payload, sessions, session_id).await {
            RevocableRead::Filled => {}
            RevocableRead::Closed | RevocableRead::Revoked => return Ok(()),
        }
        // Per-request session check (AC 2): a dead session answers nothing.
        if !sessions.live(session_id, now_unix()) {
            return Ok(());
        }
        let started = tokio::time::Instant::now();
        let op = payload[0];
        match op {
            SSH_AGENTC_REQUEST_IDENTITIES | SSH_AGENTC_SIGN_REQUEST => {}
            // D3: add/remove, smartcard, lock/unlock and extensions are
            // refused locally and never reach the user's agent; the
            // connection stays open. (Refusing 27 also refuses
            // `session-bind@openssh.com`; a client treats extension failure
            // as "agent without extension support" and proceeds.)
            SSH_AGENTC_ADD_IDENTITY
            | SSH_AGENTC_REMOVE_IDENTITY
            | SSH_AGENTC_REMOVE_ALL_IDENTITIES
            | SSH_AGENTC_ADD_SMARTCARD_KEY
            | SSH_AGENTC_REMOVE_SMARTCARD_KEY
            | SSH_AGENTC_LOCK
            | SSH_AGENTC_UNLOCK
            | SSH_AGENTC_ADD_ID_CONSTRAINED
            | SSH_AGENTC_ADD_SMARTCARD_KEY_CONSTRAINED
            | SSH_AGENTC_EXTENSION => {
                if !refuse_op(&mut client, sessions, session_id, launch_id, op, started).await {
                    return Ok(());
                }
                continue;
            }
            // Default-deny: an unknown request type is refused the same way.
            _ => {
                if !refuse_op(&mut client, sessions, session_id, launch_id, op, started).await {
                    return Ok(());
                }
                continue;
            }
        }
        let op_name = if op == SSH_AGENTC_REQUEST_IDENTITIES {
            "request_identities"
        } else {
            "sign_request"
        };
        // The D4 key filter: a sign request for a key outside the allowlist
        // is refused locally, before the upstream sees anything.
        let key = if op == SSH_AGENTC_SIGN_REQUEST {
            match sign_request_key_blob(&payload) {
                Some(blob) => key_fingerprint(blob),
                None => "-".to_owned(),
            }
        } else {
            "-".to_owned()
        };
        if op == SSH_AGENTC_SIGN_REQUEST
            && !sign_request_key_blob(&payload)
                .is_some_and(|blob| policy.allows_key(&key_fingerprint(blob)))
        {
            if !matches!(
                write_revocable(&mut client, &FAILURE_FRAME, sessions, session_id).await,
                RevocableWrite::Written
            ) {
                return Ok(());
            }
            emit_agent_line(
                session_id, launch_id, op, op_name, "refuse", &key, started, "complete",
            );
            continue;
        }
        // Forward the frame as it was read (AC 4), bounded by
        // AGENT_ROUND_TRIP_TIMEOUT (expiry answers FAILURE like any upstream
        // failure); race the round trip against revocation so an in-flight
        // request gets connection EOF, never a late answer (D7).
        let mut framed = header.to_vec();
        framed.extend_from_slice(&payload);
        let round = tokio::time::timeout(AGENT_ROUND_TRIP_TIMEOUT, upstream.round_trip(&framed));
        tokio::pin!(round);
        let mut poll = tokio::time::interval(REVOCATION_POLL);
        let reply = loop {
            tokio::select! {
                result = &mut round => {
                    break result.unwrap_or_else(|_elapsed| {
                        Err(anyhow::anyhow!("upstream agent round trip timed out"))
                    });
                }
                _ = poll.tick() => {
                    if !sessions.live(session_id, now_unix()) {
                        emit_agent_line(
                            session_id, launch_id, op, op_name, "allow", &key, started,
                            "canceled",
                        );
                        return Ok(());
                    }
                }
            }
        };
        // The round trip can complete in the same poll as a revoke (D7):
        // a dead session gets EOF, never the answer.
        if !sessions.live(session_id, now_unix()) {
            emit_agent_line(
                session_id, launch_id, op, op_name, "allow", &key, started, "canceled",
            );
            return Ok(());
        }
        let reply = match reply {
            // Upstream failure (AC 4).
            Err(_) => None,
            Ok(bytes) => {
                if validated_reply_payload(&bytes).is_none() {
                    // A reply frame out of bounds or malformed (addendum G3).
                    None
                } else if op == SSH_AGENTC_REQUEST_IDENTITIES
                    && policy.allowed_keys.is_some()
                    && bytes.get(4) == Some(&SSH_AGENT_IDENTITIES_ANSWER)
                {
                    // The D4 filter: the IDENTITIES_ANSWER is re-encoded with
                    // only the allowed keys; an answer the broker cannot
                    // parse is never passed through unfiltered. `None` policy
                    // (all keys) passes the reply through byte-identical.
                    let allowed = policy.allowed_keys.as_ref().expect("checked is_some");
                    filter_identities_answer(&bytes[4..], allowed).map(|filtered| {
                        let mut framed = (filtered.len() as u32).to_be_bytes().to_vec();
                        framed.extend_from_slice(&filtered);
                        framed
                    })
                } else {
                    Some(bytes)
                }
            }
        };
        match reply {
            Some(reply) => {
                match write_revocable(&mut client, &reply, sessions, session_id).await {
                    RevocableWrite::Written => {}
                    RevocableWrite::Closed => return Ok(()),
                    RevocableWrite::Revoked => {
                        emit_agent_line(
                            session_id, launch_id, op, op_name, "allow", &key, started, "canceled",
                        );
                        return Ok(());
                    }
                }
                emit_agent_line(
                    session_id, launch_id, op, op_name, "allow", &key, started, "complete",
                );
            }
            None => {
                if !matches!(
                    write_revocable(&mut client, &FAILURE_FRAME, sessions, session_id).await,
                    RevocableWrite::Written
                ) {
                    return Ok(());
                }
                emit_agent_line(
                    session_id,
                    launch_id,
                    op,
                    op_name,
                    "upstream_unreachable",
                    &key,
                    started,
                    "complete",
                );
            }
        }
    }
}

/// What a budget counts against its limit.
struct BudgetInner {
    live: AtomicUsize,
    limit: usize,
}

/// Shared count of live connections against a fixed limit (the per-launch
/// pipe cap, or one TCP leg's cap; addendum G1). Clones share one count; a
/// permit holds one slot until it is dropped. The cap refusal (close + the
/// AC 8 log line) happens where `try_acquire` returns `None`.
#[derive(Clone)]
pub(crate) struct ConnectionBudget {
    inner: Arc<BudgetInner>,
}

/// One slot of a `ConnectionBudget`, released on drop.
pub(crate) struct ConnectionPermit {
    inner: Arc<BudgetInner>,
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.inner.live.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ConnectionBudget {
    /// A budget admitting at most `limit` live connections.
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(BudgetInner {
                live: AtomicUsize::new(0),
                limit,
            }),
        }
    }

    /// A slot, or `None` when `limit` permits are live.
    pub(crate) fn try_acquire(&self) -> Option<ConnectionPermit> {
        self.inner
            .live
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |live| {
                (live < self.inner.limit).then_some(live + 1)
            })
            .ok()
            .map(|_| ConnectionPermit {
                inner: self.inner.clone(),
            })
    }
}

/// Pumps bytes between an accepted leg connection and its upstream until
/// both directions reach EOF, or the session is revoked or expires (then
/// both sides are closed within one [`REVOCATION_POLL`] and bytes written
/// afterwards never cross; addendum G2, D7). Holds `permit` for the
/// connection's lifetime. Dialing the upstream stays with the caller.
pub(crate) async fn pump_leg<C, U>(
    client: C,
    upstream: U,
    permit: ConnectionPermit,
    sessions: &SessionRegistry,
    session_id: &str,
) -> Result<()>
where
    C: AsyncRead + AsyncWrite + Unpin,
    U: AsyncRead + AsyncWrite + Unpin,
{
    let _permit = permit;
    let (mut client_read, mut client_write) = tokio::io::split(client);
    let (mut upstream_read, mut upstream_write) = tokio::io::split(upstream);
    // Each direction propagates its EOF as a half-close, so a request can
    // still be answered over a half-closed leg.
    let to_upstream = async {
        let _ = tokio::io::copy(&mut client_read, &mut upstream_write).await;
        let _ = upstream_write.shutdown().await;
    };
    let to_client = async {
        let _ = tokio::io::copy(&mut upstream_read, &mut client_write).await;
        let _ = client_write.shutdown().await;
    };
    let both = async {
        tokio::join!(to_upstream, to_client);
    };
    tokio::pin!(both);
    let mut poll = tokio::time::interval(REVOCATION_POLL);
    loop {
        tokio::select! {
            () = &mut both => return Ok(()),
            _ = poll.tick() => {
                // Returning drops both halves of both streams: the client
                // sees EOF and nothing the upstream writes later reaches it.
                if !sessions.live(session_id, now_unix()) {
                    return Ok(());
                }
            }
        }
    }
}

/// Connect timeout of a leg's upstream dial (AC 5).
pub(crate) const LEG_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Counts the bytes crossing a stream, for the AC 8 leg line.
struct Counted<T> {
    inner: T,
    read: Arc<AtomicUsize>,
    written: Arc<AtomicUsize>,
}

impl<T> Counted<T> {
    fn new(inner: T) -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let read = Arc::new(AtomicUsize::new(0));
        let written = Arc::new(AtomicUsize::new(0));
        (
            Self {
                inner,
                read: read.clone(),
                written: written.clone(),
            },
            read,
            written,
        )
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for Counted<T> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &poll {
            self.read
                .fetch_add(buf.filled().len() - before, Ordering::Relaxed);
        }
        poll
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for Counted<T> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let poll = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(written)) = &poll {
            self.written.fetch_add(*written, Ordering::Relaxed);
        }
        poll
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// One AC 8 leg-connection log line.
#[allow(clippy::too_many_arguments)]
fn emit_leg_line(
    session_id: &str,
    launch_id: &str,
    host: &str,
    port: u16,
    bytes_in: usize,
    bytes_out: usize,
    started: tokio::time::Instant,
    outcome: &str,
) {
    tracing::info!(
        session = %session_id,
        launch_id = %launch_id,
        host = %host,
        port = u64::from(port),
        bytes_in = bytes_in as u64,
        bytes_out = bytes_out as u64,
        duration_ms = started.elapsed().as_millis() as u64,
        outcome = %outcome,
        "ssh leg connection"
    );
}

/// The clock the launch-side accept loops check session liveness against:
/// the launch host's (`LaunchHost::now`), the same clock the session was
/// registered with. The contract-level seams (`broker_connection`,
/// `pump_leg`) read system time, which is what `LaunchHost::now` is in
/// production.
pub(crate) type SessionClock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Serves one host's loopback leg (AC 5): each accepted connection is a dumb
/// byte pump to `host:22` originated by the daemon, capped per D9, torn down
/// with the session per D7 (the listener drop closes the leg port).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn serve_leg(
    listener: tokio::net::TcpListener,
    host: String,
    port: u16,
    sessions: SessionRegistry,
    session_id: String,
    launch_id: String,
    clock: SessionClock,
) {
    let budget = ConnectionBudget::new(MAX_LEG_CONNECTIONS);
    let mut poll = tokio::time::interval(REVOCATION_POLL);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((client, _)) = accepted else { continue };
                match budget.try_acquire() {
                    // D9: the excess connection is accepted, then closed,
                    // with one log line.
                    None => tracing::info!(
                        session = %session_id,
                        launch_id = %launch_id,
                        host = %host,
                        port = u64::from(port),
                        "ssh leg connection over the cap; closed"
                    ),
                    Some(permit) => {
                        let sessions = sessions.clone();
                        let session_id = session_id.clone();
                        let launch_id = launch_id.clone();
                        let host = host.clone();
                        tokio::spawn(async move {
                            serve_leg_connection(
                                client, permit, &host, 22, &sessions, &session_id,
                                &launch_id,
                            )
                            .await;
                        });
                    }
                }
            }
            _ = poll.tick() => {
                if !sessions.live(&session_id, clock()) {
                    return;
                }
            }
        }
    }
}

/// Dials `host:22` (connect timeout [`LEG_CONNECT_TIMEOUT`]; failure closes
/// the client side) and pumps until EOF or revocation.
async fn serve_leg_connection(
    client: tokio::net::TcpStream,
    permit: ConnectionPermit,
    host: &str,
    port: u16,
    sessions: &SessionRegistry,
    session_id: &str,
    launch_id: &str,
) {
    let started = tokio::time::Instant::now();
    let upstream = tokio::time::timeout(
        LEG_CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect((host, 22u16)),
    )
    .await;
    match upstream {
        Ok(Ok(upstream)) => {
            let (counted, bytes_in, bytes_out) = Counted::new(client);
            let _ = pump_leg(counted, upstream, permit, sessions, session_id).await;
            let outcome = if sessions.live(session_id, now_unix()) {
                "complete"
            } else {
                "canceled"
            };
            emit_leg_line(
                session_id,
                launch_id,
                host,
                port,
                bytes_in.load(Ordering::Relaxed),
                bytes_out.load(Ordering::Relaxed),
                started,
                outcome,
            );
        }
        Ok(Err(_)) | Err(_) => {
            drop(permit);
            emit_leg_line(session_id, launch_id, host, port, 0, 0, started, "complete");
        }
    }
}

/// The SHA256 fingerprint of an ssh key blob, `SHA256:<base64, no padding>`,
/// as `ssh-add -l` prints it.
pub(crate) fn key_fingerprint(blob: &[u8]) -> String {
    use base64::Engine as _;
    let digest = Sha256::digest(blob);
    format!(
        "SHA256:{}",
        base64::engine::general_purpose::STANDARD_NO_PAD.encode(digest)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_policy_from_config_empty_keys_denies_all() {
        let policy = AgentPolicy::from_config_keys(&[]);
        assert!(!policy.allows_key("SHA256:abcd1234"));
        assert!(!policy.allows_key("SHA256:xyz789"));
    }

    #[test]
    fn agent_policy_from_config_wildcard_allows_all() {
        let policy = AgentPolicy::from_config_keys(&["*".to_owned()]);
        assert!(policy.allows_key("SHA256:abcd1234"));
        assert!(policy.allows_key("SHA256:xyz789"));
    }

    #[test]
    fn agent_policy_from_config_specific_keys_filters() {
        let policy = AgentPolicy::from_config_keys(&[
            "SHA256:allowed1".to_owned(),
            "SHA256:allowed2".to_owned(),
        ]);
        assert!(policy.allows_key("SHA256:allowed1"));
        assert!(policy.allows_key("SHA256:allowed2"));
        assert!(!policy.allows_key("SHA256:denied"));
    }
}

// Broker protocol tests (spec "Tests first": framing, op-code battery, key
// filter, D4 default, upstream failure, session death, log shape). Every test
// drives `broker_connection` over a tokio duplex against a scripted
// `AgentUpstream` stub; none touches a named pipe.

#[cfg(test)]
mod broker_tests {
    use super::*;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::sync::{Arc, Mutex};

    use base64::Engine as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream, duplex};
    use tokio::time::{Duration, Instant, timeout};

    const LAUNCH_ID: &str = "00c0ffee00c0ffee";
    /// Upper bound for one whole broker exchange.
    const EXCHANGE_LIMIT: Duration = Duration::from_secs(5);
    /// Revocation closes a connection within one `REVOCATION_POLL` (50 ms);
    /// the rest is slack for a loaded CI runner. A broker that waits for the
    /// upstream reply (2 s in the in-flight test) cannot meet it.
    const REVOKE_LIMIT: Duration = Duration::from_millis(1000);
    const FAILURE_FRAME: [u8; 5] = [0, 0, 0, 1, SSH_AGENT_FAILURE];

    /// A real ed25519 public key blob (generated with `ssh-keygen -t ed25519`,
    /// private half discarded) and the fingerprint `ssh-add -l` printed for it
    /// with the key loaded into an agent.
    const PINNED_KEY_B64: &str =
        "AAAAC3NzaC1lZDI1NTE5AAAAIKP1Mc1BgIC0217LXJoMqtTVkkpFN2muVsKFZyX8aQxw";
    const PINNED_KEY_FINGERPRINT: &str = "SHA256:H+vC+HiEhnhnYOIN8MIx0q11irGkwU4QHzCmJQoFbSs";
    /// Synthetic ed25519 blobs (`key_a`: key bytes 0x01..=0x20, `key_b`:
    /// 0x21..=0x40) and the fingerprints `ssh-keygen -lf` prints for them.
    const KEY_A_FINGERPRINT: &str = "SHA256:mKqU+0K8OhKmA8bBQi9Rz0Q5l7/g160hIP+rJYSTNj4";
    const KEY_B_FINGERPRINT: &str = "SHA256:yLcHRnrl/YjyiSIWUGY7IPRLbf9VEWEyVnQKRyzhxKw";
    /// Embedded in every sign request's data; must never reach the log.
    const SIGN_PAYLOAD_MARKER: &str = "SIGN-PAYLOAD-MARKER-7f3a";

    // --- wire helpers

    fn string(bytes: &[u8]) -> Vec<u8> {
        let mut out = (bytes.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(bytes);
        out
    }

    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut out = (payload.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(payload);
        out
    }

    fn msg(op: u8, body: &[u8]) -> Vec<u8> {
        let mut payload = vec![op];
        payload.extend_from_slice(body);
        frame(&payload)
    }

    fn ed25519_blob(first: u8) -> Vec<u8> {
        let key: Vec<u8> = (first..first + 32).collect();
        let mut blob = string(b"ssh-ed25519");
        blob.extend(string(&key));
        blob
    }

    fn key_a() -> Vec<u8> {
        ed25519_blob(0x01)
    }

    fn key_b() -> Vec<u8> {
        ed25519_blob(0x21)
    }

    fn request_identities() -> Vec<u8> {
        vec![SSH_AGENTC_REQUEST_IDENTITIES]
    }

    fn identities_answer(keys: &[(&[u8], &str)]) -> Vec<u8> {
        let mut payload = vec![SSH_AGENT_IDENTITIES_ANSWER];
        payload.extend_from_slice(&(keys.len() as u32).to_be_bytes());
        for (blob, comment) in keys {
            payload.extend(string(blob));
            payload.extend(string(comment.as_bytes()));
        }
        payload
    }

    fn both_keys_answer() -> Vec<u8> {
        identities_answer(&[(&key_a(), "allowed@test"), (&key_b(), "filtered@test")])
    }

    fn sign_request(blob: &[u8]) -> Vec<u8> {
        let mut data = b"session-id-and-userauth-request:".to_vec();
        data.extend_from_slice(SIGN_PAYLOAD_MARKER.as_bytes());
        let mut payload = vec![SSH_AGENTC_SIGN_REQUEST];
        payload.extend(string(blob));
        payload.extend(string(&data));
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload
    }

    fn sign_response() -> Vec<u8> {
        let mut signature = string(b"ssh-ed25519");
        signature.extend(string(&[0x5a; 64]));
        let mut payload = vec![SSH_AGENT_SIGN_RESPONSE];
        payload.extend(string(&signature));
        payload
    }

    // --- scripted upstream

    enum Reply {
        Answer(Vec<u8>),
        Fail,
        After(Duration, Vec<u8>),
        /// Returned verbatim, never framed.
        Raw(Vec<u8>),
        /// Never answers.
        Hang,
        /// Revokes the session, then answers at once.
        RevokeThen(SessionRegistry, String, Vec<u8>),
    }

    /// Scripted `AgentUpstream`: records every request payload and answers
    /// from a queue. The contract leaves open whether `round_trip` carries the
    /// 4-byte length prefix; the stub mirrors whatever the broker sends
    /// (framed request, framed reply; bare payload, bare reply) and records
    /// the bare payload either way.
    #[derive(Clone, Default)]
    struct StubUpstream {
        requests: Arc<Mutex<Vec<Vec<u8>>>>,
        replies: Arc<Mutex<VecDeque<Reply>>>,
    }

    impl StubUpstream {
        fn replying(replies: Vec<Reply>) -> Self {
            Self {
                requests: Arc::default(),
                replies: Arc::new(Mutex::new(replies.into())),
            }
        }

        fn calls(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn requests(&self) -> Vec<Vec<u8>> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn split_frame(request: &[u8]) -> (bool, Vec<u8>) {
        if request.len() >= 4 {
            let declared = u32::from_be_bytes([request[0], request[1], request[2], request[3]]);
            if declared as usize == request.len() - 4 {
                return (true, request[4..].to_vec());
            }
        }
        (false, request.to_vec())
    }

    #[async_trait]
    impl AgentUpstream for StubUpstream {
        async fn round_trip(&self, request: &[u8]) -> Result<Vec<u8>> {
            let (framed, payload) = split_frame(request);
            self.requests.lock().unwrap().push(payload);
            let next = self.replies.lock().unwrap().pop_front();
            let reply = match next {
                Some(Reply::Answer(payload)) => payload,
                Some(Reply::After(delay, payload)) => {
                    tokio::time::sleep(delay).await;
                    payload
                }
                Some(Reply::Fail) => anyhow::bail!("stub upstream: agent unreachable"),
                Some(Reply::Raw(bytes)) => return Ok(bytes),
                Some(Reply::Hang) => std::future::pending().await,
                Some(Reply::RevokeThen(sessions, id, payload)) => {
                    assert!(sessions.revoke(&id));
                    payload
                }
                None => anyhow::bail!("stub upstream: no scripted reply"),
            };
            Ok(if framed { frame(&reply) } else { reply })
        }
    }

    // --- fixture and driver

    struct Fixture {
        sessions: SessionRegistry,
        session_id: String,
        upstream: StubUpstream,
        policy: AgentPolicy,
    }

    fn now_unix() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    /// A live session, a stub answering `replies` in order, and the policy
    /// config `keys` map to (D4).
    fn fixture(keys: &[&str], replies: Vec<Reply>) -> Fixture {
        let sessions = SessionRegistry::default();
        let session = sessions
            .register(
                "claude-code",
                "ssh broker test",
                Duration::from_secs(3600),
                now_unix(),
            )
            .unwrap_or_else(|error| panic!("register a session: {error:?}"));
        let keys: Vec<String> = keys.iter().map(|key| (*key).to_owned()).collect();
        Fixture {
            sessions,
            session_id: session.id,
            upstream: StubUpstream::replying(replies),
            policy: AgentPolicy::from_config_keys(&keys),
        }
    }

    /// Runs the broker on one end of a duplex and `script` on the other,
    /// concurrently in this task, until both finish.
    async fn run<F, Fut, T>(fx: &Fixture, script: F) -> (Result<()>, T)
    where
        F: FnOnce(DuplexStream) -> Fut,
        Fut: Future<Output = T>,
    {
        let (client, server) = duplex(1 << 20);
        let broker = broker_connection(
            server,
            &fx.upstream,
            &fx.policy,
            &fx.sessions,
            &fx.session_id,
            LAUNCH_ID,
        );
        timeout(EXCHANGE_LIMIT, async {
            tokio::join!(broker, script(client))
        })
        .await
        .expect("broker and client finish within the exchange limit")
    }

    /// One whole frame, length prefix included; `None` on EOF before or
    /// inside it.
    async fn read_frame(client: &mut DuplexStream) -> Option<Vec<u8>> {
        let mut header = [0u8; 4];
        client.read_exact(&mut header).await.ok()?;
        let len = u32::from_be_bytes(header) as usize;
        let mut out = header.to_vec();
        out.resize(4 + len, 0);
        client.read_exact(&mut out[4..]).await.ok()?;
        Some(out)
    }

    /// Sends each request and reads one reply frame for it, then half-closes
    /// and returns whatever else arrives before the broker closes.
    async fn exchange(fx: &Fixture, requests: Vec<Vec<u8>>) -> (Vec<Option<Vec<u8>>>, Vec<u8>) {
        let (_result, out) = run(fx, |mut client| async move {
            let mut replies = Vec::new();
            for request in requests {
                if client.write_all(&request).await.is_err() {
                    replies.push(None);
                    continue;
                }
                replies.push(read_frame(&mut client).await);
            }
            let _ = client.shutdown().await;
            let mut trailing = Vec::new();
            let _ = client.read_to_end(&mut trailing).await;
            (replies, trailing)
        })
        .await;
        out
    }

    // --- log capture (the llm_proxy test idiom)

    #[derive(Clone, Default)]
    struct LogCapture(Arc<Mutex<Vec<u8>>>);

    struct LogWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for LogWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogCapture {
        type Writer = LogWriter;

        fn make_writer(&'a self) -> LogWriter {
            LogWriter(self.0.clone())
        }
    }

    impl LogCapture {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }

        /// Lines an operator sees (info and above).
        fn visible_lines(&self) -> Vec<String> {
            self.text()
                .lines()
                .filter(|line| {
                    line.contains(" INFO ") || line.contains(" WARN ") || line.contains(" ERROR ")
                })
                .map(str::to_owned)
                .collect()
        }

        /// The per-request agent lines (AC 8): the ones carrying `op_name`.
        fn agent_lines(&self) -> Vec<String> {
            self.text()
                .lines()
                .filter(|line| line.contains(" op_name="))
                .map(str::to_owned)
                .collect()
        }
    }

    fn capture_logs() -> (LogCapture, tracing::subscriber::DefaultGuard) {
        crate::launch::tests::pin_tracing_dispatchers();
        let capture = LogCapture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .finish();
        (capture, tracing::subscriber::set_default(subscriber))
    }

    /// The value of `key=value` in a formatted event line, without quotes.
    fn log_field(line: &str, key: &str) -> String {
        let marker = format!(" {key}=");
        let start = line
            .find(&marker)
            .unwrap_or_else(|| panic!("no field {key} in {line}"))
            + marker.len();
        line[start..]
            .split(' ')
            .next()
            .unwrap()
            .trim_matches('"')
            .to_owned()
    }

    // --- Framing (Tests-first bullet 1)

    #[tokio::test]
    async fn framing_oversize_length_closes_connection() {
        let (logs, _guard) = capture_logs();
        let fx = fixture(&["*"], vec![Reply::Answer(both_keys_answer())]);
        let (_result, received) = run(&fx, |mut client| async move {
            let header = ((MAX_AGENT_MESSAGE + 1) as u32).to_be_bytes();
            client.write_all(&header).await.expect("write the length");
            // No payload follows and the client never half-closes: only the
            // broker closing ends this read.
            let mut received = Vec::new();
            let _ = client.read_to_end(&mut received).await;
            received
        })
        .await;
        assert!(
            received.is_empty(),
            "no reply to an oversize frame: {received:?}"
        );
        assert_eq!(fx.upstream.calls(), 0);
        let visible = logs.visible_lines();
        assert_eq!(visible.len(), 1, "one log line for the close: {visible:?}");
    }

    #[tokio::test]
    async fn framing_zero_length_closes_connection() {
        let (logs, _guard) = capture_logs();
        let fx = fixture(&["*"], vec![Reply::Answer(both_keys_answer())]);
        let (_result, received) = run(&fx, |mut client| async move {
            client
                .write_all(&[0, 0, 0, 0])
                .await
                .expect("write the length");
            let mut received = Vec::new();
            let _ = client.read_to_end(&mut received).await;
            received
        })
        .await;
        assert!(
            received.is_empty(),
            "no reply to a zero-length frame: {received:?}"
        );
        assert_eq!(fx.upstream.calls(), 0);
        let visible = logs.visible_lines();
        assert_eq!(visible.len(), 1, "one log line for the close: {visible:?}");
    }

    #[tokio::test]
    async fn framing_truncated_frame_is_eof() {
        let (logs, _guard) = capture_logs();
        let fx = fixture(&["*"], vec![Reply::Answer(both_keys_answer())]);
        let (_result, received) = run(&fx, |mut client| async move {
            // Declares 10 bytes, delivers 3, then EOF.
            client
                .write_all(&[0, 0, 0, 10, SSH_AGENTC_REQUEST_IDENTITIES, 1, 2])
                .await
                .expect("write the partial frame");
            client.shutdown().await.expect("half-close");
            let mut received = Vec::new();
            let _ = client.read_to_end(&mut received).await;
            received
        })
        .await;
        assert!(
            received.is_empty(),
            "no reply to a truncated frame: {received:?}"
        );
        assert_eq!(
            fx.upstream.calls(),
            0,
            "a partial request is never forwarded"
        );
        let visible = logs.visible_lines();
        assert!(
            visible.is_empty(),
            "a truncated read closes silently: {visible:?}"
        );
    }

    // --- Op-code battery (Tests-first bullet 2)

    /// `op` is answered `[0,0,0,1,5]` without an upstream call, and the
    /// connection stays usable: a following REQUEST_IDENTITIES is forwarded
    /// (policy `*`, so the key filter plays no part).
    async fn assert_refused_locally(op: u8) {
        let answer = identities_answer(&[(&key_a(), "a@test")]);
        let fx = fixture(&["*"], vec![Reply::Answer(answer.clone())]);
        let (replies, trailing) = exchange(
            &fx,
            vec![
                msg(op, b"\x00\x00\x00\x04body"),
                frame(&request_identities()),
            ],
        )
        .await;
        assert_eq!(
            replies,
            vec![Some(FAILURE_FRAME.to_vec()), Some(frame(&answer))],
            "op {op} is answered SSH_AGENT_FAILURE locally and the connection stays open"
        );
        assert!(trailing.is_empty(), "nothing else is sent: {trailing:?}");
        assert_eq!(
            fx.upstream.requests(),
            vec![request_identities()],
            "op {op} never reaches the user's agent"
        );
    }

    #[tokio::test]
    async fn policy_add_identity_refused() {
        assert_refused_locally(SSH_AGENTC_ADD_IDENTITY).await;
    }

    #[tokio::test]
    async fn policy_remove_identity_refused() {
        assert_refused_locally(SSH_AGENTC_REMOVE_IDENTITY).await;
    }

    #[tokio::test]
    async fn policy_remove_all_identities_refused() {
        assert_refused_locally(SSH_AGENTC_REMOVE_ALL_IDENTITIES).await;
    }

    #[tokio::test]
    async fn policy_add_smartcard_key_refused() {
        assert_refused_locally(SSH_AGENTC_ADD_SMARTCARD_KEY).await;
    }

    #[tokio::test]
    async fn policy_remove_smartcard_key_refused() {
        assert_refused_locally(SSH_AGENTC_REMOVE_SMARTCARD_KEY).await;
    }

    #[tokio::test]
    async fn policy_lock_refused() {
        assert_refused_locally(SSH_AGENTC_LOCK).await;
    }

    #[tokio::test]
    async fn policy_unlock_refused() {
        assert_refused_locally(SSH_AGENTC_UNLOCK).await;
    }

    #[tokio::test]
    async fn policy_add_id_constrained_refused() {
        assert_refused_locally(SSH_AGENTC_ADD_ID_CONSTRAINED).await;
    }

    #[tokio::test]
    async fn policy_add_smartcard_key_constrained_refused() {
        assert_refused_locally(SSH_AGENTC_ADD_SMARTCARD_KEY_CONSTRAINED).await;
    }

    #[tokio::test]
    async fn policy_extension_refused() {
        assert_refused_locally(SSH_AGENTC_EXTENSION).await;
    }

    #[tokio::test]
    async fn policy_unknown_op_0_refused() {
        assert_refused_locally(0).await;
    }

    #[tokio::test]
    async fn policy_unknown_op_99_refused() {
        assert_refused_locally(99).await;
    }

    #[tokio::test]
    async fn policy_request_identities_forwarded() {
        let fx = fixture(&["*"], vec![Reply::Answer(both_keys_answer())]);
        let (replies, trailing) = exchange(&fx, vec![frame(&request_identities())]).await;
        assert_eq!(replies, vec![Some(frame(&both_keys_answer()))]);
        assert!(trailing.is_empty(), "{trailing:?}");
        assert_eq!(fx.upstream.requests(), vec![request_identities()]);
    }

    #[tokio::test]
    async fn policy_sign_request_forwarded() {
        let fx = fixture(&["*"], vec![Reply::Answer(sign_response())]);
        let (replies, trailing) = exchange(&fx, vec![frame(&sign_request(&key_a()))]).await;
        assert_eq!(replies, vec![Some(frame(&sign_response()))]);
        assert!(trailing.is_empty(), "{trailing:?}");
        assert_eq!(fx.upstream.requests(), vec![sign_request(&key_a())]);
    }

    // --- Key filter (Tests-first bullet 3)

    #[test]
    fn key_fingerprint_matches_ssh_add_l() {
        let blob = base64::engine::general_purpose::STANDARD
            .decode(PINNED_KEY_B64)
            .unwrap();
        assert_eq!(key_fingerprint(&blob), PINNED_KEY_FINGERPRINT);
        assert_eq!(key_fingerprint(&key_a()), KEY_A_FINGERPRINT);
        assert_eq!(key_fingerprint(&key_b()), KEY_B_FINGERPRINT);
    }

    #[tokio::test]
    async fn key_filter_identities_answer_re_encodes_to_allowed_only() {
        let fx = fixture(
            &[KEY_A_FINGERPRINT],
            vec![Reply::Answer(both_keys_answer())],
        );
        let (replies, trailing) = exchange(&fx, vec![frame(&request_identities())]).await;
        let expected = frame(&identities_answer(&[(&key_a(), "allowed@test")]));
        assert_eq!(replies, vec![Some(expected.clone())]);
        // The count field (bytes 5..9: after length and type) says 1.
        assert_eq!(&expected[5..9], &[0, 0, 0, 1]);
        assert!(trailing.is_empty(), "{trailing:?}");
    }

    #[tokio::test]
    async fn key_filter_sign_request_for_filtered_key_refused() {
        let fx = fixture(&[KEY_A_FINGERPRINT], vec![Reply::Answer(sign_response())]);
        let (replies, trailing) = exchange(&fx, vec![frame(&sign_request(&key_b()))]).await;
        assert_eq!(replies, vec![Some(FAILURE_FRAME.to_vec())]);
        assert!(trailing.is_empty(), "{trailing:?}");
        assert_eq!(
            fx.upstream.calls(),
            0,
            "a filtered key's sign request stays local"
        );
    }

    #[tokio::test]
    async fn key_filter_sign_request_for_allowed_key_forwarded() {
        let fx = fixture(&[KEY_A_FINGERPRINT], vec![Reply::Answer(sign_response())]);
        let (replies, trailing) = exchange(&fx, vec![frame(&sign_request(&key_a()))]).await;
        assert_eq!(replies, vec![Some(frame(&sign_response()))]);
        assert!(trailing.is_empty(), "{trailing:?}");
        assert_eq!(fx.upstream.requests(), vec![sign_request(&key_a())]);
    }

    // --- D4 default at the broker (Tests-first bullet 4).
    // (flips if Lutz changes D4)

    /// (flips if Lutz changes D4) `keys` empty: no identity is offered.
    #[tokio::test]
    async fn d4_default_keys_empty_identities_answer_re_encodes_to_zero() {
        let fx = fixture(&[], vec![Reply::Answer(both_keys_answer())]);
        let (replies, trailing) = exchange(&fx, vec![frame(&request_identities())]).await;
        assert_eq!(
            replies,
            vec![Some(vec![
                0,
                0,
                0,
                5,
                SSH_AGENT_IDENTITIES_ANSWER,
                0,
                0,
                0,
                0
            ])]
        );
        assert!(trailing.is_empty(), "{trailing:?}");
    }

    /// (flips if Lutz changes D4) `keys` empty: every sign request is refused
    /// without reaching the user's agent.
    #[tokio::test]
    async fn d4_default_keys_empty_sign_request_refused_locally() {
        let fx = fixture(&[], vec![Reply::Answer(sign_response())]);
        let (replies, trailing) = exchange(&fx, vec![frame(&sign_request(&key_a()))]).await;
        assert_eq!(replies, vec![Some(FAILURE_FRAME.to_vec())]);
        assert!(trailing.is_empty(), "{trailing:?}");
        assert_eq!(fx.upstream.calls(), 0);
    }

    /// (flips if Lutz changes D4) `keys: ["*"]`: both ops pass through
    /// byte-identical.
    #[tokio::test]
    async fn d4_default_keys_wildcard_passes_through_unfiltered() {
        let fx = fixture(
            &["*"],
            vec![
                Reply::Answer(both_keys_answer()),
                Reply::Answer(sign_response()),
            ],
        );
        let (replies, trailing) = exchange(
            &fx,
            vec![frame(&request_identities()), frame(&sign_request(&key_b()))],
        )
        .await;
        assert_eq!(
            replies,
            vec![
                Some(frame(&both_keys_answer())),
                Some(frame(&sign_response()))
            ]
        );
        assert!(trailing.is_empty(), "{trailing:?}");
        assert_eq!(
            fx.upstream.requests(),
            vec![request_identities(), sign_request(&key_b())]
        );
    }

    // --- Upstream failure (Tests-first bullet 5)

    #[tokio::test]
    async fn upstream_failure_returns_agent_failure() {
        let (logs, _guard) = capture_logs();
        let fx = fixture(&["*"], vec![Reply::Fail]);
        let (replies, trailing) = exchange(&fx, vec![frame(&request_identities())]).await;
        assert_eq!(replies, vec![Some(FAILURE_FRAME.to_vec())]);
        assert!(trailing.is_empty(), "{trailing:?}");
        assert_eq!(fx.upstream.calls(), 1);
        let lines = logs.agent_lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(
            log_field(&lines[0], "decision"),
            "upstream_unreachable",
            "{}",
            lines[0]
        );
    }

    // --- Session death (Tests-first bullet 6)

    #[tokio::test]
    async fn session_revocation_closes_an_idle_connection() {
        let answer = identities_answer(&[(&key_a(), "a@test")]);
        let fx = fixture(&["*"], vec![Reply::Answer(answer.clone())]);
        let fx_ref = &fx;
        let (_result, (first, elapsed, received)) = run(&fx, |mut client| async move {
            // The connection is live and serving before the revoke.
            client
                .write_all(&frame(&request_identities()))
                .await
                .expect("write request");
            let first = read_frame(&mut client).await;
            assert!(fx_ref.sessions.revoke(&fx_ref.session_id));
            let revoked_at = Instant::now();
            let mut received = Vec::new();
            let _ = client.read_to_end(&mut received).await;
            (first, revoked_at.elapsed(), received)
        })
        .await;
        assert_eq!(first, Some(frame(&answer)));
        assert!(
            received.is_empty(),
            "no bytes after the revoke: {received:?}"
        );
        assert!(
            elapsed < REVOKE_LIMIT,
            "closed {elapsed:?} after the revoke (poll is 50 ms)"
        );
    }

    #[tokio::test]
    async fn session_revocation_closes_connection() {
        // In-flight: the upstream answers only after 2 s; the revoke lands
        // while the sign request is pending. The client gets EOF, never the
        // late answer.
        let (logs, _guard) = capture_logs();
        let fx = fixture(
            &["*"],
            vec![Reply::After(Duration::from_secs(2), sign_response())],
        );
        let fx_ref = &fx;
        let (_result, (elapsed, received)) = run(&fx, |mut client| async move {
            client
                .write_all(&frame(&sign_request(&key_a())))
                .await
                .expect("write request");
            let deadline = Instant::now() + Duration::from_secs(1);
            while fx_ref.upstream.calls() == 0 && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            assert_eq!(
                fx_ref.upstream.calls(),
                1,
                "the request is in flight upstream"
            );
            assert!(fx_ref.sessions.revoke(&fx_ref.session_id));
            let revoked_at = Instant::now();
            let mut received = Vec::new();
            let _ = client.read_to_end(&mut received).await;
            (revoked_at.elapsed(), received)
        })
        .await;
        assert!(
            received.is_empty(),
            "no late reply after the revoke: {received:?}"
        );
        assert!(
            elapsed < REVOKE_LIMIT,
            "closed {elapsed:?} after the revoke (poll is 50 ms)"
        );
        let lines = logs.agent_lines();
        assert_eq!(
            lines.len(),
            1,
            "one line for the canceled request: {lines:?}"
        );
        assert_eq!(log_field(&lines[0], "outcome"), "canceled", "{}", lines[0]);
    }

    #[tokio::test]
    async fn session_revoked_before_request_answers_nothing_and_never_reaches_upstream() {
        let fx = fixture(&["*"], vec![Reply::Answer(both_keys_answer())]);
        assert!(fx.sessions.revoke(&fx.session_id));
        let (_result, received) = run(&fx, |mut client| async move {
            let _ = client.write_all(&frame(&request_identities())).await;
            let mut received = Vec::new();
            let _ = client.read_to_end(&mut received).await;
            received
        })
        .await;
        assert!(
            received.is_empty(),
            "a dead session answers nothing: {received:?}"
        );
        assert_eq!(fx.upstream.calls(), 0);
    }

    // --- Delta fix round (reviews/PR-5-delta-opus.md, sections 2 and 4)

    #[tokio::test]
    async fn session_revoked_while_round_trip_completes_gets_no_reply() {
        let (logs, _guard) = capture_logs();
        let mut fx = fixture(&["*"], vec![]);
        fx.upstream = StubUpstream::replying(vec![Reply::RevokeThen(
            fx.sessions.clone(),
            fx.session_id.clone(),
            sign_response(),
        )]);
        let (_result, received) = run(&fx, |mut client| async move {
            client
                .write_all(&frame(&sign_request(&key_a())))
                .await
                .expect("write the sign request");
            let mut received = Vec::new();
            let _ = client.read_to_end(&mut received).await;
            received
        })
        .await;
        assert_eq!(fx.upstream.calls(), 1, "the request reached the upstream");
        assert!(
            received.is_empty(),
            "a revoked session gets EOF, never the sign reply: {received:?}"
        );
        let lines = logs.agent_lines();
        assert_eq!(
            lines.len(),
            1,
            "one line for the canceled request: {lines:?}"
        );
        assert_eq!(log_field(&lines[0], "outcome"), "canceled", "{}", lines[0]);
    }

    #[tokio::test]
    async fn session_revocation_ends_a_broker_blocked_on_a_box_that_stops_reading() {
        let fx = fixture(&["*"], vec![]);
        let (mut client, server) = duplex(64);
        let upstream = fx.upstream.clone();
        let policy = fx.policy.clone();
        let sessions = fx.sessions.clone();
        let session_id = fx.session_id.clone();
        let broker = tokio::spawn(async move {
            broker_connection(
                server,
                &upstream,
                &policy,
                &sessions,
                &session_id,
                LAUNCH_ID,
            )
            .await
        });
        // 20 refused frames over a 64-byte duplex: the broker consumes 13
        // before its reply buffer fills, and the remaining 35 bytes fit the
        // request buffer, so the write_all itself completes.
        let frames: Vec<u8> = (0..20).flat_map(|_| msg(SSH_AGENTC_LOCK, b"")).collect();
        timeout(EXCHANGE_LIMIT, client.write_all(&frames))
            .await
            .expect("the frames fit the buffers")
            .expect("write the frames");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !broker.is_finished(),
            "the broker is blocked writing to a box that stopped reading"
        );
        assert!(fx.sessions.revoke(&fx.session_id));
        let result = timeout(REVOKE_LIMIT, broker)
            .await
            .expect("the revoke ends the blocked broker within the limit");
        assert!(matches!(result, Ok(Ok(()))), "{result:?}");
        assert_eq!(fx.upstream.calls(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn upstream_round_trip_timeout_answers_failure() {
        let (logs, _guard) = capture_logs();
        let fx = fixture(&["*"], vec![Reply::Hang]);
        let (mut client, server) = duplex(1 << 20);
        let broker = broker_connection(
            server,
            &fx.upstream,
            &fx.policy,
            &fx.sessions,
            &fx.session_id,
            LAUNCH_ID,
        );
        let script = async move {
            client
                .write_all(&frame(&request_identities()))
                .await
                .expect("write the request");
            let reply = read_frame(&mut client).await;
            client.shutdown().await.expect("half-close");
            reply
        };
        let started = Instant::now();
        let (_result, reply) = timeout(AGENT_ROUND_TRIP_TIMEOUT * 2, async {
            tokio::join!(broker, script)
        })
        .await
        .expect("a regression fails here instead of hanging");
        let elapsed = started.elapsed();
        assert_eq!(reply, Some(FAILURE_FRAME.to_vec()));
        assert!(
            elapsed >= AGENT_ROUND_TRIP_TIMEOUT
                && elapsed < AGENT_ROUND_TRIP_TIMEOUT + Duration::from_secs(1),
            "the timeout bounds the round trip: {elapsed:?}"
        );
        let lines = logs.agent_lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(
            log_field(&lines[0], "decision"),
            "upstream_unreachable",
            "{}",
            lines[0]
        );
        assert_eq!(log_field(&lines[0], "outcome"), "complete", "{}", lines[0]);
    }

    #[tokio::test]
    async fn key_filter_malformed_sign_request_refused_locally() {
        let (logs, _guard) = capture_logs();
        let fx = fixture(&["*"], vec![Reply::Answer(sign_response())]);
        // The blob claims 255 bytes and carries none.
        let (replies, trailing) =
            exchange(&fx, vec![msg(SSH_AGENTC_SIGN_REQUEST, &[0, 0, 0, 0xff])]).await;
        assert_eq!(replies, vec![Some(FAILURE_FRAME.to_vec())]);
        assert!(trailing.is_empty(), "{trailing:?}");
        assert_eq!(fx.upstream.calls(), 0);
        let lines = logs.agent_lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(log_field(&lines[0], "decision"), "refuse", "{}", lines[0]);
        assert_eq!(log_field(&lines[0], "key"), "-", "{}", lines[0]);
    }

    #[tokio::test]
    async fn key_filter_unparseable_identities_answer_is_refused_not_passed_through() {
        let mut answer = both_keys_answer();
        // The count field says 3, the payload carries two entries.
        answer[1..5].copy_from_slice(&3u32.to_be_bytes());
        let fx = fixture(&[KEY_A_FINGERPRINT], vec![Reply::Answer(answer)]);
        let (replies, trailing) = exchange(&fx, vec![frame(&request_identities())]).await;
        assert_eq!(replies, vec![Some(FAILURE_FRAME.to_vec())]);
        assert!(trailing.is_empty(), "{trailing:?}");
        assert_eq!(fx.upstream.calls(), 1);
    }

    #[tokio::test]
    async fn upstream_reply_over_cap_answers_failure() {
        let mut payload = both_keys_answer();
        payload.resize(MAX_AGENT_MESSAGE + 1, 0);
        let fx = fixture(&["*"], vec![Reply::Raw(frame(&payload))]);
        let (replies, trailing) = exchange(&fx, vec![frame(&request_identities())]).await;
        assert_eq!(replies, vec![Some(FAILURE_FRAME.to_vec())]);
        assert!(trailing.is_empty(), "{trailing:?}");
    }

    #[tokio::test]
    async fn upstream_reply_length_mismatch_answers_failure() {
        let fx = fixture(
            &["*"],
            vec![
                // Declares 1, carries 5.
                Reply::Raw(vec![0, 0, 0, 1, SSH_AGENT_IDENTITIES_ANSWER, 0, 0, 0, 0]),
                // Declares 10, carries 5.
                Reply::Raw(vec![0, 0, 0, 10, SSH_AGENT_IDENTITIES_ANSWER, 0, 0, 0, 0]),
            ],
        );
        let (replies, trailing) = exchange(
            &fx,
            vec![frame(&request_identities()), frame(&request_identities())],
        )
        .await;
        assert_eq!(
            replies,
            vec![Some(FAILURE_FRAME.to_vec()), Some(FAILURE_FRAME.to_vec())]
        );
        assert!(trailing.is_empty(), "{trailing:?}");
    }

    #[tokio::test]
    async fn framing_exact_max_length_accepted() {
        let answer = identities_answer(&[(&key_a(), "a@test")]);
        let mut request = request_identities();
        request.resize(MAX_AGENT_MESSAGE, 0);
        let fx = fixture(&["*"], vec![Reply::Answer(answer.clone())]);
        let (replies, trailing) = exchange(&fx, vec![frame(&request)]).await;
        assert_eq!(replies, vec![Some(frame(&answer))]);
        assert!(trailing.is_empty(), "{trailing:?}");
        assert_eq!(fx.upstream.requests(), vec![request]);
    }

    // --- Log shape per decision (Tests-first bullet 10)

    fn assert_common_shape(line: &str, session: &str, op: u8) {
        assert_eq!(log_field(line, "session"), session, "{line}");
        assert_eq!(log_field(line, "launch_id"), LAUNCH_ID, "{line}");
        assert_eq!(log_field(line, "op"), op.to_string(), "{line}");
        log_field(line, "duration_ms")
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("duration_ms is an integer: {line}"));
        // Not an HTTP surface: none of the PR 4 emit's HTTP fields.
        for http_field in ["client_id", "method", "path", "status", "code"] {
            assert!(
                !line.contains(&format!(" {http_field}=")),
                "{http_field} must not appear: {line}"
            );
        }
    }

    #[tokio::test]
    async fn log_shape_allow() {
        let (logs, _guard) = capture_logs();
        let fx = fixture(
            &[KEY_A_FINGERPRINT],
            vec![
                Reply::Answer(both_keys_answer()),
                Reply::Answer(sign_response()),
            ],
        );
        exchange(
            &fx,
            vec![frame(&request_identities()), frame(&sign_request(&key_a()))],
        )
        .await;
        let lines = logs.agent_lines();
        assert_eq!(lines.len(), 2, "one line per request: {lines:?}");
        let identities = &lines[0];
        assert_common_shape(identities, &fx.session_id, SSH_AGENTC_REQUEST_IDENTITIES);
        assert_eq!(
            log_field(identities, "op_name"),
            "request_identities",
            "{identities}"
        );
        assert_eq!(log_field(identities, "decision"), "allow", "{identities}");
        assert_eq!(log_field(identities, "key"), "-", "{identities}");
        assert_eq!(log_field(identities, "outcome"), "complete", "{identities}");
        let sign = &lines[1];
        assert_common_shape(sign, &fx.session_id, SSH_AGENTC_SIGN_REQUEST);
        assert_eq!(log_field(sign, "op_name"), "sign_request", "{sign}");
        assert_eq!(log_field(sign, "decision"), "allow", "{sign}");
        assert_eq!(log_field(sign, "key"), KEY_A_FINGERPRINT, "{sign}");
        assert_eq!(log_field(sign, "outcome"), "complete", "{sign}");
        assert!(
            !logs.text().contains(SIGN_PAYLOAD_MARKER),
            "the sign payload never reaches the log"
        );
    }

    #[tokio::test]
    async fn log_shape_refuse() {
        let (logs, _guard) = capture_logs();
        let fx = fixture(&[KEY_A_FINGERPRINT], vec![]);
        exchange(
            &fx,
            vec![
                msg(SSH_AGENTC_ADD_IDENTITY, b"\x00\x00\x00\x04body"),
                frame(&sign_request(&key_b())),
            ],
        )
        .await;
        let lines = logs.agent_lines();
        assert_eq!(lines.len(), 2, "one line per request: {lines:?}");
        let refused_op = &lines[0];
        assert_common_shape(refused_op, &fx.session_id, SSH_AGENTC_ADD_IDENTITY);
        assert_eq!(
            log_field(refused_op, "op_name"),
            "refused:17",
            "{refused_op}"
        );
        assert_eq!(log_field(refused_op, "decision"), "refuse", "{refused_op}");
        assert_eq!(log_field(refused_op, "key"), "-", "{refused_op}");
        assert_eq!(log_field(refused_op, "outcome"), "complete", "{refused_op}");
        // A sign request for a filtered key: refused, and the line names the
        // key that was asked for.
        let filtered = &lines[1];
        assert_common_shape(filtered, &fx.session_id, SSH_AGENTC_SIGN_REQUEST);
        assert_eq!(log_field(filtered, "decision"), "refuse", "{filtered}");
        assert_eq!(log_field(filtered, "key"), KEY_B_FINGERPRINT, "{filtered}");
        assert_eq!(log_field(filtered, "outcome"), "complete", "{filtered}");
        assert!(
            !logs.text().contains(SIGN_PAYLOAD_MARKER),
            "the sign payload never reaches the log"
        );
    }

    #[tokio::test]
    async fn log_shape_upstream_unreachable() {
        let (logs, _guard) = capture_logs();
        let fx = fixture(&["*"], vec![Reply::Fail]);
        exchange(&fx, vec![frame(&request_identities())]).await;
        let lines = logs.agent_lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line = &lines[0];
        assert_common_shape(line, &fx.session_id, SSH_AGENTC_REQUEST_IDENTITIES);
        assert_eq!(log_field(line, "op_name"), "request_identities", "{line}");
        assert_eq!(
            log_field(line, "decision"),
            "upstream_unreachable",
            "{line}"
        );
        assert_eq!(log_field(line, "key"), "-", "{line}");
        assert_eq!(log_field(line, "outcome"), "complete", "{line}");
    }

    // --- Connection cap (Tests-first bullet 7, addendum G1: ConnectionBudget)

    #[test]
    fn connection_budget_grants_up_to_the_limit_then_refuses() {
        let budget = ConnectionBudget::new(MAX_AGENT_CONNECTIONS);
        let permits: Vec<ConnectionPermit> = (0..MAX_AGENT_CONNECTIONS)
            .map(|i| {
                budget
                    .try_acquire()
                    .unwrap_or_else(|| panic!("permit {i} within the limit"))
            })
            .collect();
        assert!(budget.try_acquire().is_none(), "the 9th is refused");
        assert!(
            budget.try_acquire().is_none(),
            "and stays refused while 8 are live"
        );
        drop(permits);
    }

    #[test]
    fn connection_budget_dropping_a_permit_frees_a_slot() {
        let budget = ConnectionBudget::new(MAX_AGENT_CONNECTIONS);
        let mut permits: Vec<ConnectionPermit> = (0..MAX_AGENT_CONNECTIONS)
            .map(|_| budget.try_acquire().expect("within the limit"))
            .collect();
        assert!(budget.try_acquire().is_none());
        drop(permits.pop());
        let again = budget
            .try_acquire()
            .expect("a dropped permit frees its slot");
        assert!(budget.try_acquire().is_none(), "exactly one slot was freed");
        drop(again);
        drop(permits);
        let all: Vec<ConnectionPermit> = (0..MAX_AGENT_CONNECTIONS)
            .map(|_| budget.try_acquire().expect("every slot is free again"))
            .collect();
        assert!(budget.try_acquire().is_none());
        drop(all);
    }

    #[test]
    fn connection_budget_clones_share_one_count() {
        let budget = ConnectionBudget::new(MAX_LEG_CONNECTIONS);
        let clone = budget.clone();
        let mut permits = Vec::new();
        for i in 0..MAX_LEG_CONNECTIONS {
            let source = if i % 2 == 0 { &budget } else { &clone };
            permits.push(source.try_acquire().expect("within the shared limit"));
        }
        assert!(
            budget.try_acquire().is_none(),
            "the original sees the clone's permits"
        );
        assert!(
            clone.try_acquire().is_none(),
            "the clone sees the original's permits"
        );
        drop(permits.pop());
        assert!(
            clone.try_acquire().is_some(),
            "a slot freed through either is free in both"
        );
    }

    /// Acceptor modelled on the pipe server (addendum G1): one `try_acquire`
    /// per accepted connection, the connection's task holds the permit, a
    /// refused connection is closed at once. The close and its log line are
    /// the caller's (not pinned here: the seam returns `Option`).
    #[tokio::test]
    async fn connection_cap_ninth_concurrent_duplex_connection_closed_immediately() {
        let replies = (0..MAX_AGENT_CONNECTIONS)
            .map(|_| Reply::Answer(both_keys_answer()))
            .collect();
        let fx = fixture(&["*"], replies);
        let budget = ConnectionBudget::new(MAX_AGENT_CONNECTIONS);
        let mut clients = Vec::new();
        let mut tasks = Vec::new();
        for _ in 0..=MAX_AGENT_CONNECTIONS {
            let (client, server) = duplex(1 << 16);
            match budget.try_acquire() {
                Some(permit) => {
                    let upstream = fx.upstream.clone();
                    let policy = fx.policy.clone();
                    let sessions = fx.sessions.clone();
                    let session_id = fx.session_id.clone();
                    tasks.push(tokio::spawn(async move {
                        let _permit = permit;
                        broker_connection(
                            server,
                            &upstream,
                            &policy,
                            &sessions,
                            &session_id,
                            LAUNCH_ID,
                        )
                        .await
                    }));
                }
                None => drop(server),
            }
            clients.push(client);
        }
        assert_eq!(
            tasks.len(),
            MAX_AGENT_CONNECTIONS,
            "exactly the cap is admitted"
        );

        let mut ninth = clients.pop().unwrap();
        let mut received = Vec::new();
        timeout(Duration::from_secs(1), ninth.read_to_end(&mut received))
            .await
            .expect("the 9th connection is closed at once")
            .ok();
        assert!(received.is_empty(), "the 9th gets no bytes: {received:?}");

        // All 8 admitted connections are served concurrently.
        for (i, client) in clients.iter_mut().enumerate() {
            client
                .write_all(&frame(&request_identities()))
                .await
                .expect("write request");
            let reply = timeout(EXCHANGE_LIMIT, read_frame(client))
                .await
                .unwrap_or_else(|_| panic!("connection {i} answers"));
            assert_eq!(reply, Some(frame(&both_keys_answer())), "connection {i}");
        }
        assert!(
            budget.try_acquire().is_none(),
            "8 live connections hold all slots"
        );

        // Closing one admitted connection frees its slot.
        let mut first = clients.remove(0);
        first.shutdown().await.expect("half-close");
        let mut trailing = Vec::new();
        let _ = timeout(EXCHANGE_LIMIT, first.read_to_end(&mut trailing)).await;
        let finished = tasks.remove(0);
        timeout(EXCHANGE_LIMIT, finished)
            .await
            .expect("the closed connection's broker returns")
            .expect("the broker task does not panic")
            .ok();
        assert!(
            budget.try_acquire().is_some(),
            "a closed connection frees its slot"
        );

        drop(clients);
        for task in tasks {
            let _ = timeout(EXCHANGE_LIMIT, task).await;
        }
    }

    // --- Leg pump (Tests-first bullet 9, addendum G2: pump_leg)

    /// `(box side, daemon side of the client, daemon side of the upstream,
    /// far upstream end)`.
    fn leg_ends() -> (DuplexStream, DuplexStream, DuplexStream, DuplexStream) {
        let (client, client_daemon) = duplex(1 << 20);
        let (upstream_daemon, far) = duplex(1 << 20);
        (client, client_daemon, upstream_daemon, far)
    }

    /// An echo server on the far end: writes back what it reads, and shuts
    /// its write side down when its read side reaches EOF.
    async fn echo(far: DuplexStream) {
        let (mut read, mut write) = tokio::io::split(far);
        let _ = tokio::io::copy(&mut read, &mut write).await;
        let _ = write.shutdown().await;
    }

    fn leg_budget() -> ConnectionBudget {
        ConnectionBudget::new(MAX_LEG_CONNECTIONS)
    }

    #[tokio::test]
    async fn leg_pump_echoes_bytes_both_ways() {
        let fx = fixture(&["*"], vec![]);
        let budget = leg_budget();
        let permit = budget.try_acquire().expect("a slot");
        let (mut client, client_daemon, upstream_daemon, far) = leg_ends();
        let bulk: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
        let script = async {
            client.write_all(b"SSH-2.0-test\r\n").await.expect("write");
            let mut banner = [0u8; 14];
            client.read_exact(&mut banner).await.expect("echoed banner");
            assert_eq!(&banner, b"SSH-2.0-test\r\n");
            client.write_all(&bulk).await.expect("write bulk");
            let mut back = vec![0u8; bulk.len()];
            client.read_exact(&mut back).await.expect("echoed bulk");
            assert!(back == bulk, "64 KiB come back unchanged");
            client.shutdown().await.expect("half-close");
            let mut trailing = Vec::new();
            let _ = client.read_to_end(&mut trailing).await;
            trailing
        };
        let (result, _, trailing) = timeout(EXCHANGE_LIMIT, async {
            tokio::join!(
                pump_leg(
                    client_daemon,
                    upstream_daemon,
                    permit,
                    &fx.sessions,
                    &fx.session_id
                ),
                echo(far),
                script
            )
        })
        .await
        .expect("the pump ends once both directions reach EOF");
        assert!(trailing.is_empty(), "{trailing:?}");
        assert!(result.is_ok(), "{result:?}");
        assert!(
            budget.try_acquire().is_some(),
            "the permit is released when the pump returns"
        );
    }

    #[tokio::test]
    async fn leg_pump_propagates_eof_from_client_to_upstream() {
        let fx = fixture(&["*"], vec![]);
        let budget = leg_budget();
        let permit = budget.try_acquire().expect("a slot");
        let (mut client, client_daemon, upstream_daemon, mut far) = leg_ends();
        let client_side = async {
            client.write_all(b"request").await.expect("write");
            client.shutdown().await.expect("half-close");
            // The upstream can still answer after the client's EOF.
            let mut received = Vec::new();
            let _ = client.read_to_end(&mut received).await;
            received
        };
        let far_side = async {
            let mut received = Vec::new();
            far.read_to_end(&mut received)
                .await
                .expect("EOF reaches the upstream");
            far.write_all(b"answer after eof")
                .await
                .expect("write after the client's EOF");
            far.shutdown().await.expect("upstream half-close");
            received
        };
        let (_result, to_client, to_upstream) = timeout(EXCHANGE_LIMIT, async {
            tokio::join!(
                pump_leg(
                    client_daemon,
                    upstream_daemon,
                    permit,
                    &fx.sessions,
                    &fx.session_id
                ),
                client_side,
                far_side
            )
        })
        .await
        .expect("both directions finish");
        assert_eq!(to_upstream, b"request", "the client's bytes, then EOF");
        assert_eq!(
            to_client, b"answer after eof",
            "the half-closed leg still carries the answer"
        );
    }

    #[tokio::test]
    async fn leg_pump_propagates_eof_from_upstream_to_client() {
        let fx = fixture(&["*"], vec![]);
        let budget = leg_budget();
        let permit = budget.try_acquire().expect("a slot");
        let (mut client, client_daemon, upstream_daemon, mut far) = leg_ends();
        let far_side = async {
            far.write_all(b"banner").await.expect("write");
            far.shutdown().await.expect("upstream half-close");
            let mut received = Vec::new();
            let _ = far.read_to_end(&mut received).await;
            received
        };
        let client_side = async {
            let mut received = Vec::new();
            client
                .read_to_end(&mut received)
                .await
                .expect("EOF reaches the client");
            client
                .write_all(b"reply after eof")
                .await
                .expect("write after the upstream's EOF");
            client.shutdown().await.expect("client half-close");
            received
        };
        let (_result, to_upstream, to_client) = timeout(EXCHANGE_LIMIT, async {
            tokio::join!(
                pump_leg(
                    client_daemon,
                    upstream_daemon,
                    permit,
                    &fx.sessions,
                    &fx.session_id
                ),
                far_side,
                client_side
            )
        })
        .await
        .expect("both directions finish");
        assert_eq!(to_client, b"banner", "the upstream's bytes, then EOF");
        assert_eq!(
            to_upstream, b"reply after eof",
            "the half-closed leg still carries the reply"
        );
    }

    #[tokio::test]
    async fn leg_pump_revocation_tears_down_without_further_bytes() {
        let fx = fixture(&["*"], vec![]);
        let budget = leg_budget();
        let permit = budget.try_acquire().expect("a slot");
        let (mut client, client_daemon, upstream_daemon, mut far) = leg_ends();
        let fx_ref = &fx;
        let client_side = async {
            client.write_all(b"ping").await.expect("write");
            let mut pong = [0u8; 4];
            client
                .read_exact(&mut pong)
                .await
                .expect("echo before the revoke");
            assert!(fx_ref.sessions.revoke(&fx_ref.session_id));
            let revoked_at = Instant::now();
            let mut received = Vec::new();
            let _ = client.read_to_end(&mut received).await;
            (revoked_at.elapsed(), received)
        };
        let far_side = async {
            let mut ping = [0u8; 4];
            far.read_exact(&mut ping).await.expect("read");
            far.write_all(&ping).await.expect("echo");
            // Well after the revoke: these bytes must never reach the box.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = far.write_all(b"late bytes").await;
            let mut rest = Vec::new();
            let _ = far.read_to_end(&mut rest).await;
            rest
        };
        let (_result, (elapsed, received), _) = timeout(EXCHANGE_LIMIT, async {
            tokio::join!(
                pump_leg(
                    client_daemon,
                    upstream_daemon,
                    permit,
                    &fx.sessions,
                    &fx.session_id
                ),
                client_side,
                far_side
            )
        })
        .await
        .expect("revocation ends the pump and closes the upstream side");
        assert!(
            received.is_empty(),
            "no bytes after the revoke: {received:?}"
        );
        assert!(
            elapsed < REVOKE_LIMIT,
            "closed {elapsed:?} after the revoke (poll is 50 ms)"
        );
        assert!(
            budget.try_acquire().is_some(),
            "the permit is released on teardown"
        );
    }

    #[tokio::test]
    async fn leg_pump_per_leg_cap_via_the_budget() {
        let fx = fixture(&["*"], vec![]);
        let budget = leg_budget();
        let mut clients = Vec::new();
        let mut pumps = Vec::new();
        for _ in 0..MAX_LEG_CONNECTIONS {
            let permit = budget.try_acquire().expect("within the leg cap");
            let (client, client_daemon, upstream_daemon, far) = leg_ends();
            let sessions = fx.sessions.clone();
            let session_id = fx.session_id.clone();
            tokio::spawn(echo(far));
            pumps.push(tokio::spawn(async move {
                pump_leg(
                    client_daemon,
                    upstream_daemon,
                    permit,
                    &sessions,
                    &session_id,
                )
                .await
            }));
            clients.push(client);
        }
        assert!(
            budget.try_acquire().is_none(),
            "a 9th leg connection is refused"
        );

        for (i, client) in clients.iter_mut().enumerate() {
            client.write_all(b"ping").await.expect("write");
            let mut pong = [0u8; 4];
            timeout(EXCHANGE_LIMIT, client.read_exact(&mut pong))
                .await
                .unwrap_or_else(|_| panic!("leg {i} pumps"))
                .unwrap_or_else(|error| panic!("leg {i}: {error}"));
            assert_eq!(&pong, b"ping", "leg {i}");
        }

        // One leg finishing frees exactly its slot.
        let mut first = clients.remove(0);
        first.shutdown().await.expect("half-close");
        let mut trailing = Vec::new();
        let _ = timeout(EXCHANGE_LIMIT, first.read_to_end(&mut trailing)).await;
        timeout(EXCHANGE_LIMIT, pumps.remove(0))
            .await
            .expect("the finished leg's pump returns")
            .expect("the pump task does not panic")
            .ok();
        let freed = budget.try_acquire().expect("a finished leg frees its slot");
        assert!(budget.try_acquire().is_none(), "exactly one slot was freed");
        drop(freed);

        // Revocation ends the remaining legs (their clients stay open) and
        // releases every slot.
        assert!(fx.sessions.revoke(&fx.session_id));
        for pump in pumps {
            timeout(REVOKE_LIMIT, pump)
                .await
                .expect("revocation ends every live leg")
                .expect("the pump task does not panic")
                .ok();
        }
        let all: Vec<ConnectionPermit> = (0..MAX_LEG_CONNECTIONS)
            .map(|_| {
                budget
                    .try_acquire()
                    .expect("every slot is free after teardown")
            })
            .collect();
        drop(all);
        drop(clients);
    }

    #[test]
    fn key_fingerprint_sha256_prefix() {
        let fp = key_fingerprint(b"test");
        assert!(fp.starts_with("SHA256:"));
    }
}
