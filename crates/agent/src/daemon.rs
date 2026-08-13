use std::{net::SocketAddr, path::PathBuf};

#[cfg(unix)]
use std::{
    ffi::CString,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::Path,
};

use agentdesktop_core::{DEFAULT_CONFIG_PATH, DEFAULT_STATE_DIR, config, telemetry};
use anyhow::Context;
#[cfg(unix)]
use anyhow::bail;
use clap::Args;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;

use crate::{api, discovery, enrollment::EnrollmentState, reconcile, remote, secure_fs};

#[cfg(unix)]
const LOCAL_API_GROUP: &str = "agentdesktop";

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LocalApiAccess {
    User(u32),
    Group(u32),
    Owner,
}

#[derive(Args)]
pub struct DaemonArgs {
    /// Path to the local YAML configuration file.
    #[arg(long, default_value = DEFAULT_CONFIG_PATH)]
    config: PathBuf,

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

    /// Path to Claude Desktop's system-managed JSON configuration.
    #[arg(
        long,
        default_value_os_t = reconcile::default_claude_desktop_managed_settings_path()
    )]
    claude_desktop_managed_settings: PathBuf,

    /// Path used for Claude Desktop's Agentdesktop credential helper.
    #[arg(
        long,
        default_value_os_t = reconcile::default_claude_desktop_credential_helper_path()
    )]
    claude_desktop_credential_helper: PathBuf,

    /// Path to Codex's organization-managed TOML configuration.
    #[arg(long, default_value_os_t = reconcile::default_codex_managed_config_path())]
    codex_managed_config: PathBuf,

    /// Path to OpenCode's system-managed JSONC configuration.
    #[arg(long, default_value_os_t = reconcile::default_open_code_managed_config_path())]
    open_code_managed_config: PathBuf,

    /// Path used for Agentdesktop's OpenCode credential plugin.
    #[arg(long, default_value_os_t = reconcile::default_open_code_plugin_path())]
    open_code_plugin: PathBuf,
}

pub async fn run(args: DaemonArgs, socket: PathBuf) -> anyhow::Result<()> {
    let _log_flush = telemetry::setup_logging("info", false);
    secure_fs::ensure_private_dir(&args.state_dir)?;
    let config = config::load_daemon(&args.config)?;
    let enrollment = EnrollmentState::new(config.controller.is_some());
    let reconciler = reconcile::Reconciler::new(
        args.claude_code_managed_settings_dir.clone(),
        args.claude_desktop_managed_settings.clone(),
        args.claude_desktop_credential_helper.clone(),
        args.codex_managed_config.clone(),
        args.open_code_managed_config.clone(),
        args.open_code_plugin.clone(),
        agentdesktop_executable()?,
        socket.clone(),
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
    let (logout_sender, logout_receiver) = mpsc::channel(1);
    let logout = config.controller.as_ref().map(|_| logout_sender);
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
                remote::Requests {
                    telemetry: telemetry_receiver,
                    logout: logout_receiver,
                },
            )
            .await
            {
                remote_enrollment.set("failed").await;
                tracing::error!(error = %format!("{error:#}"), "controller integration disabled");
            }
        });
    }
    let app = api::router(api::AppState {
        config,
        discovery,
        enrollment,
        state_dir: args.state_dir,
        telemetry,
        logout,
    });

    tracing::info!(socket = %socket.display(), "agent daemon listening");
    #[cfg(unix)]
    serve_unix(&socket, local_api_access()?, app).await?;
    #[cfg(windows)]
    serve_named_pipe(&socket, app).await?;

    Ok(())
}

#[cfg(unix)]
async fn serve_unix(
    socket: &Path,
    access: LocalApiAccess,
    app: axum::Router,
) -> anyhow::Result<()> {
    let listener = bind_unix(socket, access)?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accept connection")?;
                let peer = stream.peer_cred().context("inspect local API peer credentials")?;
                tracing::debug!(peer_uid = peer.uid(), peer_pid = peer.pid(), "accepted local API connection");
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

    if let Err(error) = std::fs::remove_file(socket) {
        tracing::warn!(%error, socket = %socket.display(), "failed to remove socket");
    }
    Ok(())
}

