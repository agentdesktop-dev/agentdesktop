use std::future::Future;

use anyhow::{Context, Result};

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(all(target_os = "windows", target_env = "msvc"))]
pub mod windows;

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
