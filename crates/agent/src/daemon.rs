use std::{
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::{
    ffi::CString,
    os::unix::fs::{FileTypeExt, PermissionsExt},
};

use agentdesktop_core::{
    DEFAULT_CONFIG_PATH, DEFAULT_SOCKET_PATH, DEFAULT_STATE_DIR, config, telemetry,
};
use anyhow::{Context, bail};
use clap::Args;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;
#[cfg(windows)]
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

#[cfg(windows)]
use crate::windows_security::SecurityDescriptor;
use crate::{
    api, discovery, enrollment::EnrollmentState, gateway_oidc, reconcile, remote, secure_fs,
};

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
    /// Run entirely as the current user and manage user-level tool settings.
    #[arg(long)]
    user: bool,

    /// Reconcile the local configuration once and exit.
    #[arg(long)]
    once: bool,

    /// Preview reconciliation of local configuration without changing files. Implies --once.
    #[arg(long)]
    dry_run: bool,

    /// Path to the local YAML configuration file.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Directory used for persistent daemon state.
    #[arg(long)]
    state_dir: Option<PathBuf>,

    /// Address for the OIDC callback server to bind instead of the redirect URI's loopback address.
    #[arg(long)]
    oidc_callback_listen: Option<SocketAddr>,

    /// Path to Claude Code's Agentdesktop-managed settings file.
    #[arg(long)]
    claude_code_settings: Option<PathBuf>,

    /// Path to Claude Desktop's system-managed JSON configuration.
    #[arg(long)]
    claude_desktop_managed_settings: Option<PathBuf>,

    /// Path used for Claude Desktop's Agentdesktop credential helper.
    #[arg(long)]
    claude_desktop_credential_helper: Option<PathBuf>,

    /// Path to Codex's organization-managed TOML configuration.
    #[arg(long)]
    codex_managed_config: Option<PathBuf>,

    /// Path to OpenCode's system-managed JSONC configuration.
    #[arg(long)]
    open_code_managed_config: Option<PathBuf>,

    /// Path used for Agentdesktop's OpenCode credential plugin.
    #[arg(long)]
    open_code_plugin: Option<PathBuf>,
}

struct ResolvedDaemonArgs {
    user: bool,
    config: PathBuf,
    state_dir: PathBuf,
    socket: PathBuf,
    oidc_callback_listen: Option<SocketAddr>,
    claude_code_settings: PathBuf,
    claude_desktop_managed_settings: PathBuf,
    claude_desktop_credential_helper: PathBuf,
    codex_managed_config: PathBuf,
    open_code_managed_config: PathBuf,
    open_code_plugin: PathBuf,
    once: bool,
    dry_run: bool,
}

impl DaemonArgs {
    fn resolve(self, socket: PathBuf) -> anyhow::Result<ResolvedDaemonArgs> {
        if !self.user {
            return Ok(ResolvedDaemonArgs {
                user: false,
                config: self.config.unwrap_or_else(|| DEFAULT_CONFIG_PATH.into()),
                state_dir: self.state_dir.unwrap_or_else(|| DEFAULT_STATE_DIR.into()),
                socket,
                oidc_callback_listen: self.oidc_callback_listen,
                claude_code_settings: self.claude_code_settings.unwrap_or_else(|| {
                    reconcile::default_claude_code_managed_settings_dir()
                        .join("50-agentdesktop.json")
                }),
                claude_desktop_managed_settings: self
                    .claude_desktop_managed_settings
                    .unwrap_or_else(reconcile::default_claude_desktop_managed_settings_path),
                claude_desktop_credential_helper: self
                    .claude_desktop_credential_helper
                    .unwrap_or_else(reconcile::default_claude_desktop_credential_helper_path),
                codex_managed_config: self
                    .codex_managed_config
                    .unwrap_or_else(reconcile::default_codex_managed_config_path),
                open_code_managed_config: self
                    .open_code_managed_config
                    .unwrap_or_else(reconcile::default_open_code_managed_config_path),
                open_code_plugin: self
                    .open_code_plugin
                    .unwrap_or_else(reconcile::default_open_code_plugin_path),
                once: self.once || self.dry_run,
                dry_run: self.dry_run,
            });
        }

        let home = home_directory()?;
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let state_home = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state"));
        let state_dir = self
            .state_dir
            .unwrap_or_else(|| state_home.join("agentdesktop"));
        let socket = if socket == Path::new(DEFAULT_SOCKET_PATH) {
            user_socket_path(&state_dir)
        } else {
            socket
        };
        let claude_desktop_settings = user_claude_desktop_settings(&home, &config_home);
        Ok(ResolvedDaemonArgs {
            user: true,
            config: self
                .config
                .unwrap_or_else(|| config_home.join("agentdesktop/config.yaml")),
            state_dir: state_dir.clone(),
            socket,
            oidc_callback_listen: self.oidc_callback_listen,
            claude_code_settings: self
                .claude_code_settings
                .unwrap_or_else(|| home.join(".claude/settings.json")),
            claude_desktop_managed_settings: self
                .claude_desktop_managed_settings
                .unwrap_or(claude_desktop_settings),
            claude_desktop_credential_helper: self
                .claude_desktop_credential_helper
                .unwrap_or_else(|| state_dir.join("bin/claude-desktop-credential-helper")),
            codex_managed_config: self
                .codex_managed_config
                .unwrap_or_else(|| home.join(".codex/config.toml")),
            open_code_managed_config: self
                .open_code_managed_config
                .unwrap_or_else(|| config_home.join("opencode/opencode.json")),
            open_code_plugin: self
                .open_code_plugin
                .unwrap_or_else(|| config_home.join("opencode/plugins/agentdesktop.js")),
            once: self.once || self.dry_run,
            dry_run: self.dry_run,
        })
    }
}

