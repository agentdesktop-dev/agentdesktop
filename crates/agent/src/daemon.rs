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
    model::{DaemonControllerInfo, DaemonInfo, DaemonScope, LlmProxyInfo, UsageReceiverInfo},
    telemetry,
};
use agentdesktop_usage::{UsageIngest, UsageStore};
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
use tonic::transport::server::TcpIncoming;
#[cfg(windows)]
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;

#[cfg(windows)]
use crate::windows_security::SecurityDescriptor;
use crate::{
    api, enrollment::EnrollmentState, gateway_oidc, inventory_watch::InventoryWatch, launch,
    llm_proxy, reconcile, remote, secure_fs,
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

    /// Start the user daemon without a console window (Windows). The daemon
    /// re-executes itself detached with its standard handles inherited, so
    /// the scheduled task that started it completes and no console window
    /// (and no taskbar entry) exists. Requires --user.
    #[arg(long)]
    hidden: bool,
}

struct ResolvedDaemonArgs {
    user: bool,
    config: PathBuf,
    state_dir: PathBuf,
    socket: PathBuf,
    oidc_callback_listen: Option<SocketAddr>,
    llm_proxy_listen: Option<SocketAddr>,
    llm_proxy_client_id: String,
    /// `daemon.usage`; `None` for `--once` and `--dry-run`, which serve nothing.
    usage: Option<config::UsageStartupConfig>,
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
    pi: ResolvedPiStartupConfig,
    /// `daemon.reconcileInterval`; `None` means no periodic re-apply.
    reconcile_interval: Option<Duration>,
    once: bool,
    dry_run: bool,
    /// `daemon.containment`; `None` for `--once` and `--dry-run`.
    containment: Option<config::ContainmentStartupConfig>,
    /// Whether `daemon.claudeCode.config` was set: a box reads the default path.
    claude_settings_overridden: bool,
    /// Resolved copy of `daemon.containment.egress` for the launch routes.
    containment_egress: config::ContainmentEgress,
    /// Resolved copy of `daemon.containment.engine`; selects the sandbox engine.
    containment_engine: config::ContainmentEngine,
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

struct ResolvedPiStartupConfig {
    models: Option<PathBuf>,
    settings: Option<PathBuf>,
}

impl DaemonArgs {
    fn resolve(
        self,
        startup: config::DaemonStartupConfig,
        config: PathBuf,
        socket: PathBuf,
    ) -> anyhow::Result<ResolvedDaemonArgs> {
        let user = self.user || startup.user;
        // A non-default socket only arrives here from `daemon --user --socket`
        // (run_until_shutdown refuses it for the system daemon); the flag wins
        // over `daemon.socket` so one shared, non-env-expanded config file can
        // coexist with per-user pipe names from the task line (DV6-8). A
        // `--socket` equal to the default is indistinguishable from no flag, so
        // the config value wins then (harmless for per-user names; not fixed).
        let socket = if socket != Path::new(DEFAULT_SOCKET_PATH) {
            socket
        } else {
            startup.socket.clone().unwrap_or(socket)
        };
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
        let usage = startup
            .usage
            .clone()
            .filter(|_| !(self.once || self.dry_run));
        // A sandboxed launch runs as the daemon's user with that user's gateway
        // credential; like the proxy, it has no meaning in system mode.
        let containment = startup
            .containment
            .clone()
            .filter(|_| !(self.once || self.dry_run));
        if containment.is_some() && !user {
            bail!("daemon.containment requires --user (or daemon.user: true)");
        }
        let claude_settings_overridden = startup.claude_code.config.is_some();
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
                usage,
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
                pi: {
                    if startup.pi.models.is_some() || startup.pi.settings.is_some() {
                        bail!("daemon.pi paths require --user (or daemon.user: true)");
                    }
                    ResolvedPiStartupConfig {
                        models: None,
                        settings: None,
                    }
                },
                reconcile_interval: startup.reconcile_interval,
                once: self.once || self.dry_run,
                dry_run: self.dry_run,
                containment,
                claude_settings_overridden,
                containment_egress: config::ContainmentEgress::default(),
                containment_engine: config::ContainmentEngine::default(),
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
            usage,
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
            pi: ResolvedPiStartupConfig {
                models: Some(startup.pi.models.unwrap_or_else(|| {
                    crate::provider::pi::user_pi_agent_dir(&home).join("models.json")
                })),
                settings: Some(startup.pi.settings.unwrap_or_else(|| {
                    crate::provider::pi::user_pi_agent_dir(&home).join("settings.json")
                })),
            },
            reconcile_interval: startup.reconcile_interval,
            once: self.once || self.dry_run,
            dry_run: self.dry_run,
            containment,
            claude_settings_overridden,
            containment_egress: startup
                .containment
                .as_ref()
                .map(|c| c.egress)
                .unwrap_or_default(),
            containment_engine: startup
                .containment
                .as_ref()
                .map(|c| c.engine)
                .unwrap_or_default(),
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
    // `--hidden` (Windows user daemon): re-execute without a console and let
    // this process exit. The child has no console object at all, so there is
    // no window, no minimized state and no taskbar entry; it inherits the
    // standard handles, so a task's log redirection keeps receiving output.
    #[cfg(windows)]
    if args.hidden && args.user {
        detach_windowless()?;
        return Ok(());
    }
    #[cfg(not(windows))]
    if args.hidden && args.user {
        eprintln!("agentdesktop: --hidden is only supported on Windows; starting normally");
    }
    run_until_shutdown(args, socket, async {
        tokio::signal::ctrl_c()
            .await
            .context("wait for shutdown signal")
    })
    .await
}

/// The child command line for `--hidden`: the original arguments without
/// `--hidden`, so the re-executed daemon starts normally (no second detach).
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn detached_argv<I>(args: I) -> Vec<std::ffi::OsString>
where
    I: IntoIterator<Item = std::ffi::OsString>,
{
    args.into_iter()
        .filter(|arg| arg.as_os_str() != std::ffi::OsStr::new("--hidden"))
        .collect()
}

/// Re-executes this daemon without a console, keeping the standard handles.
#[cfg(windows)]
fn detach_windowless() -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let executable = std::env::current_exe().context("resolve the daemon executable")?;
    let mut command = Command::new(executable);
    command
        .args(detached_argv(std::env::args_os().skip(1)))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .creation_flags(CREATE_NO_WINDOW);
    command.spawn().context("start the windowless daemon")?;
    Ok(())
}

fn check_socket_flag(socket: &Path, user: bool) -> anyhow::Result<()> {
    if socket != Path::new(DEFAULT_SOCKET_PATH) && !user {
        bail!("set daemon.socket in the configuration file; --socket is for client commands");
    }
    Ok(())
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
    // `daemon --user` accepts `--socket`: the per-user pipe name on Windows
    // comes from one shared scheduled-task line (`%USERNAME%` expanded at
    // start), and config `daemon.socket` values are not env-expanded, so a
    // shared config file cannot carry per-user names (DV6-8). The system
    // daemon keeps refusing the flag.
    check_socket_flag(&socket, args.user)?;
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
        args.pi.models.clone(),
        args.pi.settings.clone(),
        agentdesktop_client_executable()?,
        socket.clone(),
    )
    .with_llm_proxy(proxy_context);
    let choice = crate::program_choice::load(&args.state_dir);
    if choice != agentdesktop_core::model::ProgramChoice::All {
        tracing::info!(
            ?choice,
            "the device user chose the programs to manage; honored while the configuration sets programSelection.allowUserChoice"
        );
    }
    reconciler.set_program_choice(choice);
    if args.once {
        if args.dry_run {
            validate_dry_run(&config)?;
            reconciler
                .dry_run(&config)
                .context("preview daemon configuration")?;
        } else {
            // Programs the device user opted out of are only cleaned up, so
            // they need nothing that `--once` cannot provide.
            validate_one_shot(
                &reconcile::apply_program_choice(&config, &reconciler.program_choice()).0,
                args.user,
            )?;
            let (report, applied) = reconciler.apply_with_report(&config);
            report.log();
            applied.context("apply daemon configuration")?;
            println!("Reconciliation complete.");
        }
        return Ok(());
    }

