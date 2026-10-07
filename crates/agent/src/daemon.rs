use std::{
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

#[cfg(unix)]
use std::{
    ffi::CString,
    os::unix::fs::{FileTypeExt, PermissionsExt},
};

use agentdesktop_core::config::GitHubTokenSource;
use agentdesktop_core::{
    DEFAULT_CONFIG_PATH, DEFAULT_SOCKET_PATH, VERSION, config,
    model::{DaemonControllerInfo, DaemonInfo, DaemonScope, LlmProxyInfo},
    telemetry,
};
use anyhow::{Context, bail};
use clap::Args;
use hyper_util::{rt::TokioIo, service::TowerToHyperService};
#[cfg(unix)]
use tokio::net::UnixListener;
#[cfg(windows)]
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::{
    sync::{mpsc, watch},
    time,
};
#[cfg(windows)]
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

#[cfg(windows)]
use crate::windows_security::SecurityDescriptor;
use crate::{
    api, enrollment::EnrollmentState, gateway_oidc, llm_proxy, reconcile, remote, secure_fs,
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
}

struct ResolvedDaemonArgs {
    user: bool,
    config: PathBuf,
    state_dir: PathBuf,
    socket: PathBuf,
    oidc_callback_listen: Option<SocketAddr>,
    llm_proxy_listen: Option<SocketAddr>,
    llm_proxy_client_id: String,
    claude_code: ResolvedToolConfigPath,
    claude_desktop: ResolvedClaudeDesktopStartupConfig,
    codex: ResolvedToolConfigPath,
    open_code: ResolvedOpenCodeStartupConfig,
    grok: ResolvedToolConfigPath,
    /// The Copilot CLI providers file; `None` in system mode (user-only program).
    copilot_providers: Option<PathBuf>,
    /// VS Code's `chatLanguageModels.json`; `None` in system mode (user-only program).
    vscode_chat_models: Option<PathBuf>,
    /// VS Code's user `settings.json`; `None` in system mode (user-only program).
    vscode_settings: Option<PathBuf>,
    once: bool,
    dry_run: bool,
}

struct ResolvedToolConfigPath {
    config: PathBuf,
}

struct ResolvedClaudeDesktopStartupConfig {
    config: PathBuf,
    credential_helper: PathBuf,
}

struct ResolvedOpenCodeStartupConfig {
    config: PathBuf,
    plugin: PathBuf,
}

impl DaemonArgs {
    fn resolve(
        self,
        startup: config::DaemonStartupConfig,
        config: PathBuf,
        socket: PathBuf,
    ) -> anyhow::Result<ResolvedDaemonArgs> {
        let user = self.user || startup.user;
        let socket = startup.socket.clone().unwrap_or(socket);
        let llm_proxy_listen = startup.llm_proxy.listen;
        // The proxy issues the current user's gateway credential to whatever
        // connects on loopback; a system daemon has no single user to act for.
        // Checked first so a system-mode operator sees this reason, not a
        // secondary one about the address or the run mode.
        if !user && llm_proxy_listen.is_some() {
            // clientId without listen is inert in either mode; only listen is rejected.
            bail!("daemon.llmProxy.listen requires --user (or daemon.user: true)");
        }
        if llm_proxy_listen.is_some_and(|address| !address.ip().is_loopback()) {
            bail!("daemon.llmProxy.listen must be a loopback address");
        }
        if llm_proxy_listen.is_some() && (self.once || self.dry_run) {
            bail!("daemon.llmProxy.listen cannot be combined with --once or --dry-run");
        }
        let llm_proxy_client_id = startup
            .llm_proxy
            .client_id
            .clone()
            .unwrap_or_else(|| "vscode".to_owned());
        if llm_proxy_listen.is_some() && !config::valid_client_id(&llm_proxy_client_id) {
            bail!("invalid daemon.llmProxy.clientId");
        }
        // VS Code's chatLanguageModels.json and settings.json live in the
        // user's own profile, like the Copilot CLI providers file: a system
        // daemon has no user file to manage, so an explicit override is
        // rejected up front.
        if !user
            && let Some(field) = [
                ("config", startup.vscode.config.is_some()),
                ("settings", startup.vscode.settings.is_some()),
            ]
            .into_iter()
            .find_map(|(field, set)| set.then_some(field))
        {
            bail!("daemon.vscode.{field} requires --user (or daemon.user: true)");
        }
        if !user {
            return Ok(ResolvedDaemonArgs {
                user: false,
                config,
                state_dir: startup
                    .state_dir
                    .unwrap_or_else(|| PathBuf::from(agentdesktop_core::DEFAULT_STATE_DIR)),
                socket,
                oidc_callback_listen: startup.oidc_callback_listen,
                llm_proxy_listen: None,
                llm_proxy_client_id: llm_proxy_client_id.clone(),
                claude_code: ResolvedToolConfigPath {
                    config: startup.claude_code.config.unwrap_or_else(|| {
                        reconcile::default_claude_code_managed_settings_dir()
                            .join("50-agentdesktop.json")
                    }),
                },
                claude_desktop: ResolvedClaudeDesktopStartupConfig {
                    config: startup
                        .claude_desktop
                        .config
                        .unwrap_or_else(reconcile::default_claude_desktop_managed_settings_path),
                    credential_helper: startup
                        .claude_desktop
                        .credential_helper
                        .unwrap_or_else(reconcile::default_claude_desktop_credential_helper_path),
                },
                codex: ResolvedToolConfigPath {
                    config: startup
                        .codex
                        .config
                        .unwrap_or_else(reconcile::default_codex_managed_config_path),
                },
                open_code: ResolvedOpenCodeStartupConfig {
                    config: startup
                        .open_code
                        .config
                        .unwrap_or_else(reconcile::default_open_code_managed_config_path),
                    plugin: startup
                        .open_code
                        .plugin
                        .unwrap_or_else(reconcile::default_open_code_plugin_path),
                },
                grok: ResolvedToolConfigPath {
                    config: startup
                        .grok
                        .config
                        .unwrap_or_else(reconcile::default_grok_managed_config_path),
                },
                copilot_providers: {
                    if startup.copilot.config.is_some() {
                        bail!("daemon.copilot.config requires --user (or daemon.user: true)");
                    }
                    None
                },
                vscode_chat_models: None,
                vscode_settings: None,
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
        let state_dir = startup
            .state_dir
            .unwrap_or_else(|| state_home.join("agentdesktop"));
        let socket = if startup.socket.is_none() && socket == Path::new(DEFAULT_SOCKET_PATH) {
            user_socket_path(&state_dir)
        } else {
            socket
        };
        let claude_desktop_settings = user_claude_desktop_settings(&home, &config_home);
        Ok(ResolvedDaemonArgs {
            user: true,
            config,
            state_dir: state_dir.clone(),
            socket,
            oidc_callback_listen: startup.oidc_callback_listen,
            llm_proxy_listen,
            llm_proxy_client_id,
            claude_code: ResolvedToolConfigPath {
                config: startup
                    .claude_code
                    .config
                    .unwrap_or_else(|| home.join(".claude/settings.json")),
            },
            claude_desktop: ResolvedClaudeDesktopStartupConfig {
                config: startup
                    .claude_desktop
                    .config
                    .unwrap_or(claude_desktop_settings),
                credential_helper: startup
                    .claude_desktop
                    .credential_helper
                    .unwrap_or_else(|| state_dir.join("bin/claude-desktop-credential-helper")),
            },
            codex: ResolvedToolConfigPath {
                config: startup
                    .codex
                    .config
                    .unwrap_or_else(|| home.join(".codex/config.toml")),
            },
            open_code: ResolvedOpenCodeStartupConfig {
                config: startup
                    .open_code
                    .config
                    .unwrap_or_else(|| config_home.join("opencode/opencode.json")),
                plugin: startup
                    .open_code
                    .plugin
                    .unwrap_or_else(|| config_home.join("opencode/plugins/agentdesktop.js")),
            },
            grok: ResolvedToolConfigPath {
                config: startup.grok.config.unwrap_or_else(|| {
                    std::env::var_os("GROK_HOME")
                        .filter(|value| !value.is_empty())
                        .map(PathBuf::from)
                        .unwrap_or_else(|| home.join(".grok"))
                        .join("managed_config.toml")
                }),
            },
            copilot_providers: Some(match startup.copilot.config {
                Some(path) => path,
                None => reconcile::default_copilot_providers_path()?,
            }),
            vscode_chat_models: Some(
                startup
                    .vscode
                    .config
                    .unwrap_or_else(|| reconcile::default_vscode_chat_models_path(&home)),
            ),
            vscode_settings: Some(
                startup
                    .vscode
                    .settings
                    .unwrap_or_else(|| reconcile::default_vscode_settings_path(&home)),
            ),
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
    if socket != Path::new(DEFAULT_SOCKET_PATH) {
        bail!("set daemon.socket in the configuration file; --socket is for client commands");
    }
    let config_path = match args.config.clone() {
        Some(path) => path,
        None if args.user => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or(home_directory()?.join(".config"))
            .join("agentdesktop/config.yaml"),
        None => DEFAULT_CONFIG_PATH.into(),
    };
    let config = config::load_daemon(&config_path)?;
    let startup = config.daemon.clone().unwrap_or_default();
    let args = args.resolve(startup, config_path, socket)?;
    // The user-mode LaunchAgent/systemd unit we generate sets an explicit
    // stdout log path, so stdout logging already works there. The
    // system-wide daemon has no such guarantee — nothing in this repo
    // controls how its supervisor (e.g. an MDM-deployed LaunchDaemon) is
    // configured, and a supervisor entry with no log redirect silently
    // discards everything written to stdout. Give the system daemon a log
    // file it controls itself so its logs exist regardless of that.
    let log_dir = (!args.user && !args.once).then(|| args.state_dir.join("logs"));
    // Logs can carry hostnames, paths, and controller addresses. Create the
    // state and log directories owner-only before the logger can create them
    // with the process umask.
    if let Some(log_dir) = &log_dir {
        secure_fs::ensure_private_dir(&args.state_dir)?;
        secure_fs::ensure_private_dir(log_dir)?;
    }
    let _log_flush = telemetry::setup_logging(
        if args.once { "warn" } else { "info" },
        false,
        log_dir.as_deref(),
    )?;
    let socket = args.socket.clone();
    // Bind the loopback proxy before the reconciler exists, so anything that
    // writes the proxy address into a managed file gets the bind result, never
    // a configured address the daemon does not own. A failed bind is reported
    // through daemon-info and the proxy stays off; the daemon keeps enrolling
    // and reconciling, since a port taken by some other process must not take
    // the device out of the fleet.
    let (mut proxy_listener, mut llm_proxy) =
        bind_llm_proxy(args.llm_proxy_listen, &args.llm_proxy_client_id).await;
    let pairing = attach_llm_proxy_pairing(&mut proxy_listener, &mut llm_proxy, &args.state_dir);
    let proxy_context = match (&proxy_listener, &pairing) {
        (Some(listener), Some(pairing)) => {
            listener
                .local_addr()
                .ok()
                .map(|address| llm_proxy::LlmProxyContext {
                    address,
                    pairing: pairing.clone(),
                })
        }
        _ => None,
    };
    let reconciler = reconcile::Reconciler::new(
        args.user,
        args.claude_code.config.clone(),
        args.claude_desktop.config.clone(),
        args.claude_desktop.credential_helper.clone(),
        args.codex.config.clone(),
        args.open_code.config.clone(),
        args.open_code.plugin.clone(),
        args.grok.config.clone(),
        args.copilot_providers.clone(),
        args.vscode_chat_models.clone(),
        args.vscode_settings.clone(),
        agentdesktop_client_executable()?,
        socket.clone(),
    )
    .with_llm_proxy(proxy_context);
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

    let daemon_info = describe_daemon(&config, &args.config, &args.state_dir, args.user, llm_proxy);
    secure_fs::ensure_private_dir(&args.state_dir)?;
    start_gateway_authentication(
        &config,
        args.state_dir.clone(),
        args.oidc_callback_listen,
        proxy_listener.is_some(),
    );
    let enrollment = EnrollmentState::new(config.controller.is_some());
    let controller_status = remote::ControllerConnectionState::new();
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
        // With a controller, the cached controller configuration is what is in effect
        // from here, and start_gateway_authentication above only saw the local file.
        // Without one the two are the same configuration and it already ran.
        if config.controller.is_some() {
            gateway_oidc::start_device_login_if_configured(&initial_config, &args.state_dir);
        }
    } else {
        tracing::info!(
            "preserving managed files until the controller provides daemon configuration"
        );
    }
    let discovery = reconciler.discover().await;
    log_discovery(&discovery);
    let (inventory_sender, inventory) = watch::channel(Arc::new(discovery));
    tokio::spawn(refresh_inventory(
        inventory_sender,
        config.inventory_interval,
        reconciler.clone(),
    ));
    let (telemetry_sender, telemetry_receiver) = mpsc::channel(256);
    let telemetry = config.controller.as_ref().map(|_| telemetry_sender.clone());
    let (logout_sender, logout_receiver) = mpsc::channel(1);
    let logout = config.controller.as_ref().map(|_| logout_sender);
    if let Some(controller) = config.controller.clone() {
        let remote_discovery = inventory.clone();
        let state_dir = args.state_dir.clone();
        let oidc_callback_listen = args.oidc_callback_listen;
        let remote_enrollment = enrollment.clone();
        tokio::spawn(remote::run(
            controller,
            remote_discovery,
            state_dir,
            oidc_callback_listen,
            reconciler,
            remote_enrollment,
            controller_status.clone(),
            remote::Requests {
                telemetry: telemetry_receiver,
                logout: logout_receiver,
            },
        ));
    }
    let has_controller = config.controller.is_some();
    let state = api::AppState {
        config,
        daemon_info,
        discovery: inventory,
        enrollment,
        controller_status: has_controller.then_some(controller_status),
        state_dir: args.state_dir,
        oidc_callback_listen: args.oidc_callback_listen,
        telemetry,
        logout,
    };
    let app = api::router(state.clone());
    // The proxy is optional at runtime too: if serving fails (for example the
    // TLS root store cannot be built), log it and keep the daemon running
    // without the proxy rather than dropping the device out of the fleet.
    let proxy = async move {
        if let Some(listener) = proxy_listener
            && let Err(error) = llm_proxy::serve(
                listener,
                state,
                llm_proxy::ProxyConfig {
                    default_client_id: args.llm_proxy_client_id,
                    pairing: pairing.expect("pairing exists whenever the listener does"),
                },
            )
            .await
        {
            tracing::error!(
                error = %format!("{error:#}"),
                "local LLM proxy stopped; proxy unavailable until the daemon restarts"
            );
        }
        std::future::pending::<anyhow::Result<()>>().await
    };

    tracing::info!(socket = %socket.display(), "agent daemon listening");
    #[cfg(unix)]
    let local_api = serve_unix(&socket, local_api_access()?, app, shutdown);
    #[cfg(windows)]
    let local_api = serve_named_pipe(&socket, app, shutdown);
    tokio::select! {
        result = local_api => result?,
        result = proxy => result?,
    }

    Ok(())
}

/// Load or create the pairing value for a bound proxy. Without a pairing value
/// the proxy would accept anyone on the host, so a failure to obtain one
/// switches the proxy off like a failed bind: the listener is dropped and
/// daemon-info reports `bound: false` with the reason.
fn attach_llm_proxy_pairing(
    proxy_listener: &mut Option<tokio::net::TcpListener>,
    llm_proxy: &mut Option<LlmProxyInfo>,
    state_dir: &Path,
) -> Option<std::sync::Arc<str>> {
    proxy_listener.as_ref()?;
    match llm_proxy::load_or_create_pairing(state_dir) {
        Ok(pairing) => Some(pairing),
        Err(error) => {
            tracing::error!(
                error = %format!("{error:#}"),
                "LLM proxy pairing unavailable; proxy disabled until the daemon restarts"
            );
            *proxy_listener = None;
            if let Some(info) = llm_proxy.as_mut() {
                info.bound = false;
                info.error = Some(format!("pairing unavailable: {error:#}"));
            }
            None
        }
    }
}

/// Bind the loopback LLM proxy listener, if one is configured.
///
/// Returns the listener and the state to report through daemon-info. A bind
/// failure is logged and reported as `bound: false` rather than propagated:
/// the proxy is optional, the rest of the daemon is not.
///
/// Invariant for callers: this runs before the initial `reconciler.apply`, so a
/// reconciler that writes the proxy address into a client file must take the
/// address from this result and must not write it when `bound` is false. The
/// accept loop starts later, after discovery; connections in between queue in
/// the listen backlog.
async fn bind_llm_proxy(
    listen: Option<SocketAddr>,
    client_id: &str,
) -> (Option<tokio::net::TcpListener>, Option<LlmProxyInfo>) {
    let Some(address) = listen else {
        return (None, None);
    };
    match tokio::net::TcpListener::bind(address).await {
        Ok(listener) => {
            let bound = listener.local_addr().unwrap_or(address);
            tracing::info!(address = %bound, "local LLM proxy listening");
            (
                Some(listener),
                Some(LlmProxyInfo {
                    listen: bound.to_string(),
                    bound: true,
                    client_id: client_id.to_owned(),
                    error: None,
                }),
            )
        }
        Err(error) => {
            tracing::error!(
                address = %address,
                %error,
                "local LLM proxy could not bind; proxy and its GitHub sign-in step disabled until the daemon restarts"
            );
            (
                None,
                Some(LlmProxyInfo {
                    listen: address.to_string(),
                    bound: false,
                    client_id: client_id.to_owned(),
                    error: Some(error.to_string()),
                }),
            )
        }
    }
}

fn describe_daemon(
    config: &config::DaemonConfig,
    config_path: &Path,
    state_directory: &Path,
    user: bool,
    llm_proxy: Option<LlmProxyInfo>,
) -> DaemonInfo {
    let controller = config
        .controller
        .as_ref()
        .map(|controller| DaemonControllerInfo {
            address: controller_address_for_display(&controller.address),
            ca_certificate_path: controller
                .ca_certificate_path
                .as_ref()
                .map(|path| path_for_display(path)),
            heartbeat_interval: controller.heartbeat_interval,
        });
    DaemonInfo {
        version: VERSION.to_owned(),
        scope: if user {
            DaemonScope::User
        } else {
            DaemonScope::System
        },
        config_path: path_for_display(config_path),
        state_directory: path_for_display(state_directory),
        inventory_interval: config.inventory_interval,
        controller,
        llm_proxy,
    }
}

fn path_for_display(path: &Path) -> String {
    // Diagnostics must not stop startup if an optional path is empty or the
    // working directory cannot be resolved. Only this display copy is lossy;
    // filesystem operations keep the original native path.
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_owned())
        .to_string_lossy()
        .into_owned()
}

fn controller_address_for_display(address: &str) -> String {
    let Some(mut url) = url::Url::parse(address)
        .ok()
        .filter(|url| url.scheme() == "https" && url.has_host())
    else {
        return "Invalid controller address".to_owned();
    };
    if url.set_username("").is_err() || url.set_password(None).is_err() {
        return "Invalid controller address".to_owned();
    }
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

fn log_discovery(discovery: &agentdesktop_core::model::Discovery) {
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
    for runtime in &discovery.model_runtimes {
        tracing::info!(
            kind = %runtime.kind,
            models = runtime.models.len(),
            "discovered model runtime"
        );
    }
}

/// Re-runs discovery on an interval so the inventory reflects tools installed,
/// removed, or reconfigured after the daemon started.
///
/// The snapshot is published only when it differs from the previous one, so
/// idle devices neither log nor wake the controller stream.
async fn refresh_inventory(
    sender: watch::Sender<Arc<agentdesktop_core::model::Discovery>>,
    interval: Duration,
    reconciler: reconcile::Reconciler,
) {
    refresh_inventory_with(sender, interval, || reconciler.discover()).await;
}

async fn refresh_inventory_with<F, Fut>(
    sender: watch::Sender<Arc<agentdesktop_core::model::Discovery>>,
    interval: Duration,
    mut discover: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = agentdesktop_core::model::Discovery>,
{
    // Configuration rejects a zero interval, but `interval_at` panics on one,
    // so refuse it here too rather than leaving a landmine for another caller.
    if interval.is_zero() {
        tracing::error!("inventory interval must be greater than zero; not refreshing inventory");
        return;
    }
    let mut ticker = time::interval_at(time::Instant::now() + interval, interval);
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if sender.is_closed() {
            return;
        }
        let discovery = discover().await;
        if **sender.borrow() == discovery {
            continue;
        }
        tracing::info!("inventory changed");
        log_discovery(&discovery);
        if sender.send(Arc::new(discovery)).is_err() {
            return;
        }
    }
}

fn start_gateway_authentication(
    config: &agentdesktop_core::config::DaemonConfig,
    state_dir: PathBuf,
    callback_listen: Option<SocketAddr>,
    proxy_enabled: bool,
) {
    let Some(gateway) = config.llm_gateway.as_ref() else {
        return;
    };
    let authentication = gateway.authentication.clone();
    let github = gateway.github_oauth.clone().filter(|_| proxy_enabled);
    let subscription = config.programs.claude_code.as_ref().is_some_and(|program| {
        program.auth == Some(agentdesktop_core::config::ProgramAuthentication::Subscription)
    }) || config
        .programs
        .claude_desktop
        .as_ref()
        .is_some_and(|program| {
            program.auth == Some(agentdesktop_core::config::ProgramAuthentication::Subscription)
        });
    if authentication.is_none() && !subscription {
        return;
    }
    tokio::spawn(async move {
        let result: anyhow::Result<()> = async {
            let mut continue_in_browser = false;
            if let Some(agentdesktop_core::config::LlmGatewayAuthentication::Oidc {
                issuer,
                client_id,
                redirect_uri,
                scopes,
                allow_insecure,
                device_authorization,
            }) = authentication
            {
                if device_authorization {
                    // Logs the verification URL and code, then signs in in the
                    // background once the user approves from any device.
                    tracing::info!(%issuer, "starting LLM gateway OIDC device authorization");
                    gateway_oidc::device_login(
                        &issuer,
                        &client_id,
                        &scopes,
                        allow_insecure,
                        &state_dir,
                    )
                    .await?;
                    return Ok(());
                }
                tracing::info!(%issuer, "starting LLM gateway OIDC authentication");
                let acquired = gateway_oidc::credential(
                    &issuer,
                    &client_id,
                    &redirect_uri,
                    &scopes,
                    allow_insecure,
                    &state_dir,
                    gateway_oidc::LoginOptions {
                        callback_listen,
                        subscription_available: subscription,
                        // Only the device flow has anything for the user to
                        // sign in to. With source: request the credential comes
                        // from the client, so the sign-in page must not offer a
                        // GitHub step that would never be used.
                        github_client_id: github.as_ref().and_then(|github| {
                            matches!(github.source, GitHubTokenSource::DeviceFlow)
                                .then(|| github.client_id.clone())
                                .flatten()
                        }),
                        device_authorization: false,
                    },
                )
                .await?;
                continue_in_browser = acquired.interactive && subscription;
                tracing::info!(%issuer, "LLM gateway OIDC authentication ready");
            }
            if subscription {
                tracing::info!("starting Anthropic subscription authentication");
                crate::anthropic_oauth::credential(
                    &state_dir,
                    callback_listen,
                    !continue_in_browser,
                )
                .await?;
                tracing::info!("Anthropic subscription authentication ready");
            }
            if let Some(github) = github {
                // source: request has no credential of its own to acquire; the
                // client supplies one per request.
                if let (GitHubTokenSource::DeviceFlow, Some(client_id)) =
                    (github.source, github.client_id.as_deref())
                {
                    tracing::info!("starting GitHub App authentication");
                    crate::github_oauth::credential(client_id, &state_dir, None).await?;
                    tracing::info!("GitHub App authentication ready");
                }
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::error!(
                error = %format!("{error:#}"),
                "LLM gateway authentication failed"
            );
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
    if config.programs.copilot.is_some() {
        bail!(
            "--once cannot manage the GitHub Copilot CLI providers file because its local proxy requires the daemon to remain running"
        );
    }
    if config.programs.vscode.is_some() {
        bail!(
            "--once cannot manage the VS Code chatLanguageModels.json and settings.json files because their local proxy requires the daemon to remain running"
        );
    }
    let authenticated_gateway_is_used = config
        .llm_gateway
        .as_ref()
        .is_some_and(|gateway| gateway.authentication.is_some())
        && [
            config
                .programs
                .claude_code
                .as_ref()
                .is_some_and(|program| program.use_llm_gateway),
            config
                .programs
                .claude_desktop
                .as_ref()
                .is_some_and(|program| program.use_llm_gateway),
            config
                .programs
                .codex
                .as_ref()
                .is_some_and(|program| program.use_llm_gateway),
            config
                .programs
                .open_code
                .as_ref()
                .is_some_and(|program| program.use_llm_gateway),
            config
                .programs
                .grok
                .as_ref()
                .is_some_and(|program| program.use_llm_gateway),
            config
                .programs
                .copilot
                .as_ref()
                .is_some_and(|program| program.use_llm_gateway),
            config
                .programs
                .vscode
                .as_ref()
                .is_some_and(|program| program.use_llm_gateway),
        ]
        .into_iter()
        .any(|used| used);
    if authenticated_gateway_is_used {
        bail!(
            "--once cannot configure an authenticated LLM gateway because credential helpers require the daemon to remain running"
        );
    }
    Ok(())
}

async fn serve_local_connection<I>(stream: I, app: axum::Router) -> Result<(), hyper::Error>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let service = TowerToHyperService::new(app);
    hyper::server::conn::http1::Builder::new()
        .serve_connection(TokioIo::new(stream), service)
        // Local clients use one connection per request and may close as soon
        // as the response body is complete. Drop the IPC handle instead of
        // redundantly shutting down an already-closed socket or pipe.
        .without_shutdown()
        .await
        .map(|_| ())
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
                let app = app.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_local_connection(stream, app).await {
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
                let app = app.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_local_connection(connected, app).await {
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

fn agentdesktop_client_executable() -> anyhow::Result<PathBuf> {
    let executable = std::env::current_exe().context("locate agentdesktop executable")?;
    Ok(client_executable_for_daemon(&executable))
}

fn client_executable_for_daemon(executable: &Path) -> PathBuf {
    if executable.file_name().is_some_and(|name| {
        name.to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case("agentdesktop-service.exe"))
    }) {
        return executable.with_file_name("agentdesktop.exe");
    }
    executable.to_owned()
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
    use std::{
        io,
        path::{Path, PathBuf},
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        task::{Context, Poll},
        time::Duration,
    };

    use super::{DaemonArgs, bind_llm_proxy};
    use agentdesktop_core::DEFAULT_SOCKET_PATH;
    use agentdesktop_core::config::{self, parse_daemon};
    use agentdesktop_core::model::{Agent, Discovery, LlmProxyInfo};
    use tokio::sync::watch;

    fn discovery(kinds: &[&str]) -> Discovery {
        Discovery {
            agents: kinds
                .iter()
                .map(|kind| Agent {
                    kind: (*kind).to_owned(),
                    executable: Path::new("/usr/bin").join(kind),
                    version: None,
                    mcp_servers: Vec::new(),
                    skills: Vec::new(),
                })
                .collect(),
            model_runtimes: Vec::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn refuses_to_schedule_a_zero_interval() {
        let (sender, receiver) = watch::channel(Arc::new(discovery(&["codex"])));
        super::refresh_inventory_with(sender, Duration::ZERO, || async {
            panic!("a zero interval must not schedule a scan")
        })
        .await;
        assert_eq!(receiver.borrow().agents.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn publishes_inventory_only_when_it_changes() {
        let (sender, mut receiver) = watch::channel(Arc::new(discovery(&["codex"])));
        let scans = Arc::new(AtomicUsize::new(0));
        let refresher = tokio::spawn({
            let scans = Arc::clone(&scans);
            super::refresh_inventory_with(sender, Duration::from_secs(60), move || {
                let scan = scans.fetch_add(1, Ordering::SeqCst);
                // The first scan repeats the boot snapshot; every later scan
                // reports a newly installed tool.
                async move {
                    if scan == 0 {
                        discovery(&["codex"])
                    } else {
                        discovery(&["codex", "cursor"])
                    }
                }
            })
        });

        // Paused time auto-advances between ticks, so this resolves on the
        // first scan that actually differs rather than on the first tick.
        receiver.changed().await.unwrap();
        assert_eq!(receiver.borrow_and_update().agents.len(), 2);
        assert!(
            scans.load(Ordering::SeqCst) >= 2,
            "an unchanged scan should not have published a snapshot"
        );

        // Repeating that same state does not republish it.
        assert!(
            tokio::time::timeout(Duration::from_secs(600), receiver.changed())
                .await
                .is_err()
        );
        refresher.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn stops_refreshing_once_every_reader_is_gone() {
        let (sender, receiver) = watch::channel(Arc::new(discovery(&["codex"])));
        let refresher = tokio::spawn(super::refresh_inventory_with(
            sender,
            Duration::from_secs(60),
            || async { discovery(&["codex", "cursor"]) },
        ));
        drop(receiver);

        assert!(
            tokio::time::timeout(Duration::from_secs(600), refresher)
                .await
                .is_ok()
        );
    }

    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

    use super::{
        client_executable_for_daemon, controller_address_for_display, describe_daemon,
        serve_local_connection, validate_dry_run, validate_one_shot,
    };

    #[test]
    fn daemon_information_reports_standalone_defaults_and_resolved_paths() {
        let config = parse_daemon("{}").unwrap();
        let info = describe_daemon(
            &config,
            Path::new("config.yaml"),
            Path::new("state"),
            true,
            None,
        );

        assert_eq!(info.version, agentdesktop_core::VERSION);
        assert_eq!(info.scope, super::DaemonScope::User);
        assert_eq!(
            info.config_path,
            std::path::absolute("config.yaml")
                .unwrap()
                .to_string_lossy()
        );
        assert_eq!(
            info.state_directory,
            std::path::absolute("state").unwrap().to_string_lossy()
        );
        assert_eq!(info.inventory_interval, Duration::from_secs(15 * 60));
        assert!(info.controller.is_none());
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["scope"], "user");
        assert_eq!(json["inventoryInterval"], "15m");
        assert_eq!(
            serde_json::from_value::<super::DaemonInfo>(json).unwrap(),
            info
        );
    }

    #[test]
    fn controller_display_omits_url_secrets_and_fails_closed_on_invalid_urls() {
        for address in [
            "https://controller.example.com:8443/fleet",
            "https://test-user:test-password@controller.example.com:8443/fleet?token=test-query#test-fragment",
            "https://controller.example.com:8443/fleet?api_key=test-query#test-fragment",
        ] {
            assert_eq!(
                controller_address_for_display(address),
                "https://controller.example.com:8443/fleet"
            );
        }
        for address in [
            "https://[test-secret",
            "https://test-user:test-password@",
            "http://test-user:test-password@example.com",
            "not-a-url-test-secret",
        ] {
            assert_eq!(
                controller_address_for_display(address),
                "Invalid controller address"
            );
        }
    }

    #[test]
    fn daemon_information_preserves_an_empty_ca_path_without_failing_startup() {
        let config = parse_daemon(
            "controller:\n  address: https://controller.example.com\n  caCertificatePath: ''\n",
        )
        .unwrap();
        let info = describe_daemon(
            &config,
            Path::new("config.yaml"),
            Path::new("state"),
            false,
            None,
        );
        assert_eq!(
            info.controller.unwrap().ca_certificate_path.as_deref(),
            Some("")
        );
    }

    #[tokio::test]
    async fn daemon_information_api_is_read_only_and_reports_the_loaded_local_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("config.yaml");
        std::fs::write(
            &config_path,
            r#"
controller:
  address: https://test-user:test-password@controller.example.com:8443/fleet?token=test-query#test-fragment
  caCertificatePath: certs/controller-ca.pem
  heartbeatInterval: 45s
inventoryInterval: 2m
programs:
  claudeCode:
    env:
      API_KEY: test-program-secret
"#,
        )
        .unwrap();
        let config = agentdesktop_core::config::load_daemon(&config_path).unwrap();
        let info = describe_daemon(&config, &config_path, root.path(), false, None);
        assert_eq!(info.scope, super::DaemonScope::System);
        let controller = info.controller.as_ref().unwrap();
        assert_eq!(controller.heartbeat_interval, Duration::from_secs(45));
        assert_eq!(
            controller.ca_certificate_path.as_deref(),
            Some(
                std::path::absolute("certs/controller-ca.pem")
                    .unwrap()
                    .to_string_lossy()
                    .as_ref()
            )
        );

        // Later disk edits and cached controller policy must not replace the
        // local settings actually used by the running connection worker.
        std::fs::write(
            &config_path,
            "controller:\n  address: https://edited.example.com\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("remote-config.yaml"),
            "controller:\n  address: https://remote.example.com\ninventoryInterval: 3m\n",
        )
        .unwrap();
        let (_, inventory) = watch::channel(Arc::new(discovery(&[])));
        let app = crate::api::router(crate::api::AppState {
            config,
            daemon_info: info.clone(),
            discovery: inventory,
            enrollment: crate::enrollment::EnrollmentState::new(true),
            controller_status: None,
            state_dir: root.path().to_owned(),
            oidc_callback_listen: None,
            telemetry: None,
            logout: None,
        });

        for (method, expected_status) in [("GET", "200 OK"), ("POST", "405 Method Not Allowed")] {
            let (mut client, server) = tokio::io::duplex(16 * 1024);
            let connection = tokio::spawn(serve_local_connection(server, app.clone()));
            client
                .write_all(
                    format!(
                        "{method} /v1/daemon-info HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).await.unwrap();
            connection.await.unwrap().unwrap();
            assert!(response.starts_with(&format!("HTTP/1.1 {expected_status}\r\n")));
            if method == "GET" {
                let (_, body) = response.split_once("\r\n\r\n").unwrap();
                assert_eq!(
                    serde_json::from_str::<super::DaemonInfo>(body).unwrap(),
                    info
                );
                let json: serde_json::Value = serde_json::from_str(body).unwrap();
                assert_eq!(json["inventoryInterval"], "2m");
                assert_eq!(json["controller"]["heartbeatInterval"], "45s");
                assert_eq!(
                    json["controller"]["address"],
                    "https://controller.example.com:8443/fleet"
                );
                for omitted in [
                    "test-user",
                    "test-password",
                    "test-query",
                    "test-fragment",
                    "test-program-secret",
                    "programs",
                    "edited.example.com",
                    "remote.example.com",
                ] {
                    assert!(
                        !body.contains(omitted),
                        "unexpected diagnostics value: {omitted}"
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn daemon_information_api_handles_non_utf8_startup_paths() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        let root = tempfile::tempdir().unwrap();
        let config_path = root
            .path()
            .join(OsString::from_vec(b"config-\xff.yaml".to_vec()));
        let state_dir = root.path().join(OsString::from_vec(b"state-\xfe".to_vec()));
        // Keep invalid-byte paths in memory: filesystems such as APFS reject
        // these filenames, but the diagnostic projection must handle them.
        let config = parse_daemon("{}").unwrap();
        let info = describe_daemon(&config, &config_path, &state_dir, true, None);
        let (_, inventory) = watch::channel(Arc::new(discovery(&[])));
        let app = crate::api::router(crate::api::AppState {
            config,
            daemon_info: info,
            discovery: inventory,
            enrollment: crate::enrollment::EnrollmentState::new(false),
            controller_status: None,
            state_dir: state_dir.clone(),
            oidc_callback_listen: None,
            telemetry: None,
            logout: None,
        });

        for endpoint in ["/v1/health", "/v1/config", "/v1/daemon-info"] {
            let (mut client, server) = tokio::io::duplex(16 * 1024);
            let connection = tokio::spawn(serve_local_connection(server, app.clone()));
            client
                .write_all(
                    format!(
                        "GET {endpoint} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            let mut response = String::new();
            client.read_to_string(&mut response).await.unwrap();
            connection.await.unwrap().unwrap();
            assert!(
                response.starts_with("HTTP/1.1 200 OK\r\n"),
                "{endpoint}: {response}"
            );
            if endpoint == "/v1/daemon-info" {
                let (_, body) = response.split_once("\r\n\r\n").unwrap();
                let json: serde_json::Value = serde_json::from_str(body).unwrap();
                assert_eq!(json["configPath"], config_path.to_string_lossy().as_ref());
                assert_eq!(json["stateDirectory"], state_dir.to_string_lossy().as_ref());
                assert_eq!(json["version"], agentdesktop_core::VERSION);
                assert_eq!(json["scope"], "user");
                assert_eq!(json["inventoryInterval"], "15m");
                assert!(json["controller"].is_null());

                // The native desktop must be able to decode and re-serialize the snapshot.
                let decoded: super::DaemonInfo = serde_json::from_str(body).unwrap();
                assert_eq!(serde_json::to_value(decoded).unwrap(), json);
            }
        }

        // Only the display snapshot may be lossy, not the native input paths.
        assert!(config_path.to_str().is_none());
        assert!(state_dir.to_str().is_none());
    }

    struct ShutdownRejectingStream {
        inner: tokio::io::DuplexStream,
        shutdown_called: Arc<AtomicBool>,
    }

    impl AsyncRead for ShutdownRejectingStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
            buffer: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(context, buffer)
        }
    }

    impl AsyncWrite for ShutdownRejectingStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
            buffer: &[u8],
        ) -> Poll<Result<usize, io::Error>> {
            Pin::new(&mut self.inner).poll_write(context, buffer)
        }

        fn poll_flush(
            mut self: Pin<&mut Self>,
            context: &mut Context<'_>,
        ) -> Poll<Result<(), io::Error>> {
            Pin::new(&mut self.inner).poll_flush(context)
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
        ) -> Poll<Result<(), io::Error>> {
            self.shutdown_called.store(true, Ordering::SeqCst);
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "socket is not connected",
            )))
        }
    }

    #[tokio::test]
    async fn local_connection_drops_ipc_stream_without_shutting_it_down() {
        let (mut client, server) = tokio::io::duplex(4096);
        let shutdown_called = Arc::new(AtomicBool::new(false));
        let stream = ShutdownRejectingStream {
            inner: server,
            shutdown_called: shutdown_called.clone(),
        };
        let app = axum::Router::new().route("/health", axum::routing::get(|| async { "ok" }));
        let connection = tokio::spawn(serve_local_connection(stream, app));

        client
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();

        connection.await.unwrap().unwrap();
        assert!(String::from_utf8_lossy(&response).ends_with("\r\n\r\nok"));
        assert!(!shutdown_called.load(Ordering::SeqCst));
    }

    #[test]
    fn windows_service_uses_the_sibling_client_executable() {
        assert_eq!(
            client_executable_for_daemon(Path::new(
                "/Program Files/Agent Desktop/agentdesktop-service.exe"
            )),
            Path::new("/Program Files/Agent Desktop/agentdesktop.exe")
        );
        assert_eq!(
            client_executable_for_daemon(Path::new("/usr/bin/agentdesktop")),
            Path::new("/usr/bin/agentdesktop")
        );
    }

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
llmGateway:
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
    fn one_shot_rejects_the_copilot_program() {
        let copilot = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  copilot:
    models:
      gpt-4.1: {}
"#,
        )
        .unwrap();
        assert!(
            validate_one_shot(&copilot)
                .unwrap_err()
                .to_string()
                .contains("Copilot")
        );
    }

    #[test]
    fn one_shot_rejects_the_vscode_program() {
        let vscode = parse_daemon(
            r#"
llmGateway:
  url: https://gateway.example.com
programs:
  vscode:
    models:
      gpt-4.1-mini: {}
"#,
        )
        .unwrap();
        assert!(
            validate_one_shot(&vscode)
                .unwrap_err()
                .to_string()
                .contains("VS Code")
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
    fn daemon_args(user: bool) -> DaemonArgs {
        DaemonArgs {
            user,
            once: false,
            dry_run: false,
            config: None,
        }
    }

    fn resolve_error(args: DaemonArgs, startup: config::DaemonStartupConfig) -> anyhow::Error {
        match args.resolve(
            startup,
            PathBuf::from("config.yaml"),
            PathBuf::from(DEFAULT_SOCKET_PATH),
        ) {
            Ok(_) => panic!("resolve succeeded unexpectedly"),
            Err(error) => error,
        }
    }

    fn startup_with_llm_proxy(listen: &str) -> config::DaemonStartupConfig {
        let mut startup = config::DaemonStartupConfig::default();
        startup.llm_proxy.listen = Some(listen.parse().unwrap());
        startup
    }

    #[test]
    fn system_mode_rejects_a_copilot_providers_path() {
        let mut startup = config::DaemonStartupConfig::default();
        startup.copilot.config = Some(PathBuf::from("/tmp/providers.json"));
        let error = resolve_error(daemon_args(false), startup);
        assert!(
            error
                .to_string()
                .contains("daemon.copilot.config requires --user"),
            "{error:#}"
        );
    }

    #[test]
    fn system_mode_rejects_a_vscode_settings_path() {
        let mut startup = config::DaemonStartupConfig::default();
        startup.vscode.settings = Some(PathBuf::from("/tmp/settings.json"));
        let error = resolve_error(daemon_args(false), startup);
        assert!(
            error
                .to_string()
                .contains("daemon.vscode.settings requires --user"),
            "{error:#}"
        );
    }

    #[test]
    fn system_mode_rejects_a_vscode_chat_models_path() {
        let mut startup = config::DaemonStartupConfig::default();
        startup.vscode.config = Some(PathBuf::from("/tmp/chatLanguageModels.json"));
        let error = resolve_error(daemon_args(false), startup);
        assert!(
            error
                .to_string()
                .contains("daemon.vscode.config requires --user"),
            "{error:#}"
        );
    }

    #[test]
    fn system_mode_rejects_a_configured_llm_proxy() {
        let error = resolve_error(
            daemon_args(false),
            startup_with_llm_proxy("127.0.0.1:18095"),
        );
        assert!(
            error.to_string().contains("requires --user"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn system_mode_runs_no_llm_proxy_by_default() {
        let resolved = daemon_args(false)
            .resolve(
                config::DaemonStartupConfig::default(),
                PathBuf::from("config.yaml"),
                PathBuf::from(DEFAULT_SOCKET_PATH),
            )
            .unwrap();
        assert_eq!(resolved.llm_proxy_listen, None);
    }

    #[test]
    fn user_mode_keeps_the_configured_llm_proxy_address_and_default_client_id() {
        // Pin the state dir and socket so XDG variables do not matter. resolve()
        // still needs a home directory (HOME / USERPROFILE), like every user-mode
        // daemon start.
        let mut startup = startup_with_llm_proxy("127.0.0.1:18095");
        startup.state_dir = Some(PathBuf::from("/tmp/agentdesktop-test-state"));
        startup.socket = Some(PathBuf::from("/tmp/agentdesktop-test.sock"));
        let resolved = daemon_args(true)
            .resolve(
                startup,
                PathBuf::from("config.yaml"),
                PathBuf::from(DEFAULT_SOCKET_PATH),
            )
            .unwrap();
        assert_eq!(
            resolved.llm_proxy_listen,
            Some("127.0.0.1:18095".parse().unwrap())
        );
        assert_eq!(resolved.llm_proxy_client_id, "vscode");
    }

    #[test]
    fn client_id_is_only_validated_when_a_listen_address_is_set() {
        let mut startup = config::DaemonStartupConfig::default();
        startup.llm_proxy.client_id = Some("not valid!".to_owned());
        let resolved = daemon_args(false)
            .resolve(
                startup.clone(),
                PathBuf::from("config.yaml"),
                PathBuf::from(DEFAULT_SOCKET_PATH),
            )
            .map(|_| ());
        assert!(
            resolved.is_ok(),
            "clientId without listen must not fail startup"
        );
        let mut user_startup = startup.clone();
        user_startup.state_dir = Some(PathBuf::from("/tmp/agentdesktop-test-state"));
        user_startup.socket = Some(PathBuf::from("/tmp/agentdesktop-test.sock"));
        let resolved = daemon_args(true)
            .resolve(
                user_startup,
                PathBuf::from("config.yaml"),
                PathBuf::from(DEFAULT_SOCKET_PATH),
            )
            .map(|resolved| resolved.llm_proxy_listen);
        assert!(
            matches!(resolved, Ok(None)),
            "clientId without listen is inert in user mode too"
        );
        startup.llm_proxy.listen = Some("127.0.0.1:18095".parse().unwrap());
        let error = resolve_error(daemon_args(true), startup);
        assert!(error.to_string().contains("clientId"));
    }

    #[test]
    fn llm_proxy_rejects_non_loopback_and_one_shot_runs() {
        let error = resolve_error(daemon_args(true), startup_with_llm_proxy("0.0.0.0:18095"));
        assert!(error.to_string().contains("loopback"));
        let mut once = daemon_args(true);
        once.once = true;
        let error = resolve_error(once, startup_with_llm_proxy("127.0.0.1:18095"));
        assert!(error.to_string().contains("--once"));
        let mut dry_run = daemon_args(true);
        dry_run.dry_run = true;
        let error = resolve_error(dry_run, startup_with_llm_proxy("127.0.0.1:18095"));
        assert!(error.to_string().contains("--once"));
    }

    #[test]
    fn system_mode_reports_the_user_mode_rule_before_other_checks() {
        // A system-mode operator with a non-loopback address must see the real
        // reason, not the loopback rule.
        let error = resolve_error(daemon_args(false), startup_with_llm_proxy("0.0.0.0:18095"));
        assert!(error.to_string().contains("requires --user"), "{error:#}");
    }

    #[tokio::test]
    async fn llm_proxy_bind_failure_is_reported_not_fatal() {
        let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = taken.local_addr().unwrap();
        let (listener, info) = bind_llm_proxy(Some(address), "vscode").await;
        assert!(listener.is_none());
        let info = info.expect("proxy info");
        assert_eq!(info.listen, address.to_string());
        assert!(!info.bound);
        assert_eq!(info.client_id, "vscode");
        assert!(info.error.as_deref().is_some_and(|error| !error.is_empty()));
    }

    #[tokio::test]
    async fn llm_proxy_bind_reports_the_bound_address() {
        let (listener, info) =
            bind_llm_proxy(Some("127.0.0.1:0".parse().unwrap()), "copilot-cli").await;
        let listener = listener.expect("listener bound");
        let info = info.expect("proxy info");
        assert!(info.bound);
        assert_eq!(info.listen, listener.local_addr().unwrap().to_string());
        assert_eq!(info.client_id, "copilot-cli");
        let (none, info) = bind_llm_proxy(None, "vscode").await;
        assert!(none.is_none() && info.is_none());
    }

    #[tokio::test]
    async fn pairing_failure_switches_the_proxy_off_and_reports_it() {
        // A state directory that is a regular file: reading the pairing file
        // fails (not a directory), which is neither "missing" nor "unusable", so
        // no pairing is created and the proxy is switched off.
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("state-as-file");
        std::fs::write(&blocked, b"not a directory").unwrap();
        let (mut listener, mut info) =
            bind_llm_proxy(Some("127.0.0.1:0".parse().unwrap()), "copilot-cli").await;
        assert!(listener.is_some());
        let pairing = super::attach_llm_proxy_pairing(&mut listener, &mut info, &blocked);
        assert!(pairing.is_none());
        assert!(
            listener.is_none(),
            "the listener is dropped when no pairing exists"
        );
        let info = info.expect("proxy info");
        assert!(!info.bound);
        assert!(
            info.error
                .as_deref()
                .is_some_and(|error| error.starts_with("pairing unavailable"))
        );
        // A usable state directory keeps the listener and yields the pairing.
        let (mut listener, mut info) =
            bind_llm_proxy(Some("127.0.0.1:0".parse().unwrap()), "copilot-cli").await;
        let pairing = super::attach_llm_proxy_pairing(&mut listener, &mut info, dir.path());
        assert!(pairing.is_some() && listener.is_some() && info.unwrap().bound);
    }

    #[test]
    fn daemon_information_reports_the_llm_proxy_state() {
        let config = parse_daemon("{}").unwrap();
        let proxy = LlmProxyInfo {
            listen: "127.0.0.1:18095".to_owned(),
            bound: true,
            client_id: "vscode".to_owned(),
            error: None,
        };
        let info = describe_daemon(
            &config,
            Path::new("config.yaml"),
            Path::new("state"),
            true,
            Some(proxy.clone()),
        );
        let json = serde_json::to_value(&info).unwrap();
        assert_eq!(json["llmProxy"]["bound"], serde_json::Value::Bool(true));
        assert_eq!(json["llmProxy"]["listen"], "127.0.0.1:18095");
        assert_eq!(json["llmProxy"]["clientId"], "vscode");
        assert!(json["llmProxy"].get("error").is_none());
        let without = describe_daemon(
            &config,
            Path::new("config.yaml"),
            Path::new("state"),
            true,
            None,
        );
        assert!(
            serde_json::to_value(&without)
                .unwrap()
                .get("llmProxy")
                .is_none()
        );
    }
}
