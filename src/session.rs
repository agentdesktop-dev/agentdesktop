use std::future::Future;

use anyhow::{Context, Result};

mod managed;
mod registry;

pub use registry::SessionRegistry;

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "linux")]
pub use linux::{run_user_agent, serve_registrations};

#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub mod windows;
#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub use windows::{run_user_agent, serve_registrations};

fn spawn_runtime_worker<F, Fut>(
    name: &'static str,
    worker: F,
) -> Result<tokio::task::JoinHandle<Result<()>>>
where
    F: FnOnce() -> Result<Fut> + Send + 'static,
    Fut: Future<Output = Result<()>> + 'static,
{
    let (completed, completion) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let result = (|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("build signing worker runtime")?;
                let worker = {
                    let _guard = runtime.enter();
                    worker()?
                };
                runtime.block_on(worker)
            })();
            let _ = completed.send(result);
        })
        .context("spawn signing worker thread")?;
    Ok(tokio::spawn(async move {
        completion
            .await
            .context("signing worker thread stopped without a result")?
    }))
}

async fn run_reconnecting<Session, SessionFuture>(
    reconnect_delay: std::time::Duration,
    disconnect_event: &'static str,
    mut session: Session,
) -> Result<()>
where
    Session: FnMut() -> SessionFuture,
    SessionFuture: Future<Output = Result<()>>,
{
    loop {
        if let Err(error) = session().await {
            tracing::warn!(event = disconnect_event, reason = %error);
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            _ = tokio::time::sleep(reconnect_delay) => {}
        }
    }
}
