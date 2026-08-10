use std::{os::unix::fs::FileTypeExt, path::PathBuf};

use agentplane::{DEFAULT_CONFIG_PATH, DEFAULT_SOCKET_PATH, api, config, discovery};
use anyhow::{Context, bail};
use clap::Parser;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use tokio::net::UnixListener;

#[derive(Parser)]
#[command(about = "Agentplane privileged daemon")]
struct Args {
    #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,

    #[arg(long, default_value = DEFAULT_SOCKET_PATH)]
    socket: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config = config::load(&args.config)?;
    let discovery = discovery::discover().await;
    for agent in &discovery.agents {
        eprintln!(
            "discovered {} at {} ({})",
            agent.kind,
            agent.executable.display(),
            agent.version.as_deref().unwrap_or("unknown version")
        );
    }
    let listener = bind(&args.socket)?;
    let app = api::router(api::AppState { config, discovery });

    eprintln!("agentplaned listening on {}", args.socket.display());
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accept connection")?;
                let service = TowerToHyperService::new(app.clone());
                tokio::spawn(async move {
                    if let Err(error) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                    {
                        eprintln!("serve connection: {error}");
                    }
                });
            }
            result = tokio::signal::ctrl_c() => {
                result.context("wait for shutdown signal")?;
                break;
            }
        }
    }

    if let Err(error) = std::fs::remove_file(&args.socket) {
        eprintln!("remove socket {}: {error}", args.socket.display());
    }
    Ok(())
}

fn bind(path: &PathBuf) -> anyhow::Result<UnixListener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create socket directory {}", parent.display()))?;
    }

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            std::fs::remove_file(path)
                .with_context(|| format!("remove stale socket {}", path.display()))?;
        }
        Ok(_) => bail!("refusing to replace non-socket path {}", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("inspect socket {}", path.display()));
        }
    }

    UnixListener::bind(path).with_context(|| format!("bind socket {}", path.display()))
}
