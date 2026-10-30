//! Named pipe transport for the ssh-agent proxy on Windows (AC 3, AC 4, D8).
//!
//! The per-launch pipe is created AFTER spawn, gated on reading the box's
//! per-launch AppContainer SID from the RUNNING box process (the SID is
//! random per launch and cannot be derived from the container id); the pipe
//! NAME is fixed before spawn so the box env can carry it in
//! `SSH_AUTH_SOCK`. The pipe DACL admits the daemon's user and exactly that
//! SID — never a capability SID, never `ALL APPLICATION PACKAGES`.

#[cfg(windows)]
pub(crate) use windows_impl::start_agent_pipe_server;

#[cfg(not(windows))]
pub(crate) use stub_impl::start_agent_pipe_server;

/// Windows OpenSSH agent pipe name.
#[cfg_attr(not(windows), allow(dead_code))]
pub const OPENSSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

/// Agentdesktop ssh-agent pipe name prefix.
pub const AGENT_PIPE_PREFIX: &str = r"\\.\pipe\agentdesktop-ssh-";

#[cfg(windows)]
mod windows_impl {
    use anyhow::{Context, Result, bail};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;

    use super::super::{
        AgentPolicy, AgentUpstream, ConnectionBudget, MAX_AGENT_CONNECTIONS, MAX_AGENT_MESSAGE,
        SessionClock, broker_connection,
    };
    use super::{AGENT_PIPE_PREFIX, OPENSSH_AGENT_PIPE};
    use crate::launch::EARLY_EXIT_WATCH;
    use crate::launch::host::windows::{app_container_descendant_sid, app_container_sid};
    use crate::llm_proxy::REVOCATION_POLL;
    use crate::llm_proxy::sessions::SessionRegistry;
    use crate::windows_security::SecurityDescriptor;

    /// How often the launcher re-scans for the box process under the
    /// executor child (needs-lab-check L1).
    const SID_SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

    /// The production upstream (D2, AC 4): a fresh client connection to the
    /// user's Windows OpenSSH agent per request — stateless; LOCK/UNLOCK are
    /// refused by the broker anyway. `round_trip` carries complete frames
    /// both ways (addendum G3).
    struct OpensshAgentUpstream;