    secure_fs::ensure_private_dir(&args.state_dir)?;
    #[cfg(windows)]
    let daemon_sid = if args.user {
        // The user-mode pipe admits this SID only; without it the daemon must
        // not fall back to the interactive-users descriptor.
        let identity =
            launch::peer_identity(std::process::id()).context("read the daemon's own user SID")?;
        Some(
            identity
                .user_sid
                .context("the daemon's token has no user SID")?,
        )
    } else {
        None
    };
    #[cfg(not(windows))]
    let daemon_sid: Option<String> = None;
    // On Windows user mode, bind the first pipe instance now so a duplicate
    // daemon fails here before mutating any state. The bound instance is
    // handed to serve_named_pipe later.
    #[cfg(windows)]
    let first_pipe_server = if args.user {
        let sddl = launch::user_mode_pipe_sddl(
            daemon_sid
                .as_ref()
                .expect("user mode has a daemon SID above"),
        );
        // Launch directories a previous daemon left behind: secrets go now, the
        // directory of a box that still runs stays until it exits. The sweep
        // runs only after bind succeeds, so a duplicate daemon mutates nothing.
        let state_dir = args.state_dir.clone();
        Some(bind_then_sweep(&socket, &sddl, || {
            if let Err(error) = launch::sweep(&state_dir, &launch::host::process_alive) {
                tracing::warn!(error = %format!("{error:#}"), "sweep of previous sandboxed launches failed");
            }
        })?)
    } else {
        None
    };
    // The sandbox engine is read at startup only; `mxc` is the only engine.
    let engine: Arc<dyn launch::engine::Engine> = match args.containment_engine {
        config::ContainmentEngine::Mxc => Arc::new(launch::engine::mxc::MxcEngine),
    };
    let containment = launch::containment_info(
        args.containment.as_ref(),
        args.user,
        cfg!(windows),
        args.claude_settings_overridden,
        &|path| path.exists(),
        engine.name(),
    );
    let (usage_store, usage_receiver) = match &args.usage {
        Some(usage) => {
            let (store, receiver) = start_usage(usage, &args.state_dir).await;
            (store, Some(receiver))
        }
        None => (None, None),
    };
    let daemon_info = DaemonInfo {
        usage_receiver,
        containment: containment.clone(),
        ..describe_daemon(&config, &args.config, &args.state_dir, args.user, llm_proxy)
    };
    // A program the device user opted out of does not ask for a sign-in, and
    // with every gateway program opted out nothing does.
    if reconciler.gateway_sign_in_wanted(&config) {
        start_gateway_authentication(
            &reconcile::apply_program_choice(&config, &reconciler.program_choice()).0,
            args.user,
            args.state_dir.clone(),
            args.oidc_callback_listen,
            proxy_listener.is_some(),
        );
    }
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
    if let Some(initial_config) = &initial_config {
        // The outcomes are logged before a failure stops the daemon.
        let (report, applied) = reconciler.apply_with_report(initial_config);
        report.log();
        applied.context("apply initial daemon configuration")?;
        // With a controller, the cached controller configuration is what is in effect
        // from here, and start_gateway_authentication above only saw the local file.
        // Without one the two are the same configuration and it already ran.
        if config.controller.is_some() && reconciler.gateway_sign_in_wanted(initial_config) {
            gateway_oidc::start_device_login_if_configured(initial_config, &args.state_dir);
        }
    } else {
        tracing::info!(
            "preserving managed files until the controller provides daemon configuration"
        );
    }
    // The current configuration, re-applied by the reconcile tick; held here
    // for the daemon's lifetime and replaced by each pushed configuration.
    let (current_config, current_config_receiver) =
        watch::channel(initial_config.map(|config| crate::tick::CurrentConfig {
            revision: None,
            config: Arc::new(config),
        }));
    // Re-apply outcomes, from the tick and from program choice changes, go to
    // the controller.
    let (tick_status_sender, tick_status_receiver) = watch::channel(None);
    let reapply_statuses = config.controller.is_some().then_some(tick_status_sender);
    let program_control = api::ProgramControl::new(
        reconciler.clone(),
        current_config.subscribe(),
        reapply_statuses.clone(),
        args.state_dir.clone(),
    );
    if let Some(interval) = args.reconcile_interval {
        tracing::info!(
            interval_seconds = interval.as_secs_f64(),
            "reconcile tick enabled"
        );
        tokio::spawn(crate::tick::run_tick(
            reconciler.clone(),
            current_config_receiver,
            interval,
            reapply_statuses,
        ));
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
        let remote_current_config = current_config.clone();
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
                current: remote_current_config,
                tick_statuses: Some(tick_status_receiver),
            },
        ));
    }
    let has_controller = config.controller.is_some();
    // The violation ledger exists only in a Windows user-mode daemon whose
    // `daemon.containment` is present: the same daemons whose launch routes
    // work. The system daemon receives the same policy and must not scan.
    let violation_ledger = (cfg!(windows) && args.user && args.containment.is_some())
        .then(|| std::sync::Arc::new(std::sync::Mutex::new(launch::wrap::ViolationLedger::new())));
    // A device that receives a `containment` policy block without the local
    // `daemon.containment` prerequisite reports no posture and refuses
    // launches with the existing 409; warn so the gap is visible, at startup
    // (the initial watch value) and on each applied configuration. Windows
    // only: launches, the scan and the posture exist only there, so a macOS
    // or Linux user daemon receiving the same fleet policy stays quiet.
    if cfg!(windows) && args.user && args.containment.is_none() {
        let mut configs = current_config.subscribe();
        tokio::spawn(async move {
            loop {
                {
                    let current = configs.borrow_and_update();
                    if current
                        .as_ref()
                        .is_some_and(|current| current.config.containment.is_some())
                    {
                        tracing::warn!(
                            "the applied configuration carries a containment policy, but daemon.containment is not configured on this device: launches are refused and no wrap posture is reported"
                        );
                    }
                }
                if configs.changed().await.is_err() {
                    return;
                }
            }
        });
    }
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
        usage: usage_store,
        programs: Some(program_control),
        violation_ledger: violation_ledger.clone(),
    };
    // Native runs of a wrap-mode program's managed executable become posture
    // violations: a report-only scan every VIOLATION_SCAN_INTERVAL, reading
    // the applied configuration once per tick.
    #[cfg(windows)]
    if let Some(ledger) = violation_ledger {
        let daemon_user_sid = daemon_sid
            .clone()
            .expect("a Windows user-mode daemon read its own SID above");
        tokio::spawn(launch::wrap::scan_loop(
            state.config.clone(),
            state.state_dir.clone(),
            daemon_user_sid,
            ledger,
            launch::wrap::native_process_snapshot,
            || {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_secs())
                    .unwrap_or(0)
            },
        ));
    }
    #[cfg(not(windows))]
    drop(violation_ledger);
    // Sandbox sessions (`/sandbox` route): one registry shared by the proxy,
    // the local API that mints and revokes sessions, and the sweep.
    let sessions = llm_proxy::sessions::SessionRegistry::default();
    let session_policy = std::sync::Arc::new(llm_proxy::sessions::ConfigPolicy {
        config: state.config.clone(),
        state_dir: state.state_dir.clone(),
    });
    let session_api = llm_proxy::sessions::SessionApiState {
        registry: sessions.clone(),
        proxy_address: proxy_listener
            .as_ref()
            .and_then(|listener| listener.local_addr().ok()),
        policy: session_policy.clone(),
    };
    let launch_state = launch::LaunchState {
        app: state.clone(),
        executor: Arc::new(launch::host::ProcessExecutor),
        credentials: Arc::new(launch::host::GatewayCredentialSource { app: state.clone() }),
        host: Arc::new(launch::host::HostEnvironment::new(
            home_directory().unwrap_or_default(),
            state.state_dir.clone(),
            &agentdesktop_client_executable()?,
        )),
        registry: launch::LaunchRegistry::new(),
        availability: containment,
        daemon_sid: daemon_sid.clone(),
        sessions: sessions.clone(),
        proxy_address: proxy_listener
            .as_ref()
            .and_then(|listener| listener.local_addr().ok()),
        session_policy,
        egress: args.containment_egress,
        git: args.containment.as_ref().and_then(|c| c.git.clone()),
        claude_settings_overridden: args.claude_settings_overridden,
        engine,
    };
    let app = api::router(state.clone())
        .merge(llm_proxy::sessions::session_router(session_api))
        .merge(launch::router(launch_state));
    let session_sweep = tokio::spawn(llm_proxy::sessions::sweep_forever(sessions.clone()));
    // The git allowlists/credentials and the central egress upstream the git
    // routes serve, read once at startup from daemon.containment (PR 4).
    let containment_git = args.containment.as_ref().and_then(|c| c.git.clone());
    let containment_egress_upstream = args
        .containment
        .as_ref()
        .and_then(|c| c.egress_upstream.as_ref())
        .map(|upstream| upstream.url.clone());
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
                    sessions,
                    git: containment_git,
                    egress_upstream: containment_egress_upstream,
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
    let local_api = serve_named_pipe(&socket, app, shutdown, daemon_sid, first_pipe_server);
    let result = tokio::select! {
        result = local_api => result,
        result = proxy => result,
    };
    session_sweep.abort();
    result?;

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
/// Invariant for callers: this runs before the initial apply, so a
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