fn home_directory() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("--user requires HOME or USERPROFILE")
}

#[cfg(unix)]
fn user_socket_path(state_dir: &Path) -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| state_dir.to_owned())
        .join("agentdesktop.sock")
}

#[cfg(windows)]
fn user_socket_path(_state_dir: &std::path::Path) -> PathBuf {
    PathBuf::from(DEFAULT_SOCKET_PATH)
}

fn user_claude_desktop_settings(_home: &Path, _config_home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    return _home.join("Library/Application Support/Claude/claude_desktop_config.json");
    #[cfg(target_os = "windows")]
    return std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| _home.join("AppData/Roaming"))
        .join("Claude/claude_desktop_config.json");
    #[cfg(target_os = "linux")]
    return _config_home.join("Claude/claude_desktop_config.json");
}

pub async fn run(args: DaemonArgs, socket: PathBuf) -> anyhow::Result<()> {
    run_until_shutdown(args, socket, async {
        tokio::signal::ctrl_c()
            .await
            .context("wait for shutdown signal")
    })
    .await
}

/// Serves the local API until the `shutdown` future resolves or fails.
pub async fn run_until_shutdown<F>(
    args: DaemonArgs,
    socket: PathBuf,
    shutdown: F,
) -> anyhow::Result<()>
where
    F: Future<Output = anyhow::Result<()>> + Send,
{
    let args = args.resolve(socket)?;
    let _log_flush = telemetry::setup_logging(if args.once { "warn" } else { "info" }, false);
    let socket = args.socket.clone();
    let config = config::load_daemon(&args.config)?;
    let reconciler = reconcile::Reconciler::new(
        args.user,
        args.claude_code_settings.clone(),
        args.claude_desktop_managed_settings.clone(),
        args.claude_desktop_credential_helper.clone(),
        args.codex_managed_config.clone(),
        args.open_code_managed_config.clone(),
        args.open_code_plugin.clone(),
        agentdesktop_executable()?,
        socket.clone(),
    );
    if args.once {
        if args.dry_run {
            validate_dry_run(&config)?;
            reconciler
                .dry_run(&config)
                .context("preview daemon configuration")?;
        } else {
            validate_one_shot(&config)?;
            reconciler
                .apply(&config)
                .context("apply daemon configuration")?;
            println!("Reconciliation complete.");
        }
        return Ok(());
    }

    secure_fs::ensure_private_dir(&args.state_dir)?;
    start_gateway_oidc(&config, args.state_dir.clone(), args.oidc_callback_listen);
    let enrollment = EnrollmentState::new(config.controller.is_some());
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
        oidc_callback_listen: args.oidc_callback_listen,
        telemetry,
        logout,
    });

    tracing::info!(socket = %socket.display(), "agent daemon listening");
    #[cfg(unix)]
    serve_unix(&socket, local_api_access()?, app, shutdown).await?;
    #[cfg(windows)]
    serve_named_pipe(&socket, app, shutdown).await?;

    Ok(())
}

fn start_gateway_oidc(
    config: &agentdesktop_core::config::DaemonConfig,
    state_dir: PathBuf,
    callback_listen: Option<SocketAddr>,
) {
    let Some(agentdesktop_core::config::InferenceGatewayAuthentication::Oidc {
        issuer,
        client_id,
        redirect_uri,
        scopes,
        allow_insecure,
    }) = config
        .inference_gateway
        .as_ref()
        .and_then(|gateway| gateway.authentication.as_ref())
    else {
        return;
    };
    let issuer = issuer.clone();
    let client_id = client_id.clone();
    let redirect_uri = redirect_uri.clone();
    let scopes = scopes.clone();
    let allow_insecure = *allow_insecure;
    tokio::spawn(async move {
        tracing::info!(%issuer, "starting inference gateway OIDC authentication");
        match gateway_oidc::credential(
            &issuer,
            &client_id,
            &redirect_uri,
            &scopes,
            allow_insecure,
            &state_dir,
            callback_listen,
        )
        .await
        {
            Ok(_) => tracing::info!(%issuer, "inference gateway OIDC authentication ready"),
            Err(error) => tracing::error!(
                error = %format!("{error:#}"),
                %issuer,
                "inference gateway OIDC authentication failed"
            ),
        }
    });
}

