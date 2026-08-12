use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UnixStream;

use super::SessionConnection;
use crate::service::hbone::{HboneClient, RotatingClientIdentity};
use crate::session::SessionRegistry;
use crate::session::managed::{
    SigningKeyWorker, connect_gateway, run_agent_session, spawn_signing_key,
};
use crate::session_protocol::managed;

pub(super) async fn register(
    registry: &SessionRegistry<u32>,
    connection: SessionConnection,
    generation: u64,
    registration: managed::Registration,
) -> Result<(HboneClient, tokio::task::JoinHandle<Result<()>>)> {
    let (signing_key, monitor) = connection.into_signing_key(registry.connect_timeout())?;
    let client = connect_gateway(
        registry.endpoint(),
        registry.server_name().to_owned(),
        registry.connect_timeout(),
        registry.roots().clone(),
        generation,
        registration,
        signing_key,
    )
    .await?;
    Ok((client, monitor))
}

pub async fn run_user_agent(
    path: &std::path::Path,
    identity: RotatingClientIdentity,
    reconnect_delay: Duration,
) -> Result<()> {
    let path = path.to_owned();
    super::super::run_reconnecting(reconnect_delay, "session_agent_disconnected", move || {
        let path = path.clone();
        let identity = identity.clone();
        async move {
            let mut stream = UnixStream::connect(path)
                .await
                .context("connect to machine forwarder session socket")?;
            run_agent_session(&mut stream, &identity).await
        }
    })
    .await
}

impl SessionConnection {
    pub(super) fn into_signing_key(self, timeout: Duration) -> Result<SigningKeyWorker> {
        let stream = self
            .stream
            .into_std()
            .context("detach user session socket from service runtime")?;
        spawn_signing_key(timeout, move || {
            UnixStream::from_std(stream).context("attach user session socket to signing runtime")
        })
    }
}
