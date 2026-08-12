use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncReadExt;
use tokio::net::UnixStream;

use super::SessionConnection;
use crate::service::hbone::HboneClient;
use crate::session::SessionRegistry;
use crate::session_protocol::{Registration, write_frame};

pub(super) async fn register(
    registry: &SessionRegistry<u32>,
    connection: SessionConnection,
    registration: crate::session_protocol::self_managed::Registration,
) -> Result<(HboneClient, tokio::task::JoinHandle<Result<()>>)> {
    let client = crate::local_gateway::connect_with_capability(
        registration.endpoint,
        &registration.tunnel_token,
        registry.connect_timeout(),
    )
    .await?;
    Ok((client, connection.into_monitor()))
}

pub async fn run_local_user_agent(
    path: &Path,
    endpoint: SocketAddr,
    tunnel_token: String,
    reconnect_delay: Duration,
) -> Result<()> {
    let generation = 1;
    let path = path.to_owned();
    super::super::run_reconnecting(
        reconnect_delay,
        "local_gateway_registration_disconnected",
        move || {
            let path = path.clone();
            let tunnel_token = tunnel_token.clone();
            async move {
                let mut stream = UnixStream::connect(path)
                    .await
                    .context("connect to machine forwarder session socket")?;
                write_frame(
                    &mut stream,
                    &Registration::self_managed(
                        generation,
                        crate::session_protocol::self_managed::Registration {
                            endpoint,
                            tunnel_token,
                        },
                    ),
                )
                .await
                .context("register local Agent Gateway")?;
                let mut unexpected = [0_u8; 1];
                match stream.read(&mut unexpected).await {
                    Ok(0) => anyhow::bail!("machine forwarder closed local Gateway registration"),
                    Ok(_) => anyhow::bail!("machine forwarder sent unexpected registration data"),
                    Err(error) => Err(error).context("monitor local Gateway registration"),
                }
            }
        },
    )
    .await
}

impl SessionConnection {
    pub(super) fn into_monitor(self) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(monitor_disconnect(self.stream))
    }
}

async fn monitor_disconnect(mut stream: UnixStream) -> Result<()> {
    let mut byte = [0_u8; 1];
    match stream.read(&mut byte).await {
        Ok(0) => Ok(()),
        Ok(_) => anyhow::bail!("user session sent unexpected registration data"),
        Err(error) => Err(error).context("monitor user session socket"),
    }
}