fn validate_dry_run(config: &agentdesktop_core::config::DaemonConfig) -> anyhow::Result<()> {
    if config.controller.is_some() {
        bail!(
            "--dry-run only previews local configuration; controller-managed configuration is received after enrollment while the daemon is running; run without --dry-run to enroll and apply it"
        );
    }
    Ok(())
}

fn validate_one_shot(config: &agentdesktop_core::config::DaemonConfig) -> anyhow::Result<()> {
    if config.controller.is_some() {
        bail!(
            "--once cannot use a controller because controller synchronization requires the daemon to remain running"
        );
    }
    if !config.telemetry.events.is_empty() {
        bail!("--once cannot collect telemetry because hooks require the daemon to remain running");
    }
    let authenticated_gateway_is_used = config
        .inference_gateway
        .as_ref()
        .is_some_and(|gateway| gateway.authentication.is_some())
        && [
            config
                .programs
                .claude_code
                .as_ref()
                .is_some_and(|program| program.use_inference_gateway),
            config
                .programs
                .claude_desktop
                .as_ref()
                .is_some_and(|program| program.use_inference_gateway),
            config
                .programs
                .codex
                .as_ref()
                .is_some_and(|program| program.use_inference_gateway),
            config
                .programs
                .open_code
                .as_ref()
                .is_some_and(|program| program.use_inference_gateway),
        ]
        .into_iter()
        .any(|used| used);
    if authenticated_gateway_is_used {
        bail!(
            "--once cannot configure an authenticated inference gateway because credential helpers require the daemon to remain running"
        );
    }
    Ok(())
}

#[cfg(unix)]
async fn serve_unix(
    socket: &Path,
    access: LocalApiAccess,
    app: axum::Router,
    shutdown: impl Future<Output = anyhow::Result<()>> + Send,
) -> anyhow::Result<()> {
    let listener = bind_unix(socket, access)?;
    tokio::pin!(shutdown);
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
            result = &mut shutdown => {
                result?;
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
async fn serve_named_pipe(
    socket: &std::path::Path,
    app: axum::Router,
    shutdown: impl Future<Output = anyhow::Result<()>> + Send,
) -> anyhow::Result<()> {
    let mut server = bind_named_pipe(socket, true)?;
    tokio::pin!(shutdown);
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
            result = &mut shutdown => {
                result?;
                break;
            }
        }
    }
    Ok(())
}

#[cfg(windows)]
fn bind_named_pipe(path: &std::path::Path, first: bool) -> anyhow::Result<NamedPipeServer> {
    // Full access for SYSTEM and Administrators, read/write for interactive users.
    const PIPE_SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)";

    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    let descriptor = SecurityDescriptor::from_sddl(PIPE_SDDL)?;
    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.as_ptr(),
        bInheritHandle: 0,
    };
    // SAFETY: attributes and its descriptor remain valid for the duration of
    // CreateNamedPipeW and are released only after the call returns.
    unsafe {
        options.create_with_security_attributes_raw(
            path.as_os_str(),
            (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
        )
    }
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

#[cfg(test)]
mod tests {
    use agentdesktop_core::config::parse_daemon;

    use super::{validate_dry_run, validate_one_shot};

    #[test]
    fn one_shot_accepts_static_settings_and_rejects_runtime_services() {
        let static_config = parse_daemon(
            r#"
programs:
  claudeCode:
    permissions:
      defaultMode: plan
"#,
        )
        .unwrap();
        validate_one_shot(&static_config).expect("static settings work in one-shot mode");

        let oidc = parse_daemon(
            r#"
inferenceGateway:
  url: https://gateway.example.com
  authentication:
    type: oidc
    issuer: https://login.example.com
    clientId: agentdesktop
programs:
  claudeCode: {}
"#,
        )
        .unwrap();
        assert!(
            validate_one_shot(&oidc)
                .unwrap_err()
                .to_string()
                .contains("credential helpers")
        );

        let telemetry = parse_daemon(
            r#"
telemetry:
  events: [tool.use]
"#,
        )
        .unwrap();
        assert!(
            validate_one_shot(&telemetry)
                .unwrap_err()
                .to_string()
                .contains("telemetry")
        );
    }

    #[test]
    fn dry_run_rejects_controller_managed_configuration() {
        let managed = parse_daemon(
            r#"
controller:
  address: https://controller.example.com
"#,
        )
        .expect("valid managed configuration");

        let error = validate_dry_run(&managed).expect_err("managed dry run must fail");
        assert!(
            error
                .to_string()
                .contains("only previews local configuration")
        );

        let local = parse_daemon(
            r#"
programs:
  claudeCode:
    companyAnnouncements: [Managed locally]
"#,
        )
        .expect("valid local configuration");
        validate_dry_run(&local).expect("local dry run works");
    }
}