/// Opens this device's usage database and starts the receiver that a gateway
/// on this machine exports usage to. Like the LLM proxy, a failure is logged
/// and reported through daemon-info instead of stopping the daemon; usage
/// already recorded is still reported whenever the database opened.
async fn start_usage(
    usage: &config::UsageStartupConfig,
    state_dir: &Path,
) -> (Option<UsageStore>, UsageReceiverInfo) {
    let off = |error: anyhow::Error| {
        tracing::error!(
            address = %usage.listen,
            error = %format!("{error:#}"),
            "usage receiver is off until the daemon restarts"
        );
        UsageReceiverInfo {
            listen: usage.listen.to_string(),
            bound: false,
            error: Some(format!("{error:#}")),
        }
    };
    let store = match UsageStore::open_sqlite(&state_dir.join("usage.db")).await {
        Ok(store) => store,
        Err(error) => return (None, off(error)),
    };
    agentdesktop_usage::spawn_maintenance(store.clone());
    let receiver = async {
        let token = agentdesktop_usage::load_token(&usage.token_file)?;
        let listener = tokio::net::TcpListener::bind(usage.listen)
            .await
            .with_context(|| format!("bind {}", usage.listen))?;
        anyhow::Ok((token, listener))
    };
    let (token, listener) = match receiver.await {
        Ok(receiver) => receiver,
        Err(error) => return (Some(store), off(error)),
    };
    let bound = listener.local_addr().unwrap_or(usage.listen);
    tracing::info!(address = %bound, "usage receiver listening");
    let service = UsageIngest::service(store.clone(), &token);
    tokio::spawn(async move {
        if let Err(error) = tonic::transport::Server::builder()
            .add_service(service)
            .serve_with_incoming(TcpIncoming::from(listener))
            .await
        {
            tracing::error!(
                error = %format!("{error:#}"),
                "usage receiver stopped; usage is not recorded until the daemon restarts"
            );
        }
    });
    (
        Some(store),
        UsageReceiverInfo {
            listen: bound.to_string(),
            bound: true,
            error: None,
        },
    )
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
        usage_receiver: None,
        containment: None,
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
    // The watcher owns its change channel so the capacity-one coalescing it
    // relies on cannot be undone here. When it cannot start, a dropped sender
    // leaves a receiver that never yields, and the interval carries on alone.
    let (watch, changes) = match InventoryWatch::new() {
        Ok((watch, changes)) => (Some(watch), changes),
        Err(error) => {
            tracing::warn!(
                error = %format!("{error:#}"),
                "inventory file watching unavailable; refreshing on the interval only"
            );
            (None, mpsc::channel(1).1)
        }
    };
    refresh_inventory_with(sender, interval, changes, watch, || reconciler.discover()).await;
}

