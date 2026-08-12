use std::{
    net::SocketAddr,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::{Path, PathBuf},
};

use agentdesktop_agent::{api, discovery, enrollment::EnrollmentState, reconcile, remote};
use agentdesktop_core::{
    DEFAULT_CONFIG_PATH, DEFAULT_SOCKET_PATH, DEFAULT_STATE_DIR, config, telemetry,
};
use anyhow::{Context, bail};
use clap::Parser;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
use tokio::net::UnixListener;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(about = "AgentDesktop privileged daemon")]
struct Args {
    /// Path to the local YAML configuration file.
    #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,

    /// Unix socket used by local AgentDesktop clients.
    #[arg(long, default_value = DEFAULT_SOCKET_PATH)]
    socket: PathBuf,

    /// Directory used for the device identity and other persistent daemon state.
    #[arg(long, default_value = DEFAULT_STATE_DIR)]
    state_dir: PathBuf,

    /// Address for the OIDC callback server to bind instead of the redirect URI's loopback address.
    #[arg(long)]
    oidc_callback_listen: Option<SocketAddr>,

    /// Directory containing Claude Code managed-settings drop-in files.
    #[arg(
        long,
        default_value_os_t = reconcile::default_claude_code_managed_settings_dir()
    )]
    claude_code_managed_settings_dir: PathBuf,

    /// Path to Codex's organization-managed TOML configuration.
    #[arg(long, default_value_os_t = reconcile::default_codex_managed_config_path())]
    codex_managed_config: PathBuf,

    /// Path to OpenCode's system-managed JSONC configuration.
    #[arg(long, default_value_os_t = reconcile::default_open_code_managed_config_path())]
    open_code_managed_config: PathBuf,

    /// Path used for AgentDesktop's OpenCode credential plugin.
    #[arg(long, default_value_os_t = reconcile::default_open_code_plugin_path())]
    open_code_plugin: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let _log_flush = telemetry::setup_logging("info", false);
    agentdesktop_agent::secure_fs::ensure_private_dir(&args.state_dir)?;
    let config = config::load_daemon(&args.config)?;
    let enrollment = EnrollmentState::new(config.controller.is_some());
    let reconciler = reconcile::Reconciler::new(
        args.claude_code_managed_settings_dir.clone(),
        args.codex_managed_config.clone(),
        args.open_code_managed_config.clone(),
        args.open_code_plugin.clone(),
        credential_helper_executable()?,
        args.socket.clone(),
    );
    let local_config = config.clone();
    let cached_remote_path = args.state_dir.join("remote-config.yaml");
    let initial_config = if config.controller.is_some() {
        match std::fs::read_to_string(&cached_remote_path) {
            Ok(contents) => {
                tracing::info!(
                    path = %cached_remote_path.display(),
                    "restoring last accepted controller configuration"
                );
                Some(config::parse_daemon(&contents).with_context(|| {
                    format!(
                        "parse cached controller configuration from {}",
                        cached_remote_path.display()
                    )
                })?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (!local_config.is_empty()).then_some(local_config)
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "read cached controller configuration from {}",
                        cached_remote_path.display()
                    )
                });
            }
        }
    } else {
        Some(local_config)
    };
    if let Some(initial_config) = initial_config {
        reconciler
            .apply(&initial_config)
            .context("apply initial daemon configuration")?;
    } else {
        tracing::info!(
            "preserving managed files until the controller provides daemon configuration"
        );
    }
    let discovery = discovery::discover().await;
    for agent in &discovery.agents {
        tracing::info!(
            kind = %agent.kind,
            executable = %agent.executable.display(),
            version = agent.version.as_deref().unwrap_or("unknown"),
            mcps = agent.mcp_servers.len(),
            skills = agent.skills.len(),
            "discovered program"
        );
    }
    let (telemetry_sender, telemetry_receiver) = mpsc::channel(256);
    let telemetry = config.controller.as_ref().map(|_| telemetry_sender.clone());
    if let Some(controller) = config.controller.clone() {
        let remote_discovery = discovery.clone();
        let state_dir = args.state_dir.clone();
        let oidc_callback_listen = args.oidc_callback_listen;
        let remote_enrollment = enrollment.clone();
        tokio::spawn(async move {
            if let Err(error) = remote::run(
                controller,
                remote_discovery,
                state_dir,
                oidc_callback_listen,
                reconciler,
                remote_enrollment.clone(),
                telemetry_receiver,
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
        state_dir: args.state_dir,
        telemetry,
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

fn credential_helper_executable() -> anyhow::Result<PathBuf> {
    let path = std::env::current_exe()
        .context("locate agentdesktopd executable")?
        .with_file_name(format!("agentdesktop{}", std::env::consts::EXE_SUFFIX));
    if !path.is_file() {
        anyhow::bail!(
            "AgentDesktop credential helper is missing at {}; install or build agentdesktop next to agentdesktopd",
            path.display()
        );
    }
    Ok(path)
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
    // The local API intentionally permits arbitrary clients. In particular,
    // client_id on credential requests is caller-asserted rather than an
    // authenticated process identity.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666))
        .with_context(|| format!("set socket permissions on {}", path.display()))
}