    #[async_trait::async_trait]
    impl AgentUpstream for OpensshAgentUpstream {
        async fn round_trip(&self, request: &[u8]) -> Result<Vec<u8>> {
            // CreateFile returns ERROR_PIPE_BUSY while every agent instance
            // is in use; retry like the client crate's `connect`. The
            // broker's round-trip timeout bounds the loop.
            const ERROR_PIPE_BUSY: i32 = 231;
            let mut client = loop {
                match ClientOptions::new().open(OPENSSH_AGENT_PIPE) {
                    Ok(client) => break client,
                    Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| format!("open {OPENSSH_AGENT_PIPE}"));
                    }
                }
            };
            client
                .write_all(request)
                .await
                .context("write the request")?;
            let mut header = [0u8; 4];
            client
                .read_exact(&mut header)
                .await
                .context("read the reply length")?;
            let declared = u32::from_be_bytes(header) as usize;
            if declared == 0 || declared > MAX_AGENT_MESSAGE {
                bail!("agent reply length out of bounds: {declared}");
            }
            let mut reply = header.to_vec();
            reply.resize(4 + declared, 0);
            client
                .read_exact(&mut reply[4..])
                .await
                .context("read the reply")?;
            Ok(reply)
        }
    }

    /// Binds one instance of the launch's agent pipe. `first` fails when the
    /// name exists (squat protection, AC 3). SDDL per Spike S variant c:
    /// daemon user full access, the box's per-launch AppContainer SID
    /// read/write; no integrity label needed.
    fn bind_pipe(name: &str, first: bool, sddl: &str) -> Result<NamedPipeServer> {
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        let descriptor = SecurityDescriptor::from_sddl(sddl)?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.as_ptr(),
            bInheritHandle: 0,
        };
        // SAFETY: attributes and its descriptor remain valid for the
        // duration of CreateNamedPipeW and are released only after the call
        // returns (the daemon.rs bind_named_pipe idiom).
        unsafe {
            options.create_with_security_attributes_raw(
                name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
        }
        .with_context(|| format!("bind named pipe {name}"))
    }

    /// Reads the box's AppContainer SID from the running box (the first
    /// AppContainer descendant of the executor child, polled within the
    /// early-exit watch window), creates the per-launch pipe and starts its
    /// server task. Any failure must fail the launch (AC 3, fail-closed).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_agent_pipe_server(
        launch_id: &str,
        executor_pid: u32,
        executor_start_time: u64,
        daemon_sid: Option<&str>,
        policy: AgentPolicy,
        sessions: SessionRegistry,
        session_id: String,
        clock: SessionClock,
    ) -> Result<()> {
        let Some(daemon_sid) = daemon_sid else {
            bail!("the daemon's own SID is unknown; cannot scope the agent pipe");
        };
        let deadline = tokio::time::Instant::now() + EARLY_EXIT_WATCH;
        let box_sid = loop {
            match tokio::task::spawn_blocking(move || {
                app_container_descendant_sid(executor_pid, executor_start_time)
            })
            .await
            .context("join the box process scan")?
            {
                Ok(Some(sid)) => break sid,
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(launch_id, error = %format!("{error:#}"), "box process scan failed; retrying");
                }
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("no AppContainer box process found under the executor (pid {executor_pid})");
            }
            tokio::time::sleep(SID_SCAN_INTERVAL).await;
        };
        let name = format!("{AGENT_PIPE_PREFIX}{launch_id}");
        let sddl = format!("D:(A;;GA;;;{daemon_sid})(A;;GRGW;;;{box_sid})");
        let server = bind_pipe(&name, true, &sddl)?;
        tracing::info!(
            session = %session_id,
            launch_id = %launch_id,
            pipe = %name,
            "ssh agent pipe created"
        );
        tracing::debug!(launch_id, sddl = %sddl, "ssh agent pipe DACL");
        tokio::spawn(serve_pipe(
            server,
            name,
            sddl,
            box_sid,
            policy,
            sessions,
            session_id,
            launch_id.to_owned(),
            clock,
        ));
        Ok(())
    }

    /// The pipe's accept loop: per-launch connection cap (D9, addendum G1),
    /// client verification (the client process's AppContainer SID must equal
    /// the launch's — belt and braces over the DACL), one broker per
    /// connection, teardown within one revocation poll (D7; the handles
    /// closing removes the pipe name).
    #[allow(clippy::too_many_arguments)]
    async fn serve_pipe(
        mut server: NamedPipeServer,
        name: String,
        sddl: String,
        box_sid: String,
        policy: AgentPolicy,
        sessions: SessionRegistry,
        session_id: String,
        launch_id: String,
        clock: SessionClock,
    ) {
        let budget = ConnectionBudget::new(MAX_AGENT_CONNECTIONS);
        let upstream = std::sync::Arc::new(OpensshAgentUpstream);
        let mut poll = tokio::time::interval(REVOCATION_POLL);
        loop {
            tokio::select! {
                connected = server.connect() => {
                    if connected.is_err() {
                        // The instance is unusable; replace it.
                        match bind_pipe(&name, false, &sddl) {
                            Ok(next) => server = next,
                            Err(error) => {
                                tracing::warn!(launch_id, error = %format!("{error:#}"), "ssh agent pipe instance lost");
                                break;
                            }
                        }
                        continue;
                    }
                    let connection = match bind_pipe(&name, false, &sddl) {
                        Ok(next) => std::mem::replace(&mut server, next),
                        Err(error) => {
                            tracing::warn!(launch_id, error = %format!("{error:#}"), "ssh agent pipe instance lost");
                            break;
                        }
                    };
                    if !client_is_the_box(&connection, &box_sid) {
                        tracing::info!(
                            session = %session_id,
                            launch_id = %launch_id,
                            "ssh agent pipe client is not the launch's box; closed"
                        );
                        continue;
                    }
                    match budget.try_acquire() {
                        // D9: over the cap, accepted-then-closed with one line.
                        None => tracing::info!(
                            session = %session_id,
                            launch_id = %launch_id,
                            "ssh agent pipe connection over the cap; closed"
                        ),
                        Some(permit) => {
                            let upstream = upstream.clone();
                            let policy = policy.clone();
                            let sessions = sessions.clone();
                            let session_id = session_id.clone();
                            let launch_id = launch_id.clone();
                            tokio::spawn(async move {
                                let _permit = permit;
                                let _ = broker_connection(
                                    connection,
                                    upstream.as_ref(),
                                    &policy,
                                    &sessions,
                                    &session_id,
                                    &launch_id,
                                )
                                .await;
                            });
                        }
                    }
                }
                _ = poll.tick() => {
                    if !sessions.live(&session_id, clock()) {
                        break;
                    }
                }
            }
        }
        // The server handle drops here; with every broker connection ending
        // on the same revocation, the pipe name disappears (D7).
        tracing::info!(
            session = %session_id,
            launch_id = %launch_id,
            pipe = %name,
            "ssh agent pipe torn down"
        );
    }

    /// Whether the connected client's process token carries exactly the
    /// launch's AppContainer SID.
    fn client_is_the_box(connection: &NamedPipeServer, box_sid: &str) -> bool {
        use std::os::windows::io::AsRawHandle;
        let mut pid = 0u32;
        // SAFETY: the handle is a connected pipe instance; `pid` is a valid
        // out-parameter.
        let ok = unsafe { GetNamedPipeClientProcessId(connection.as_raw_handle() as _, &mut pid) };
        if ok == 0 {
            return false;
        }
        matches!(app_container_sid(pid), Ok(Some(sid)) if sid == box_sid)
    }
}

#[cfg(not(windows))]
mod stub_impl {
    use anyhow::{Result, bail};

    use super::super::{AgentPolicy, SessionClock};
    use crate::llm_proxy::sessions::SessionRegistry;

    /// Typed-error stub: there is no agent pipe transport off Windows, and a
    /// launch with ssh hosts configured must fail closed (AC 3).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start_agent_pipe_server(
        _launch_id: &str,
        _executor_pid: u32,
        _executor_start_time: u64,
        _daemon_sid: Option<&str>,
        _policy: AgentPolicy,
        _sessions: SessionRegistry,
        _session_id: String,
        _clock: SessionClock,
    ) -> Result<()> {
        bail!("the ssh agent proxy is only available on Windows")
    }
}