#[cfg(windows)]
async fn serve_named_pipe(socket: &std::path::Path, app: axum::Router) -> anyhow::Result<()> {
    let mut server = bind_named_pipe(socket, true)?;
    loop {
        tokio::select! {
            connected = server.connect() => {
                connected.with_context(|| format!("accept connection on {}", socket.display()))?;
                let connected = server;
                // Keep an unconnected instance available while the accepted
                // connection is being served. Without this, clients can see a
                // transient pipe-not-found error between connections.
                server = bind_named_pipe(socket, false)?;
                let service = TowerToHyperService::new(app.clone());
                tokio::spawn(async move {
                    if let Err(error) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(connected), service)
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
    Ok(())
}

#[cfg(windows)]
fn bind_named_pipe(path: &std::path::Path, first: bool) -> anyhow::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    options
        .create(path.as_os_str())
        .with_context(|| format!("bind named pipe {}", path.display()))
}

fn agentdesktop_executable() -> anyhow::Result<PathBuf> {
    std::env::current_exe().context("locate agentdesktop executable")
}

#[cfg(unix)]
fn bind_unix(path: &Path, access: LocalApiAccess) -> anyhow::Result<UnixListener> {
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
    if let Err(error) = configure_socket_access(path, access) {
        let _ = std::fs::remove_file(path);
        return Err(error);
    }
    Ok(listener)
}

#[cfg(unix)]
fn configure_socket_access(path: &Path, access: LocalApiAccess) -> anyhow::Result<()> {
    let mode = match access {
        LocalApiAccess::User(uid) => {
            std::os::unix::fs::chown(path, Some(uid), None)
                .with_context(|| format!("set socket owner for {}", path.display()))?;
            tracing::info!(
                authorized_uid = uid,
                "local API access granted to sudo user"
            );
            0o600
        }
        LocalApiAccess::Group(gid) => {
            std::os::unix::fs::chown(path, None, Some(gid))
                .with_context(|| format!("set socket group for {}", path.display()))?;
            tracing::info!(
                group = LOCAL_API_GROUP,
                gid,
                "local API access granted to group"
            );
            0o660
        }
        LocalApiAccess::Owner => 0o600,
    };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("set socket permissions on {}", path.display()))
}

#[cfg(unix)]
fn local_api_access() -> anyhow::Result<LocalApiAccess> {
    let euid = effective_uid();
    if euid != 0 {
        return Ok(LocalApiAccess::Owner);
    }
    if let Some(uid) = sudo_uid(std::env::var_os("SUDO_UID"), euid) {
        return Ok(LocalApiAccess::User(uid));
    }
    group_id(LOCAL_API_GROUP)?.map(LocalApiAccess::Group).ok_or_else(|| {
        anyhow::anyhow!(
            "root daemon requires the `{LOCAL_API_GROUP}` group; create it and add authorized desktop users, or launch with sudo to authorize the invoking user automatically"
        )
    })
}

#[cfg(unix)]
fn sudo_uid(value: Option<std::ffi::OsString>, effective_uid: u32) -> Option<u32> {
    if effective_uid != 0 {
        return None;
    }
    value
        .and_then(|value| value.into_string().ok())
        .and_then(|value| value.parse().ok())
        .filter(|uid| *uid != 0)
}

#[cfg(unix)]
fn group_id(name: &str) -> anyhow::Result<Option<u32>> {
    let name = CString::new(name).context("local API group contains a null byte")?;
    // SAFETY: getgrnam returns either null or a valid process-owned group
    // entry. We copy the numeric ID before returning.
    let group = unsafe { libc::getgrnam(name.as_ptr()) };
    Ok((!group.is_null()).then(|| unsafe { (*group).gr_gid }))
}

#[cfg(unix)]
fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    unsafe { libc::geteuid() }
}