async fn refresh_inventory_with<F, Fut>(
    sender: watch::Sender<Arc<agentdesktop_core::model::Discovery>>,
    interval: Duration,
    mut changes: mpsc::Receiver<()>,
    mut watch: Option<InventoryWatch>,
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
    if let Some(watch) = watch.as_mut() {
        watch.sync(&sender.borrow().clone());
    }
    let mut ticker = time::interval_at(time::Instant::now() + interval, interval);
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    loop {
        // A closed change channel disables its branch rather than ending the
        // loop, so a watcher that could not start leaves the interval running.
        let trigger = tokio::select! {
            _ = ticker.tick() => "interval",
            Some(()) = changes.recv() => "change",
        };
        if sender.is_closed() {
            return;
        }
        let discovery = discover().await;
        if **sender.borrow() == discovery {
            continue;
        }
        tracing::info!(trigger, "inventory changed");
        log_discovery(&discovery);
        if let Some(watch) = watch.as_mut() {
            watch.sync(&discovery);
        }
        if sender.send(Arc::new(discovery)).is_err() {
            return;
        }
    }
}

/// Only programs managed in this mode can request subscription sign-in.
fn uses_subscription(config: &agentdesktop_core::config::DaemonConfig, user: bool) -> bool {
    use agentdesktop_core::config::ProgramAuthentication;

    config
        .programs
        .claude_code
        .as_ref()
        .is_some_and(|program| program.auth == Some(ProgramAuthentication::Subscription))
        || (!user
            && config
                .programs
                .claude_desktop
                .as_ref()
                .is_some_and(|program| program.auth == Some(ProgramAuthentication::Subscription)))
}

