use std::{env, fs, io::ErrorKind, path::PathBuf, time::Duration};

use agentdesktop_agent::{
    cli::{self, ClientCommand},
    daemon::{self, DaemonArgs},
};
use agentdesktop_client as client;
use agentdesktop_core::{
    DEFAULT_SOCKET_PATH,
    config::DaemonConfig,
    model::{Discovery, EnrollmentStatus, Health},
};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Manager,
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};

const OPEN_MENU_ID: &str = "open";
const QUIT_MENU_ID: &str = "quit";
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);
const TRAY_READY_ICON: &[u8] = include_bytes!("../../assets/tray-icon@2x.png");
const TRAY_ATTENTION_ICON: &[u8] = include_bytes!("../../assets/tray-icon-attention.png");
const TRAY_OFFLINE_ICON: &[u8] = include_bytes!("../../assets/tray-icon-offline.png");

#[derive(Parser)]
#[command(about = "Agent Desktop UI, daemon, and command-line tools")]
struct Args {
    /// Local endpoint exposed by the daemon (Unix socket or Windows named pipe).
    #[arg(long, default_value = DEFAULT_SOCKET_PATH, global = true)]
    socket: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the privileged device daemon.
    Daemon(DaemonArgs),

    #[command(flatten)]
    Client(ClientCommand),
}

fn tray_icon(state: &str) -> tauri::Result<Image<'static>> {
    let bytes = match state {
        "ready" => TRAY_READY_ICON,
        "attention" => TRAY_ATTENTION_ICON,
        _ => TRAY_OFFLINE_ICON,
    };
    Image::from_bytes(bytes)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "camelCase")]
