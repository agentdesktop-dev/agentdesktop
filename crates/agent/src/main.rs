use std::{
    os::unix::fs::{FileTypeExt, PermissionsExt, chown},
    path::{Path, PathBuf},
};

use agentplane_agent::{api, discovery, enrollment::EnrollmentState, reconcile, remote};
use agentplane_core::{
    DEFAULT_CONFIG_PATH, DEFAULT_SOCKET_PATH, DEFAULT_STATE_DIR, config, telemetry,
};
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

    #[arg(long, default_value = DEFAULT_STATE_DIR)]
    state_dir: PathBuf,

    #[arg(long)]
    enrollment_token: Option<String>,

    #[arg(
        long,
        default_value_os_t = reconcile::default_claude_code_managed_settings_dir()
    )]
    claude_code_managed_settings_dir: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let _log_flush = telemetry::setup_logging("info", false);
    let config = config::load(&args.config)?;
    let controller_config = config.controller.clone();
    let enrollment = EnrollmentState::new(config.controller.is_some());
    let discovery = discovery::discover().await;
    for agent in &discovery.agents {
        tracing::info!(
            kind = %agent.kind,
            executable = %agent.executable.display(),
            version = agent.version.as_deref().unwrap_or("unknown"),
            "discovered program"
        );
    }
    if let Some(controller) = config.controller.clone() {
        let enrollment_token = args
            .enrollment_token
            .or_else(|| std::env::var("AGENTPLANE_ENROLLMENT_TOKEN").ok());
        let remote_discovery = discovery.clone();
        let state_dir = args.state_dir.clone();
        let reconciler = reconcile::Reconciler::new(
            args.claude_code_managed_settings_dir.clone(),
            credential_helper_command(&args.socket)?,
        );
        let remote_enrollment = enrollment.clone();
        tokio::spawn(async move {
            if let Err(error) = remote::run(
                controller,
                remote_discovery,
                state_dir,
                enrollment_token,
                reconciler,
                remote_enrollment.clone(),
            )
            .await
            {
                remote_enrollment.set("failed").await;
                tracing::error!(error = %format!("{error:#}"), "controller integration disabled");
            }
        });
    }
    let listener = bind(&args.socket)?;
    let app = api::router(api::AppState {
        config,
        discovery,
        enrollment,
        controller: controller_config,
        state_dir: args.state_dir,
    });

    tracing::info!(socket = %args.socket.display(), "agent daemon listening");
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
                        tracing::warn!(%error, "failed to serve local API connection");
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
        tracing::warn!(%error, socket = %args.socket.display(), "failed to remove socket");
    }
    Ok(())
}

fn credential_helper_command(socket: &Path) -> anyhow::Result<String> {
    let executable = std::env::current_exe()
        .context("locate agentplaned executable")?
        .with_file_name(format!("agentplane{}", std::env::consts::EXE_SUFFIX));
    Ok(format!(
        "{} --socket {} credential",
        shell_quote(&executable.to_string_lossy()),
        shell_quote(&socket.to_string_lossy())
    ))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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

    let listener =
        UnixListener::bind(path).with_context(|| format!("bind socket {}", path.display()))?;
    configure_socket_access(path)?;
    Ok(listener)
}

fn configure_socket_access(path: &Path) -> anyhow::Result<()> {
    if let Some(gid) = std::env::var("SUDO_GID")
        .ok()
        .and_then(|gid| gid.parse::<u32>().ok())
    {
        chown(path, None, Some(gid))
            .with_context(|| format!("set socket group on {}", path.display()))?;
    }

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))
        .with_context(|| format!("set socket permissions on {}", path.display()))
}