fn start_gateway_authentication(
    config: &agentdesktop_core::config::DaemonConfig,
    user: bool,
    state_dir: PathBuf,
    callback_listen: Option<SocketAddr>,
    proxy_enabled: bool,
) {
    let Some(gateway) = config.llm_gateway.as_ref() else {
        return;
    };
    let authentication = gateway.authentication.clone();
    let github = gateway.github_oauth.clone().filter(|_| proxy_enabled);
    let subscription = uses_subscription(config, user);
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

fn validate_one_shot(
    config: &agentdesktop_core::config::DaemonConfig,
    user: bool,
) -> anyhow::Result<()> {
    if config.controller.is_some() {
        bail!(
            "--once cannot use a controller because controller synchronization requires the daemon to remain running"
        );
    }
    if !config.telemetry.events.is_empty() {
        bail!("--once cannot collect telemetry because hooks require the daemon to remain running");
    }
    if user && config.programs.copilot.is_some() {
        bail!(
            "--once cannot manage the GitHub Copilot CLI providers file because its local proxy requires the daemon to remain running"
        );
    }
    if user && config.programs.vscode.is_some() {
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
            !user
                && config
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
            !user
                && config
                    .programs
                    .grok
                    .as_ref()
                    .is_some_and(|program| program.use_llm_gateway),
            user && config
                .programs
                .pi
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

pub(crate) async fn serve_local_connection<I>(
    stream: I,
    app: axum::Router,
) -> Result<(), hyper::Error>
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
    peer_check: Option<String>,
    first_instance: Option<tokio::net::windows::named_pipe::NamedPipeServer>,
) -> anyhow::Result<()> {
    // Full access for SYSTEM and Administrators, read/write for interactive
    // users: the system daemon serves every user of the device. A user-mode
    // daemon admits its own user only (`peer_check` is that user's SID).
    let sddl = match &peer_check {
        Some(sid) => launch::user_mode_pipe_sddl(sid),
        None => launch::SYSTEM_PIPE_SDDL.to_owned(),
    };
    let mut server = match first_instance {
        Some(s) => s,
        None => bind_named_pipe(socket, true, &sddl)?,
    };
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            connected = server.connect() => {
                connected.with_context(|| format!("accept connection on {}", socket.display()))?;
                let connected = server;
                // Keep an unconnected instance available while the accepted
                // connection is being served. Without this, clients can see a
                // transient pipe-not-found error between connections.
                server = bind_named_pipe(socket, false, &sddl)?;
                let app = match &peer_check {
                    // The launch routes check the caller: its identity rides
                    // on this connection's router as an extension.
                    Some(_) => app.clone().layer(axum::Extension(pipe_peer(&connected))),
                    None => app.clone(),
                };
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

/// The identity of the process at the client end of a freshly accepted pipe;
/// an unreadable identity is a peer with no SID, which the launch routes refuse.
#[cfg(windows)]
fn pipe_peer(connected: &NamedPipeServer) -> launch::PipePeer {
    use std::os::windows::io::AsRawHandle;

    use windows_sys::Win32::System::Pipes::GetNamedPipeClientProcessId;

    let mut pid = 0u32;
    // SAFETY: the handle is a connected pipe instance; `pid` is a valid out-parameter.
    let ok = unsafe { GetNamedPipeClientProcessId(connected.as_raw_handle() as _, &mut pid) };
    if ok == 0 {
        tracing::debug!(error = %std::io::Error::last_os_error(), "pipe client pid unavailable");
        return launch::PipePeer {
            user_sid: None,
            app_container: false,
        };
    }
    launch::peer_identity(pid).unwrap_or_else(|error| {
        tracing::debug!(pid, error = %format!("{error:#}"), "pipe client identity unavailable");
        launch::PipePeer {
            user_sid: None,
            app_container: false,
        }
    })
}

#[cfg(windows)]
fn bind_named_pipe(
    path: &std::path::Path,
    first: bool,
    sddl: &str,
) -> anyhow::Result<NamedPipeServer> {
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first)
        .reject_remote_clients(true);
    let descriptor = SecurityDescriptor::from_sddl(sddl)?;
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

/// Binds the first named pipe instance, then runs the sweep closure. A
/// duplicate daemon's bind fails here before the sweep runs, leaving state
/// untouched.
#[cfg(windows)]
fn bind_then_sweep(
    socket: &Path,
    sddl: &str,
    sweep: impl FnOnce(),
) -> anyhow::Result<NamedPipeServer> {
    let server = bind_named_pipe(socket, true, sddl)?;
    sweep();
    Ok(server)
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
    #[cfg(windows)]
    use super::{bind_named_pipe, bind_then_sweep};
    #[cfg(windows)]
    use crate::launch;
    use agentdesktop_core::DEFAULT_SOCKET_PATH;
    use agentdesktop_core::config::{self, parse_daemon};
    use agentdesktop_core::model::{Agent, Discovery, LlmProxyInfo};
    use tokio::sync::{mpsc, watch};

    #[test]
    fn pi_startup_paths_require_user_mode() {
        for user in [false, true] {
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(
                file.path(),
                format!(
                    "daemon:\n  user: {user}\n  pi:\n    models: custom/pi-models.json\n    settings: custom/pi-settings.json\n"
                ),
            )
            .unwrap();
            let config = agentdesktop_core::config::load_daemon(file.path()).unwrap();
            let args = super::DaemonArgs {
                user: false,
                once: false,
                dry_run: false,
                config: None,
                hidden: false,
            };
            let result = args.resolve(
                config.daemon.unwrap(),
                file.path().to_path_buf(),
                agentdesktop_core::DEFAULT_SOCKET_PATH.into(),
            );
            if !user {
                assert!(
                    result
                        .err()
                        .unwrap()
                        .to_string()
                        .contains("daemon.pi paths require --user")
                );
                continue;
            }
            let resolved = result.unwrap();
            assert_eq!(
                resolved.pi.models.as_deref(),
                Some(Path::new("custom/pi-models.json"))
            );
            assert_eq!(
                resolved.pi.settings.as_deref(),
                Some(Path::new("custom/pi-settings.json"))
            );
        }
    }

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
        let (_changes_sender, changes) = mpsc::channel(1);
        super::refresh_inventory_with(sender, Duration::ZERO, changes, None, || async {
            panic!("a zero interval must not schedule a scan")
        })
        .await;
        assert_eq!(receiver.borrow().agents.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn publishes_inventory_only_when_it_changes() {
        let (sender, mut receiver) = watch::channel(Arc::new(discovery(&["codex"])));
        let scans = Arc::new(AtomicUsize::new(0));
        let (_changes_sender, changes) = mpsc::channel(1);
        let refresher = tokio::spawn({
            let scans = Arc::clone(&scans);
            super::refresh_inventory_with(
                sender,
                Duration::from_secs(60),
                changes,
                None,
                move || {
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
                },
            )
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

    /// Real time, and an interval far longer than the test will wait, so only
    /// the change signal can account for the refresh.
    #[tokio::test]
    async fn refreshes_on_a_change_signal_without_waiting_for_the_interval() {
        let (sender, mut receiver) = watch::channel(Arc::new(discovery(&["codex"])));
        let (changes_sender, changes) = mpsc::channel(1);
        let refresher = tokio::spawn(super::refresh_inventory_with(
            sender,
            Duration::from_secs(3600),
            changes,
            None,
            || async { discovery(&["codex", "cursor"]) },
        ));

        changes_sender.try_send(()).expect("refresher is listening");
        tokio::time::timeout(Duration::from_secs(5), receiver.changed())
            .await
            .expect("a change signal must refresh before the interval elapses")
            .expect("refresher is alive");
        assert_eq!(receiver.borrow_and_update().agents.len(), 2);

        refresher.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn stops_refreshing_once_every_reader_is_gone() {
        let (sender, receiver) = watch::channel(Arc::new(discovery(&["codex"])));
        let (_changes_sender, changes) = mpsc::channel(1);
        let refresher = tokio::spawn(super::refresh_inventory_with(
            sender,
            Duration::from_secs(60),
            changes,
            None,
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
            usage: None,
            programs: None,
            violation_ledger: None,
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

    #[tokio::test]
    async fn usage_exported_by_a_local_gateway_is_served_without_a_controller() {
        use agentdesktop_core::model::DeviceUsage;
        use agentdesktop_proto::otlp_logs::{
            AnyValue, ExportLogsServiceRequest, KeyValue, LogRecord, ResourceLogs, ScopeLogs,
            any_value::Value, logs_service_client::LogsServiceClient,
        };

        let root = tempfile::tempdir().unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        let token_file = root.path().join("usage-token");
        std::fs::write(&token_file, format!("{token}\n")).unwrap();
        let usage = config::UsageStartupConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            token_file,
        };
        let (store, receiver) = super::start_usage(&usage, root.path()).await;
        assert!(receiver.bound, "{receiver:?}");
        assert_ne!(receiver.listen, "127.0.0.1:0", "the bound port is reported");

        let attribute = |key: &str, value: Value| KeyValue {
            key: key.to_owned(),
            value: Some(AnyValue { value: Some(value) }),
        };
        let text = |value: &str| Value::StringValue(value.to_owned());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        let request = ExportLogsServiceRequest {
            resource_logs: vec![ResourceLogs {
                scope_logs: vec![ScopeLogs {
                    log_records: vec![LogRecord {
                        time_unix_nano: now,
                        attributes: vec![
                            attribute("gen_ai.provider.name", text("anthropic")),
                            attribute("gen_ai.response.model", text("claude-sonnet-4-5")),
                            attribute("gen_ai.usage.input_tokens", Value::IntValue(1200)),
                            attribute("gen_ai.usage.output_tokens", Value::IntValue(300)),
                            attribute("agw.ai.usage.cost.total", text("0.0081")),
                            attribute("jwt.sub", text("local-user")),
                            attribute(
                                "user_agent.original",
                                text("claude-cli/2.1.260 (external, cli)"),
                            ),
                        ],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }],
        };
        let channel =
            tonic::transport::Endpoint::from_shared(format!("http://{}", receiver.listen))
                .unwrap()
                .connect()
                .await
                .unwrap();
        let mut client = LogsServiceClient::new(channel);
        let refused = client
            .export(request.clone())
            .await
            .expect_err("an export without the token is refused");
        assert_eq!(refused.code(), tonic::Code::Unauthenticated);
        let mut authorized = tonic::Request::new(request);
        authorized
            .metadata_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        client.export(authorized).await.unwrap();

        let config = parse_daemon("{}").unwrap();
        let info = super::DaemonInfo {
            usage_receiver: Some(receiver),
            ..describe_daemon(&config, Path::new("config.yaml"), root.path(), true, None)
        };
        let (_, inventory) = watch::channel(Arc::new(discovery(&[])));
        let app = crate::api::router(crate::api::AppState {
            config,
            daemon_info: info,
            discovery: inventory,
            enrollment: crate::enrollment::EnrollmentState::new(false),
            controller_status: None,
            state_dir: root.path().to_owned(),
            oidc_callback_listen: None,
            telemetry: None,
            logout: None,
            usage: store,
            programs: None,
            violation_ledger: None,
        });
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let connection = tokio::spawn(serve_local_connection(server, app));
        client
            .write_all(
                b"GET /v1/usage?range=24h&utcOffsetMinutes=0 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        let mut response = String::new();
        client.read_to_string(&mut response).await.unwrap();
        connection.await.unwrap().unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        let (_, body) = response.split_once("\r\n\r\n").unwrap();
        let usage: DeviceUsage = serde_json::from_str(body).unwrap();
        assert!(usage.enabled);
        assert_eq!(usage.totals.requests, 1);
        assert_eq!(usage.totals.input_tokens, 1200);
        assert!((usage.totals.cost_usd - 0.0081).abs() < 1e-9);
        assert_eq!(usage.by_tool[0].key, "claude-code");
        assert_eq!(usage.buckets.len(), 24);
    }

    #[tokio::test]
    async fn a_usage_receiver_that_cannot_start_keeps_recorded_usage_available() {
        let root = tempfile::tempdir().unwrap();
        let usage = config::UsageStartupConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            token_file: root.path().join("missing-token"),
        };
        let (store, receiver) = super::start_usage(&usage, root.path()).await;
        assert!(store.is_some(), "the database still opened");
        assert!(!receiver.bound);
        assert!(
            receiver
                .error
                .as_deref()
                .is_some_and(|error| error.contains("read usage token")),
            "{receiver:?}"
        );

        let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        std::fs::write(&usage.token_file, "0123456789abcdef0123456789abcdef").unwrap();
        let usage = config::UsageStartupConfig {
            listen: taken.local_addr().unwrap(),
            ..usage
        };
        let (store, receiver) = super::start_usage(&usage, root.path()).await;
        assert!(store.is_some());
        assert!(!receiver.bound);
        assert!(
            receiver
                .error
                .as_deref()
                .is_some_and(|error| error.starts_with("bind ")),
            "{receiver:?}"
        );
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
            usage: None,
            programs: None,
            violation_ledger: None,
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
    fn skipped_desktop_does_not_request_subscription_sign_in() {
        let mut config = parse_daemon(
            "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: oidc\n    issuer: https://login.example.com\n    clientId: agentdesktop\nprograms:\n  pi:\n    model: selected\n  claudeDesktop:\n    auth: subscription\n"
        ).unwrap();
        assert!(!super::uses_subscription(&config, true));
        assert!(super::uses_subscription(&config, false));

        config.programs.claude_code =
            Some(serde_json::from_value(serde_json::json!({ "auth": "subscription" })).unwrap());
        assert!(super::uses_subscription(&config, true));
        assert!(super::uses_subscription(&config, false));
    }

    #[test]
    fn one_shot_rejects_authenticated_pi_gateway() {
        for authentication in [
            "    type: oidc\n    issuer: https://login.example.com\n    clientId: agentdesktop\n",
            "    type: controllerJwt\n    audience: agentgateway\n    allowedClientIds: [pi]\n",
        ] {
            let config = parse_daemon(&format!(
                "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n{authentication}programs:\n  pi:\n    model: selected\n"
            )).unwrap();
            let error = validate_one_shot(&config, true)
                .expect_err("Pi credentials require a running daemon");
            assert!(error.to_string().contains("credential helpers"));
        }
    }

    #[test]
    fn one_shot_accepts_pi_without_runtime_credentials() {
        for yaml in [
            "programs:\n  pi: {}\n",
            "llmGateway:\n  url: https://gateway.example.com\nprograms:\n  pi:\n    model: selected\n",
            "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: oidc\n    issuer: https://login.example.com\n    clientId: agentdesktop\nprograms:\n  pi:\n    useLlmGateway: false\n",
        ] {
            validate_one_shot(&parse_daemon(yaml).unwrap(), true).unwrap();
        }
    }

    #[test]
    fn one_shot_ignores_runtime_requirements_of_skipped_programs() {
        for (user, programs) in [
            (
                false,
                "  pi:\n    model: selected\n  copilot:\n    models:\n      selected: {}\n  vscode:\n    models:\n      selected: {}\n",
            ),
            (
                true,
                "  claudeDesktop:\n    model: selected\n  grok:\n    model: selected\n",
            ),
        ] {
            let config = parse_daemon(&format!(
                "llmGateway:\n  url: https://gateway.example.com\n  authentication:\n    type: oidc\n    issuer: https://login.example.com\n    clientId: agentdesktop\nprograms:\n  claudeCode:\n    useLlmGateway: false\n{programs}"
            )).unwrap();
            validate_one_shot(&config, user).expect("skipped programs need no runtime services");
            validate_one_shot(&config, !user)
                .expect_err("supported programs still require runtime services");
        }
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
        validate_one_shot(&static_config, true).expect("static settings work in one-shot mode");

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
            validate_one_shot(&oidc, true)
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
            validate_one_shot(&telemetry, true)
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
            validate_one_shot(&copilot, true)
                .unwrap_err()
                .to_string()
                .contains("Copilot")
        );
    }

    #[test]
    fn one_shot_accepts_a_copilot_program_the_user_opted_out_of() {
        let config = parse_daemon(
            r#"
programSelection: { allowUserChoice: true }
llmGateway:
  url: https://gateway.example.com
programs:
  claudeCode: {}
  copilot:
    models:
      gpt-4.1: {}
"#,
        )
        .unwrap();
        let only_claude = agentdesktop_core::model::ProgramChoice::Selected {
            programs: ["claude-code".to_owned()].into(),
        };
        let chosen = crate::reconcile::apply_program_choice(&config, &only_claude).0;
        validate_one_shot(&chosen, true).expect("an opted-out Copilot CLI is only cleaned up");
        assert!(validate_one_shot(&config, true).is_err());
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
            validate_one_shot(&vscode, true)
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
            hidden: false,
        }
    }

    #[test]
    fn the_windowless_reexec_drops_only_hidden() {
        use std::ffi::OsString;

        let argv: Vec<OsString> = ["--hidden", "daemon", "--user", "--socket", r"\\.\pipe\x"]
            .iter()
            .map(OsString::from)
            .collect();
        assert_eq!(
            super::detached_argv(argv),
            ["daemon", "--user", "--socket", r"\\.\pipe\x"]
                .iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        let plain: Vec<OsString> = ["daemon", "--user"].iter().map(OsString::from).collect();
        assert_eq!(super::detached_argv(plain.clone()), plain);
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

    #[test]
    fn socket_flag_gate_decisions() {
        let custom = Path::new(r"\\.\pipe\agentdesktop-user-alice");
        let error = crate::daemon::check_socket_flag(custom, false).unwrap_err();
        assert_eq!(
            error.to_string(),
            "set daemon.socket in the configuration file; --socket is for client commands"
        );
        crate::daemon::check_socket_flag(custom, true).unwrap();
        crate::daemon::check_socket_flag(Path::new(DEFAULT_SOCKET_PATH), false).unwrap();
    }

    #[test]
    fn user_mode_socket_flag_overrides_the_config_socket() {
        // DV6-8: the scheduled-task wrapper passes the per-user pipe name on
        // the command line; it must win over a shared config's daemon.socket.
        let flag = PathBuf::from(r"\\.\pipe\agentdesktop-user-alice");
        let startup = config::DaemonStartupConfig {
            socket: Some(PathBuf::from(r"\\.\pipe\agentdesktop-from-config")),
            ..Default::default()
        };
        let resolved = daemon_args(true)
            .resolve(startup, PathBuf::from("config.yaml"), flag.clone())
            .unwrap();
        assert_eq!(resolved.socket, flag);

        // Without a config value the flag is used as given, not replaced by
        // the user-mode default socket path.
        let resolved = daemon_args(true)
            .resolve(
                config::DaemonStartupConfig::default(),
                PathBuf::from("config.yaml"),
                flag.clone(),
            )
            .unwrap();
        assert_eq!(resolved.socket, flag);
    }

    #[test]
    fn user_mode_without_the_flag_keeps_the_config_socket() {
        let startup = config::DaemonStartupConfig {
            socket: Some(PathBuf::from(r"\\.\pipe\agentdesktop-from-config")),
            ..Default::default()
        };
        let resolved = daemon_args(true)
            .resolve(
                startup,
                PathBuf::from("config.yaml"),
                PathBuf::from(DEFAULT_SOCKET_PATH),
            )
            .unwrap();
        assert_eq!(
            resolved.socket,
            PathBuf::from(r"\\.\pipe\agentdesktop-from-config")
        );
    }

    #[tokio::test]
    async fn the_system_daemon_still_refuses_a_socket_flag() {
        // The refusal precedes config loading, so no config file is needed.
        let error = super::run_until_shutdown(
            daemon_args(false),
            PathBuf::from(r"\\.\pipe\agentdesktop-user-alice"),
            async { anyhow::Ok(()) },
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "set daemon.socket in the configuration file; --socket is for client commands"
        );
    }

    fn startup_with_containment() -> config::DaemonStartupConfig {
        config::DaemonStartupConfig {
            containment: Some(config::ContainmentStartupConfig {
                executor: PathBuf::from(r"C:\mxc\x64\wxc-exec.exe"),
                egress: config::ContainmentEgress::default(),
                engine: config::ContainmentEngine::default(),
                egress_upstream: None,
                git: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn system_mode_rejects_containment() {
        let error = resolve_error(daemon_args(false), startup_with_containment());
        assert!(
            error
                .to_string()
                .contains("daemon.containment requires --user (or daemon.user: true)"),
            "{error:#}"
        );
    }

    #[test]
    fn user_mode_resolves_containment_and_records_a_claude_settings_override() {
        let resolved = daemon_args(true)
            .resolve(
                startup_with_containment(),
                PathBuf::from("config.yaml"),
                PathBuf::from(DEFAULT_SOCKET_PATH),
            )
            .unwrap();
        assert_eq!(
            resolved
                .containment
                .as_ref()
                .map(|cfg| cfg.executor.clone()),
            Some(PathBuf::from(r"C:\mxc\x64\wxc-exec.exe"))
        );
        assert!(!resolved.claude_settings_overridden);

        let mut startup = startup_with_containment();
        startup.claude_code.config = Some(PathBuf::from("/tmp/settings.json"));
        let resolved = daemon_args(true)
            .resolve(
                startup,
                PathBuf::from("config.yaml"),
                PathBuf::from(DEFAULT_SOCKET_PATH),
            )
            .unwrap();
        assert!(resolved.claude_settings_overridden);
    }

    #[test]
    fn one_shot_runs_drop_containment() {
        for (once, dry_run) in [(true, false), (true, true)] {
            let args = DaemonArgs {
                user: true,
                once,
                dry_run,
                config: None,
                hidden: false,
            };
            let resolved = args
                .resolve(
                    startup_with_containment(),
                    PathBuf::from("config.yaml"),
                    PathBuf::from(DEFAULT_SOCKET_PATH),
                )
                .unwrap();
            assert!(
                resolved.containment.is_none(),
                "once={once} dry_run={dry_run}"
            );
        }
    }

    #[test]
    fn containment_egress_deny_is_resolved_from_config() {
        let mut startup = startup_with_containment();
        startup.containment.as_mut().unwrap().egress = config::ContainmentEgress::Deny;
        let resolved = daemon_args(true)
            .resolve(
                startup,
                PathBuf::from("config.yaml"),
                PathBuf::from(DEFAULT_SOCKET_PATH),
            )
            .unwrap();
        assert_eq!(resolved.containment_egress, config::ContainmentEgress::Deny);
    }

    #[test]
    fn containment_egress_defaults_to_deny_when_absent() {
        // PR 4: default flipped from Allow to Deny
        let resolved = daemon_args(true)
            .resolve(
                config::DaemonStartupConfig::default(),
                PathBuf::from("config.yaml"),
                PathBuf::from(DEFAULT_SOCKET_PATH),
            )
            .unwrap();
        assert_eq!(resolved.containment_egress, config::ContainmentEgress::Deny);
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
    fn the_reconcile_tick_is_off_unless_an_interval_is_set() {
        let resolved = daemon_args(true)
            .resolve(
                config::DaemonStartupConfig::default(),
                PathBuf::from("config.yaml"),
                PathBuf::from("agentdesktop.sock"),
            )
            .unwrap();
        assert_eq!(resolved.reconcile_interval, None);
        let startup = config::DaemonStartupConfig {
            reconcile_interval: Some(Duration::from_secs(120)),
            ..Default::default()
        };
        let resolved = daemon_args(true)
            .resolve(
                startup,
                PathBuf::from("config.yaml"),
                PathBuf::from("agentdesktop.sock"),
            )
            .unwrap();
        assert_eq!(resolved.reconcile_interval, Some(Duration::from_secs(120)));
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

    #[tokio::test]
    #[cfg(windows)]
    async fn duplicate_user_daemon_fails_at_pipe_bind_before_mutating_state() {
        use std::sync::atomic::{AtomicBool, Ordering};

        // The user-mode startup path binds the first pipe instance, then runs
        // the sweep closure. A duplicate daemon's bind fails before the sweep,
        // leaving launch directories untouched. This test pins the ordering by
        // pre-binding with the production descriptor, calling bind_then_sweep
        // with a flag-setting closure, and asserting both error and flag=false.
        let pipe_name = format!("\\\\.\\pipe\\agentdesktop-test-{}", std::process::id());
        let pipe_path = std::path::Path::new(&pipe_name);

        let identity = launch::peer_identity(std::process::id()).expect("read test process SID");
        let user_sid = identity.user_sid.expect("test has a user SID");
        let sddl = launch::user_mode_pipe_sddl(&user_sid);

        // Pre-bind with the production user-mode descriptor.
        let _first = bind_named_pipe(pipe_path, true, &sddl).expect("first instance binds");

        // Attempt bind_then_sweep; the sweep should never run.
        let sweep_ran = AtomicBool::new(false);
        let duplicate = bind_then_sweep(pipe_path, &sddl, || {
            sweep_ran.store(true, Ordering::Relaxed);
        });

        assert!(duplicate.is_err(), "second bind must fail");
        assert!(
            !sweep_ran.load(Ordering::Relaxed),
            "sweep must not run when bind fails"
        );
        assert!(
            duplicate
                .unwrap_err()
                .to_string()
                .to_lowercase()
                .contains("pipe"),
            "error mentions pipe"
        );
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn second_pipe_instance_succeeds_with_first_false() {
        // Control for the duplicate test: the first_pipe_instance=true flag is
        // what rejects duplicates, not the SDDL access rights alone. A second
        // bind with first=false succeeds.
        let pipe_name = format!(
            "\\\\.\\pipe\\agentdesktop-test-control-{}",
            std::process::id()
        );
        let pipe_path = std::path::Path::new(&pipe_name);

        let identity = launch::peer_identity(std::process::id()).expect("read test process SID");
        let user_sid = identity.user_sid.expect("test has a user SID");
        let sddl = launch::user_mode_pipe_sddl(&user_sid);

        let _first = bind_named_pipe(pipe_path, true, &sddl).expect("first instance binds");
        let _second = bind_named_pipe(pipe_path, false, &sddl)
            .expect("second instance with first=false succeeds");
    }
}