struct Settings {
    open_on_startup: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            open_on_startup: true,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    settings: Settings,
    version: String,
    platform: &'static str,
    manages_provider_credentials: bool,
    provider_credential_configured: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlatformCapabilities {
    os: &'static str,
    native_gateway: bool,
    transparent_capture: bool,
    trust_installation: bool,
    secret_service: bool,
    protected_file_credentials: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectorRuntime {
    version: &'static str,
    mode: &'static str,
    gateway: &'static str,
    identity: &'static str,
    in_flight: Option<usize>,
    max_in_flight: Option<usize>,
    connect_timeout_ms: Option<u64>,
    shutdown_timeout_ms: Option<u64>,
    platform: PlatformCapabilities,
    metrics: Option<MetricsSnapshot>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MetricsSnapshot {
    requests: u64,
    upstream_responses: u64,
    identity_failures: u64,
    overload_rejections: u64,
    upstream_timeouts: u64,
    upstream_failures: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectorSnapshot {
    state: &'static str,
    detail: Option<String>,
    runtime: Option<ConnectorRuntime>,
}

impl ConnectorSnapshot {
    fn offline(detail: impl Into<String>) -> Self {
        Self {
            state: "offline",
            detail: Some(detail.into()),
            runtime: None,
        }
    }

    fn tray_text(&self) -> &'static str {
        match self.state {
            "ready" => "Status: ready",
            "attention" => "Status: attention required",
            _ => "Status: daemon offline",
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeSnapshot {
    state: &'static str,
    installed: bool,
    can_connect: bool,
    detail: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedDeviceSnapshot {
    configured: bool,
    organization_name: Option<String>,
    support_url: Option<String>,
    admin_url: Option<String>,
    session: &'static str,
    enrollment: &'static str,
    enrollment_id: Option<String>,
    enrollment_created_at: Option<String>,
    device_id: Option<String>,
    public_key_fingerprint: Option<String>,
    certificate: Option<ManagedCertificateSnapshot>,
    detail: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedCertificateSnapshot {
    serial_number: String,
    not_before: String,
    not_after: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ManagedPage {
    Support,
    Administration,
}

fn socket_path() -> PathBuf {
    env::var_os("AGENTDESKTOP_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH))
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let directory = app
        .path()
        .app_config_dir()
        .map_err(|error| format!("cannot resolve settings directory: {error}"))?;
    fs::create_dir_all(&directory)
        .map_err(|error| format!("cannot create settings directory: {error}"))?;
    Ok(directory.join("settings.json"))
}

fn load_settings(app: &AppHandle) -> Result<Settings, String> {
    match fs::read_to_string(settings_path(app)?) {
        Ok(contents) => serde_json::from_str(&contents)
            .map_err(|error| format!("cannot parse settings: {error}")),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(format!("cannot read settings: {error}")),
    }
}

async fn daemon_config() -> Result<DaemonConfig, String> {
    client::get(&socket_path(), "/v1/config")
        .await
        .map_err(|error| format!("{error:#}"))
}

async fn enrollment_status() -> Result<EnrollmentStatus, String> {
    client::get(&socket_path(), "/v1/enrollment")
        .await
        .map_err(|error| format!("{error:#}"))
}

fn enrollment_identity(status: &str, managed: bool) -> &'static str {
    match status {
        "enrolled" => "ready",
        "failed" => "unavailable",
        "notConfigured" if !managed => "not-required",
        "notConfigured" => "not-configured",
        _ => "signed-out",
    }
}

async fn read_connector_status() -> ConnectorSnapshot {
    let endpoint = socket_path();
    if let Err(error) = client::get::<Health>(&endpoint, "/v1/health").await {
        return ConnectorSnapshot::offline(format!(
            "The Agent Desktop daemon is unavailable: {error}"
        ));
    }
    let (config, effective_config) = match tokio::try_join!(
        client::get::<DaemonConfig>(&endpoint, "/v1/config"),
        client::get::<DaemonConfig>(&endpoint, "/v1/effective-config")
    ) {
        Ok(configs) => configs,
        Err(error) => {
            return ConnectorSnapshot::offline(format!("Cannot read daemon state: {error}"));
        }
    };
    let managed = config.controller.is_some();
    let enrollment = client::get::<EnrollmentStatus>(&endpoint, "/v1/enrollment")
        .await
        .ok();
    let identity = enrollment
        .as_ref()
        .map(|value| enrollment_identity(&value.status, managed))
        .unwrap_or(if managed {
            "unavailable"
        } else {
            "not-required"
        });
    let gateway = if effective_config.inference_gateway.is_some() {
        "configured"
    } else {
        "not-configured"
    };
    let state = if !matches!(identity, "unavailable" | "not-configured") {
        "ready"
    } else {
        "attention"
    };
    ConnectorSnapshot {
        state,
        detail: None,
        runtime: Some(ConnectorRuntime {
            version: env!("CARGO_PKG_VERSION"),
            mode: if managed { "managed" } else { "standalone" },
            gateway,
            identity,
            in_flight: None,
            max_in_flight: None,
            connect_timeout_ms: None,
            shutdown_timeout_ms: None,
            platform: PlatformCapabilities {
                os: env::consts::OS,
                native_gateway: false,
                transparent_capture: false,
                trust_installation: false,
                secret_service: false,
                protected_file_credentials: false,
            },
            metrics: None,
        }),
    }
}

async fn claude_snapshot() -> Result<ClaudeSnapshot, String> {
    let endpoint = socket_path();
    let (discovery, config) = tokio::try_join!(
        client::get::<Discovery>(&endpoint, "/v1/discovery"),
        client::get::<DaemonConfig>(&endpoint, "/v1/effective-config")
    )
    .map_err(|error| format!("{error:#}"))?;
    let installed = discovery
        .agents
        .iter()
        .any(|agent| agent.kind == "claude-code");
    Ok(if !installed {
        ClaudeSnapshot {
            state: "not-installed",
            installed: false,
            can_connect: false,
            detail: "Claude Code was not found on this device.",
        }
    } else if config.programs.claude_code.is_some() {
        ClaudeSnapshot {
            state: "connected",
            installed: true,
            can_connect: false,
            detail: "Claude Code is managed by Agent Desktop.",
        }
    } else {
        ClaudeSnapshot {
            state: "discovered",
            installed: true,
            can_connect: false,
            detail: "Claude Code is discovered but is not selected in the active daemon configuration.",
        }
    })
}

fn managed_snapshot(configured: bool, status: Option<&EnrollmentStatus>) -> ManagedDeviceSnapshot {
    let raw = status
        .map(|value| value.status.as_str())
        .unwrap_or("unavailable");
    let (session, enrollment, detail) = match raw {
        "enrolled" => ("ready", "approved", None),
        "awaitingAuthentication" => ("signed-out", "not-enrolled", None),
        "enrolling" => ("ready", "issuing", None),
        "starting" => ("signed-out", "not-enrolled", None),
        "notConfigured" => ("not-configured", "not-configured", None),
        "failed" => (
            "unavailable",
            "unavailable",
            Some("Device enrollment failed.".to_owned()),
        ),
        _ => (
            "unavailable",
            "unavailable",
            Some("Enrollment state is unavailable.".to_owned()),
        ),
    };
    ManagedDeviceSnapshot {
        configured,
        organization_name: configured.then(|| "Managed organization".to_owned()),
        support_url: None,
        admin_url: None,
        session,
        enrollment,
        enrollment_id: None,
        enrollment_created_at: None,
        device_id: None,
        public_key_fingerprint: None,
        certificate: None,
        detail,
    }
}

async fn read_managed_device_status() -> Result<ManagedDeviceSnapshot, String> {
    let config = daemon_config().await?;
    let status = enrollment_status().await.ok();
    Ok(managed_snapshot(
        config.controller.is_some(),
        status.as_ref(),
    ))
}

fn show_main_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[tauri::command]
fn get_bootstrap(app: AppHandle) -> Result<Bootstrap, String> {
    Ok(Bootstrap {
        settings: load_settings(&app)?,
        version: app.package_info().version.to_string(),
        platform: match env::consts::OS {
            "macos" => "macOS",
            "windows" => "Windows",
            "linux" => "Linux",
            other => other,
        },
        manages_provider_credentials: false,
        provider_credential_configured: true,
    })
}

#[tauri::command]
fn save_settings(app: AppHandle, settings: Settings) -> Result<Settings, String> {
    let serialized = serde_json::to_string_pretty(&settings)
        .map_err(|error| format!("cannot serialize settings: {error}"))?;
    fs::write(settings_path(&app)?, format!("{serialized}\n"))
        .map_err(|error| format!("cannot save settings: {error}"))?;
    Ok(settings)
}

#[tauri::command]
async fn get_connector_status() -> ConnectorSnapshot {
    read_connector_status().await
}

#[tauri::command]
async fn get_claude_status() -> Result<ClaudeSnapshot, String> {
    claude_snapshot().await
}

#[tauri::command]
async fn get_managed_device_status() -> Result<ManagedDeviceSnapshot, String> {
    read_managed_device_status().await
}

#[tauri::command]
async fn get_discovery() -> Result<Discovery, String> {
    client::get(&socket_path(), "/v1/discovery")
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn get_remote_config() -> Result<Option<String>, String> {
    client::get(&socket_path(), "/v1/remote-config")
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn logout_managed_device() -> Result<(), String> {
    client::post_json(&socket_path(), "/v1/logout", &())
        .await
        .map_err(|error| format!("{error:#}"))
}

#[tauri::command]
async fn setup_managed_device() -> Result<ManagedDeviceSnapshot, String> {
    let config = daemon_config().await?;
    let status = enrollment_status().await?;
    if let Some(url) = status.authorization_url.as_deref() {
        open::that(url).map_err(|error| format!("could not open enrollment URL: {error}"))?;
    }
    Ok(managed_snapshot(config.controller.is_some(), Some(&status)))
}

#[tauri::command]
fn open_managed_page(_page: ManagedPage) -> Result<(), String> {
    Err("No organization support or administration URL is available from the daemon.".to_owned())
}

#[tauri::command]
async fn connect_claude(api_key: Option<String>) -> Result<ClaudeSnapshot, String> {
    if api_key.is_some() {
        return Err(
            "Provider credentials are configured outside the desktop application.".to_owned(),
        );
    }
    claude_snapshot().await
}

fn run_desktop() -> anyhow::Result<()> {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            show_main_window(app);
        }))
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let open =
                MenuItem::with_id(app, OPEN_MENU_ID, "Open Agent Desktop", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let status = MenuItem::with_id(app, "status", "Checking daemon…", false, None::<&str>)?;
            let quit = MenuItem::with_id(app, QUIT_MENU_ID, "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &separator, &status, &quit])?;
            let tray = TrayIconBuilder::with_id("main")
                .tooltip("Agent Desktop")
                .icon(tray_icon("offline")?)
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    OPEN_MENU_ID => show_main_window(app),
                    QUIT_MENU_ID => app.exit(0),
                    _ => {}
                });
            #[cfg(target_os = "macos")]
            let tray = tray.icon_as_template(true);
            let tray = tray.build(app)?;

            tauri::async_runtime::spawn(async move {
                let mut previous_state = "";
                loop {
                    let snapshot = read_connector_status().await;
                    let _ = status.set_text(snapshot.tray_text());
                    if snapshot.state != previous_state {
                        if let Ok(icon) = tray_icon(snapshot.state) {
                            let _ = tray
                                .set_icon_with_as_template(Some(icon), cfg!(target_os = "macos"));
                        }
                        let _ = tray.set_tooltip(Some(match snapshot.state {
                            "ready" => "Agent Desktop — ready",
                            "attention" => "Agent Desktop — attention required",
                            _ => "Agent Desktop — daemon offline",
                        }));
                        previous_state = snapshot.state;
                    }
                    tokio::time::sleep(STATUS_POLL_INTERVAL).await;
                }
            });

            if load_settings(app.handle())?.open_on_startup {
                show_main_window(app.handle());
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if window.label() == "main"
                && let tauri::WindowEvent::CloseRequested { api, .. } = event
            {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_bootstrap,
            save_settings,
            get_connector_status,
            get_claude_status,
            get_managed_device_status,
            get_discovery,
            get_remote_config,
            logout_managed_device,
            setup_managed_device,
            open_managed_page,
            connect_claude
        ])
        .run(tauri::generate_context!())?;
    Ok(())
}

fn run_command(command: Command, socket: PathBuf) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            match command {
                Command::Daemon(args) => daemon::run(args, socket).await,
                Command::Client(command) => cli::run(command, socket).await,
            }
        })
}

#[cfg(windows)]
fn prepare_desktop_process() {
    // This is a console-subsystem executable so the CLI behaves normally on
    // Windows. Detach that console only when launching the graphical mode.
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn FreeConsole() -> i32;
    }
    unsafe {
        FreeConsole();
    }
}

#[cfg(not(windows))]
fn prepare_desktop_process() {}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    match args.command {
        Some(command) => run_command(command, args.socket),
        None => {
            prepare_desktop_process();
            run_desktop()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Args, Command};
    use clap::Parser;

    #[test]
    fn no_subcommand_launches_desktop_mode() {
        let args = Args::try_parse_from(["agentdesktop"]).unwrap();
        assert!(args.command.is_none());
    }

    #[test]
    fn daemon_and_client_commands_share_the_desktop_executable() {
        let daemon = Args::try_parse_from(["agentdesktop", "daemon"]).unwrap();
        assert!(matches!(daemon.command, Some(Command::Daemon(_))));

        let status = Args::try_parse_from(["agentdesktop", "status"]).unwrap();
        assert!(matches!(status.command, Some(Command::Client(_))));
    }
}
