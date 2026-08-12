use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer};
use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::SessionConnection;
use crate::service::hbone::RotatingClientIdentity;
use crate::session::managed::{SigningKeyWorker, run_agent_session, spawn_signing_key};

pub async fn run_user_agent(
    path: &str,
    identity: RotatingClientIdentity,
    reconnect_delay: Duration,
) -> Result<()> {
    let path = path.to_owned();
    super::super::run_reconnecting(reconnect_delay, "session_agent_disconnected", move || {
        let path = path.clone();
        let identity = identity.clone();
        async move { run_user_agent_session(&path, &identity).await }
    })
    .await
}

pub(super) async fn run_user_agent_session(
    path: &str,
    identity: &RotatingClientIdentity,
) -> Result<()> {
    let mut pipe = ClientOptions::new()
        .open(path)
        .context("connect to machine forwarder session pipe")?;
    run_agent_session(&mut pipe, identity).await
}

impl SessionConnection {
    pub(super) fn into_signing_key(self, timeout: Duration) -> Result<SigningKeyWorker> {
        let pipe = detach_pipe(self.pipe)?;
        spawn_signing_key(timeout, move || {
            unsafe { NamedPipeServer::from_raw_handle(pipe.into_raw_handle()) }
                .context("attach user session pipe to signing runtime")
        })
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
