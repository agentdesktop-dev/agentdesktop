use std::{env, fs, future::Future, io::ErrorKind, path::PathBuf, time::Duration};

#[cfg(target_os = "macos")]
use std::{ffi::OsString, io::Write, os::unix::fs::OpenOptionsExt, path::Path, process::Stdio};

#[cfg(target_os = "macos")]
use agentdesktop_agent::secure_fs;
use agentdesktop_agent::{
    access,
    cli::{self, ClientCommand},
    daemon::{self, DaemonArgs},
    discovery,
};
use agentdesktop_client as client;
use agentdesktop_core::{
    DEFAULT_SOCKET_PATH, VERSION,
    config::DaemonConfig,
    model::{AccessReport, AccessSourceKind, Discovery, EnrollmentStatus, Health},
};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tauri::{
    AppHandle, Manager,
    image::Image,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};

mod network_policy;

const OPEN_MENU_ID: &str = "open";
const QUIT_MENU_ID: &str = "quit";
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);
const ENROLLMENT_URL_POLL_INTERVAL: Duration = Duration::from_millis(100);
const ENROLLMENT_URL_WAIT_TIMEOUT: Duration = Duration::from_secs(15);
static LOCAL_ACCESS_SCAN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
#[cfg(target_os = "macos")]
const SYSTEM_LAUNCH_DAEMON_PATH: &str = "/Library/LaunchDaemons/dev.agentdesktop.daemon.plist";
#[cfg(target_os = "macos")]
const USER_LAUNCH_AGENT_LABEL: &str = "dev.agentdesktop.daemon.user";
const TRAY_READY_ICON: &[u8] = include_bytes!("../../../frontend/desktop/assets/tray-icon@2x.png");
const TRAY_ATTENTION_ICON: &[u8] =
    include_bytes!("../../../frontend/desktop/assets/tray-icon-attention.png");
const TRAY_OFFLINE_ICON: &[u8] =
    include_bytes!("../../../frontend/desktop/assets/tray-icon-offline.png");

#[derive(Parser)]
#[command(about = "Agent Desktop UI, daemon, and command-line tools")]
struct Args {
    /// Override the local endpoint exposed by the daemon (Unix socket or Windows named pipe).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the device daemon.
    Daemon(Box<DaemonArgs>),

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
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PlatformCapabilities {
    os: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectorRuntime {
    version: &'static str,
    mode: &'static str,
    gateway: &'static str,
    platform: PlatformCapabilities,
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
struct ManagedDeviceSnapshot {
    configured: bool,
    organization_name: Option<String>,
    enrollment: &'static str,
    detail: Option<String>,
}

fn socket_path() -> PathBuf {
    let system = PathBuf::from(DEFAULT_SOCKET_PATH);
    #[cfg(target_os = "macos")]
    let system_daemon_installed = Path::new(SYSTEM_LAUNCH_DAEMON_PATH).exists();
    #[cfg(not(target_os = "macos"))]
    let system_daemon_installed = system.exists();
    preferred_socket_path(
        env::var_os("AGENTDESKTOP_SOCKET").map(PathBuf::from),
        system,
        system_daemon_installed,
        user_socket_path(),
    )
}

fn preferred_socket_path(
    explicit: Option<PathBuf>,
    system: PathBuf,
    system_daemon_installed: bool,
    user: Option<PathBuf>,
) -> PathBuf {
    explicit.unwrap_or_else(|| {
        if system_daemon_installed {
            system
        } else {
            user.unwrap_or(system)
        }
    })
}

#[cfg(unix)]
fn user_socket_path() -> Option<PathBuf> {
    if let Some(runtime) = env::var_os("XDG_RUNTIME_DIR") {
        return Some(PathBuf::from(runtime).join("agentdesktop.sock"));
    }
    let home = env::var_os("HOME").map(PathBuf::from)?;
    let state = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    Some(state.join("agentdesktop/agentdesktop.sock"))
}

#[cfg(windows)]
fn user_socket_path() -> Option<PathBuf> {
    None
}

#[cfg(target_os = "macos")]
struct UserDaemonPaths {
    config: PathBuf,
    state_directory: PathBuf,
    socket: PathBuf,
    launch_agent: PathBuf,
    log: PathBuf,
}

#[cfg(target_os = "macos")]
#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct UserLaunchAgent {
    label: &'static str,
    program_arguments: Vec<String>,
    run_at_load: bool,
    keep_alive: bool,
    process_type: &'static str,
    standard_out_path: String,
    standard_error_path: String,
}

#[cfg(target_os = "macos")]
fn user_daemon_paths() -> anyhow::Result<UserDaemonPaths> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("cannot start user daemon without HOME"))?;
    let config_home = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let state_home = env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state"));
    let state_directory = state_home.join("agentdesktop");
    let socket = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| state_directory.clone())
        .join("agentdesktop.sock");

    Ok(UserDaemonPaths {
        config: config_home.join("agentdesktop/config.yaml"),
        state_directory,
        socket,
        launch_agent: home
            .join("Library/LaunchAgents")
            .join(format!("{USER_LAUNCH_AGENT_LABEL}.plist")),
        log: home.join("Library/Logs/Agentdesktop/daemon.log"),
    })
}

#[cfg(target_os = "macos")]
fn path_string(path: &Path) -> anyhow::Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8: {}", path.display()))
}

#[cfg(target_os = "macos")]
fn user_launch_agent(
    paths: &UserDaemonPaths,
    executable: &Path,
) -> anyhow::Result<UserLaunchAgent> {
    let log = path_string(&paths.log)?;
    Ok(UserLaunchAgent {
        label: USER_LAUNCH_AGENT_LABEL,
        program_arguments: vec![
            path_string(executable)?,
            "--socket".to_owned(),
            path_string(&paths.socket)?,
            "daemon".to_owned(),
            "--user".to_owned(),
            "--config".to_owned(),
            path_string(&paths.config)?,
            "--state-dir".to_owned(),
            path_string(&paths.state_directory)?,
        ],
        run_at_load: true,
        keep_alive: true,
        process_type: "Background",
        standard_out_path: log.clone(),
        standard_error_path: log,
    })
}

#[cfg(target_os = "macos")]
fn ensure_user_daemon_files(paths: &UserDaemonPaths) -> anyhow::Result<()> {
    let config_directory = paths
        .config
        .parent()
        .ok_or_else(|| anyhow::anyhow!("user daemon config has no parent directory"))?;
    secure_fs::ensure_private_dir(config_directory)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    match options.open(&paths.config) {
        Ok(mut file) => {
            file.write_all(b"{}\n")?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }

    let launch_agent_directory = paths
        .launch_agent
        .parent()
        .ok_or_else(|| anyhow::anyhow!("user LaunchAgent has no parent directory"))?;
    fs::create_dir_all(launch_agent_directory)?;
    let log_directory = paths
        .log
        .parent()
        .ok_or_else(|| anyhow::anyhow!("user daemon log has no parent directory"))?;
    secure_fs::ensure_private_dir(log_directory)?;

    let executable = env::current_exe()?;
    let launch_agent = user_launch_agent(paths, &executable)?;
    let mut contents = Vec::new();
    plist::to_writer_xml(&mut contents, &launch_agent)?;
    secure_fs::atomic_write(&paths.launch_agent, &contents, 0o600)
}

#[cfg(target_os = "macos")]
async fn run_launchctl(arguments: Vec<OsString>) -> anyhow::Result<()> {
    let output = tokio::process::Command::new("/bin/launchctl")
        .args(&arguments)
        .stdin(Stdio::null())
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "launchctl {} failed: {}",
            arguments
                .first()
                .map(|argument| argument.to_string_lossy())
                .unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn remove_user_launch_agent(paths: &UserDaemonPaths) {
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}/{USER_LAUNCH_AGENT_LABEL}");
    let _ = tokio::process::Command::new("/bin/launchctl")
        .args(["bootout", &target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    if let Err(error) = fs::remove_file(&paths.launch_agent)
        && error.kind() != ErrorKind::NotFound
    {
        eprintln!("could not remove user LaunchAgent: {error}");
    }
}

#[cfg(target_os = "macos")]
async fn ensure_desktop_daemon() -> anyhow::Result<()> {
    if env::var_os("AGENTDESKTOP_SOCKET").is_some() {
        return Ok(());
    }

    let paths = user_daemon_paths()?;
    if Path::new(SYSTEM_LAUNCH_DAEMON_PATH).exists() {
        remove_user_launch_agent(&paths).await;
        return Ok(());
    }
    if client::get::<Health>(&socket_path(), "/v1/health")
        .await
        .is_ok()
    {
        return Ok(());
    }

    ensure_user_daemon_files(&paths)?;
    let uid = unsafe { libc::getuid() };
    let domain = format!("gui/{uid}");
    let target = format!("{domain}/{USER_LAUNCH_AGENT_LABEL}");
    let _ = tokio::process::Command::new("/bin/launchctl")
        .args(["bootout", &target])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    run_launchctl(vec!["enable".into(), target.clone().into()]).await?;
    run_launchctl(vec![
        "bootstrap".into(),
        domain.into(),
        paths.launch_agent.clone().into_os_string(),
    ])
    .await?;
    run_launchctl(vec!["kickstart".into(), target.into()]).await?;

    for _ in 0..25 {
        if client::get::<Health>(&paths.socket, "/v1/health")
            .await
            .is_ok()
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("user daemon did not become ready")
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
    let organization_access_ready =
        !managed || enrollment.is_some_and(|value| value.status == "enrolled");
    let gateway = if effective_config.llm_gateway.is_some() {
        "configured"
    } else {
        "not-configured"
    };
    let state = if organization_access_ready {
        "ready"
    } else {
        "attention"
    };
    ConnectorSnapshot {
        state,
        detail: None,
        runtime: Some(ConnectorRuntime {
            version: VERSION,
            mode: if managed { "managed" } else { "standalone" },
            gateway,
            platform: PlatformCapabilities {
                os: env::consts::OS,
            },
        }),
    }
}

fn managed_snapshot(configured: bool, status: Option<&EnrollmentStatus>) -> ManagedDeviceSnapshot {
    let raw = status
        .map(|value| value.status.as_str())
        .unwrap_or("unavailable");
    let (enrollment, detail) = match raw {
        "enrolled" => ("approved", None),
        "awaitingAuthentication" => ("not-enrolled", None),
        "enrolling" => ("issuing", None),
        "starting" => ("not-enrolled", None),
        "notConfigured" => ("not-configured", None),
        "failed" => ("unavailable", Some("Device enrollment failed.".to_owned())),
        _ => (
            "unavailable",
            Some("Enrollment state is unavailable.".to_owned()),
        ),
    };
    ManagedDeviceSnapshot {
        configured,
        organization_name: configured.then(|| "Managed organization".to_owned()),
        enrollment,
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
async fn get_access_report() -> Result<AccessReport, String> {
    local_access_report()
        .await
        .map_err(|error| format!("{error:#}"))
}

async fn local_access_report() -> anyhow::Result<AccessReport> {
    let _scan = LOCAL_ACCESS_SCAN.lock().await;
    let user_home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("cannot assess local access without a user home"))?;
    let discovery = discovery::discover().await;
    tokio::task::spawn_blocking(move || access::assess(&discovery, &user_home))
        .await
        .map_err(|error| anyhow::anyhow!("local access assessment failed: {error}"))
}

#[tauri::command]
async fn apply_network_rule_change(
    request: network_policy::NetworkRuleChangeRequest,
) -> Result<AccessReport, String> {
    let config = daemon_config().await?;
    let _scan = LOCAL_ACCESS_SCAN.lock().await;
    let user_home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "Cannot change local network rules without a user home".to_owned())?;
    let discovery = discovery::discover().await;
    let initial_discovery = discovery.clone();
    let initial_home = user_home.clone();
    let report =
        tokio::task::spawn_blocking(move || access::assess(&initial_discovery, &initial_home))
            .await
            .map_err(|error| format!("Local access assessment failed: {error}"))?;
    network_policy::apply(&config, &report, &user_home, &request)
        .map_err(|error| format!("{error:#}"))?;
    tokio::task::spawn_blocking(move || access::assess(&discovery, &user_home))
        .await
        .map_err(|error| format!("Local access assessment failed: {error}"))
}

fn report_allows_access_source(report: &AccessReport, path: &PathBuf) -> bool {
    let Ok(path) = fs::canonicalize(path) else {
        return false;
    };
    report.agents.iter().any(|agent| {
        let capability_source = agent
            .capabilities
            .iter()
            .filter(|capability| capability.source.kind == AccessSourceKind::Configuration)
            .filter_map(|capability| capability.source.path.as_ref())
            .filter_map(|source| fs::canonicalize(source).ok())
            .any(|source| source == path);
        let finding_source = agent
            .findings
            .iter()
            .filter_map(|finding| finding.source.as_ref())
            .filter(|source| source.kind == AccessSourceKind::Configuration)
            .filter_map(|source| source.path.as_ref())
            .filter_map(|source| fs::canonicalize(source).ok())
            .any(|source| source == path);
        capability_source || finding_source
    })
}

async fn open_access_source_with<Load, LoadFuture, Open>(
    path: PathBuf,
    load_report: Load,
    open_path: Open,
) -> Result<(), String>
where
    Load: FnOnce() -> LoadFuture,
    LoadFuture: Future<Output = Result<AccessReport, String>>,
    Open: FnOnce(&PathBuf) -> Result<(), String>,
{
    if !path.is_absolute() {
        return Err("Access source must be an existing absolute regular file".to_owned());
    }
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| "Access source must be an existing absolute regular file".to_owned())?;
    if !metadata.file_type().is_file() {
        return Err("Access source must be an existing absolute regular file".to_owned());
    }
    let canonical = fs::canonicalize(&path)
        .map_err(|_| "Access source must be an existing absolute regular file".to_owned())?;
    if !canonical.is_file() {
        return Err("Access source must be an existing absolute regular file".to_owned());
    }
    let report = load_report().await?;
    if !report_allows_access_source(&report, &canonical) {
        return Err("File is not a source in the current access report".to_owned());
    }
    open_path(&canonical)
}

#[tauri::command]
async fn open_access_source(path: PathBuf) -> Result<(), String> {
    open_access_source_with(
        path,
        || async {
            local_access_report()
                .await
                .map_err(|error| format!("{error:#}"))
        },
        |path| {
            open::that_detached(path)
                .map_err(|error| format!("could not open access source: {error}"))
        },
    )
    .await
}

#[cfg(test)]
mod access_source_tests {
    use std::{future::ready, path::PathBuf};

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    use agentdesktop_core::model::{
        AccessCapability, AccessCategory, AccessDecision, AccessEnforcement, AccessFinding,
        AccessOperation, AccessReport, AccessReportStatus, AccessSeverity, AccessSource,
        AccessSourceKind, AgentAccessReport,
    };

    use super::{open_access_source_with, report_allows_access_source};

    fn report_with_source(kind: AccessSourceKind, path: PathBuf) -> AccessReport {
        AccessReport {
            generated_at_unix_ms: 0,
            status: AccessReportStatus::Ready,
            detail: None,
            agents: vec![AgentAccessReport {
                kind: "vscode".to_owned(),
                executable: PathBuf::from("/Applications/Visual Studio Code.app"),
                version: None,
                user_home: PathBuf::from("/Users/developer"),
                capabilities: vec![AccessCapability {
                    category: AccessCategory::Network,
                    resource: "example.com".to_owned(),
                    operations: vec![AccessOperation::Connect],
                    decision: AccessDecision::Allow,
                    enforcement: AccessEnforcement::Harness,
                    workspace: None,
                    source: AccessSource {
                        kind,
                        path: Some(path),
                    },
                    rule: None,
                    detail: None,
                }],
                observations: Vec::new(),
                findings: Vec::new(),
                coverage: Vec::new(),
            }],
        }
    }

    #[test]
    fn opens_only_explicit_configuration_sources() {
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-access-source-kinds-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, "{}\n").unwrap();

        assert!(report_allows_access_source(
            &report_with_source(AccessSourceKind::Configuration, path.clone()),
            &path
        ));
        assert!(!report_allows_access_source(
            &report_with_source(AccessSourceKind::Mcp, path.clone()),
            &path
        ));
        assert!(!report_allows_access_source(
            &report_with_source(AccessSourceKind::History, path.clone()),
            &path
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn opens_configuration_sources_attached_only_to_findings() {
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-access-finding-source-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, "{}\n").unwrap();
        let mut report = report_with_source(AccessSourceKind::History, path.clone());
        report.agents[0].capabilities.clear();
        report.agents[0].findings.push(AccessFinding {
            severity: AccessSeverity::Critical,
            title: "Unsafe setting".to_owned(),
            detail: "Review this setting".to_owned(),
            category: AccessCategory::Execution,
            workspace: None,
            source: Some(AccessSource {
                kind: AccessSourceKind::Configuration,
                path: Some(path.clone()),
            }),
        });

        assert!(report_allows_access_source(&report, &path));
        report.agents[0].findings[0].source = Some(AccessSource {
            kind: AccessSourceKind::History,
            path: Some(path.clone()),
        });
        assert!(!report_allows_access_source(&report, &path));

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn opens_a_configured_existing_source() {
        let path = std::env::temp_dir().join(format!(
            "agentdesktop-access-source-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, "{}\n").unwrap();
        let canonical = std::fs::canonicalize(&path).unwrap();
        let report = report_with_source(AccessSourceKind::Configuration, path.clone());
        let mut opened = None;

        open_access_source_with(
            path.clone(),
            || ready(Ok(report)),
            |source| {
                opened = Some(source.clone());
                Ok(())
            },
        )
        .await
        .unwrap();

        let _ = std::fs::remove_file(&path);
        assert_eq!(opened.as_ref(), Some(&canonical));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_a_symlinked_access_source() {
        let directory = std::env::temp_dir().join(format!(
            "agentdesktop-access-source-link-{}",
            std::process::id()
        ));
        let target = directory.join("settings.json");
        let link = directory.join("current-settings.json");
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(&target, "{}\n").unwrap();
        symlink(&target, &link).unwrap();
        let report = report_with_source(AccessSourceKind::Configuration, link.clone());
        let mut opened = None;

        let error = open_access_source_with(
            link,
            || ready(Ok(report)),
            |source| {
                opened = Some(source.clone());
                Ok(())
            },
        )
        .await
        .unwrap_err();

        let _ = std::fs::remove_dir_all(&directory);
        assert!(error.contains("regular file"));
        assert!(opened.is_none());
    }
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

async fn open_enrollment_url<GetStatus, StatusFuture, OpenUrl>(
    mut get_status: GetStatus,
    mut open_url: OpenUrl,
) -> Result<EnrollmentStatus, String>
where
    GetStatus: FnMut() -> StatusFuture,
    StatusFuture: Future<Output = Result<EnrollmentStatus, String>>,
    OpenUrl: FnMut(&str) -> Result<(), String>,
{
    tokio::time::timeout(ENROLLMENT_URL_WAIT_TIMEOUT, async {
        loop {
            let status = get_status().await?;
            if let Some(url) = status.authorization_url.as_deref() {
                open_url(url)?;
                return Ok(status);
            }
            if status.status != "starting" {
                return Ok(status);
            }
            tokio::time::sleep(ENROLLMENT_URL_POLL_INTERVAL).await;
        }
    })
    .await
    .map_err(|_| "timed out waiting for the enrollment sign-in URL".to_owned())?
}

#[tauri::command]
async fn setup_managed_device() -> Result<ManagedDeviceSnapshot, String> {
    let config = daemon_config().await?;
    let status = open_enrollment_url(enrollment_status, |url| {
        open::that(url).map_err(|error| format!("could not open enrollment URL: {error}"))
    })
    .await?;
    Ok(managed_snapshot(config.controller.is_some(), Some(&status)))
}

fn run_desktop() -> anyhow::Result<()> {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            show_main_window(app);
        }))
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            #[cfg(target_os = "macos")]
            tauri::async_runtime::spawn(async {
                if let Err(error) = ensure_desktop_daemon().await {
                    eprintln!("could not start Agent Desktop daemon: {error:#}");
                }
            });

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
            get_managed_device_status,
            get_discovery,
            get_access_report,
            apply_network_rule_change,
            open_access_source,
            get_remote_config,
            logout_managed_device,
            setup_managed_device
        ])
        .run(tauri::generate_context!())?;
    Ok(())
}

fn run_command(command: Command, socket: Option<PathBuf>) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            match command {
                Command::Daemon(args) => {
                    let socket = socket.unwrap_or_else(|| DEFAULT_SOCKET_PATH.into());
                    daemon::run(*args, socket).await
                }
                Command::Client(command) => {
                    let socket = socket.unwrap_or_else(socket_path);
                    cli::run(command, socket).await
                }
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
mod enrollment_tests {
    use std::{collections::VecDeque, future::ready};

    use agentdesktop_core::model::EnrollmentStatus;

    use super::open_enrollment_url;

    #[tokio::test]
    async fn waits_for_authorization_url_before_opening_browser() {
        let mut statuses = VecDeque::from([
            EnrollmentStatus {
                status: "starting".to_owned(),
                authorization_url: None,
            },
            EnrollmentStatus {
                status: "awaitingAuthentication".to_owned(),
                authorization_url: Some("https://login.example/authorize".to_owned()),
            },
        ]);
        let mut opened = Vec::new();

        let status = open_enrollment_url(
            || ready(Ok(statuses.pop_front().expect("next enrollment status"))),
            |url| {
                opened.push(url.to_owned());
                Ok(())
            },
        )
        .await
        .unwrap();

        assert_eq!(status.status, "awaitingAuthentication");
        assert_eq!(opened, ["https://login.example/authorize"]);
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::path::Path;

    use super::{
        USER_LAUNCH_AGENT_LABEL, UserDaemonPaths, preferred_socket_path, user_launch_agent,
    };

    #[test]
    fn stale_system_socket_does_not_override_user_socket() {
        assert_eq!(
            preferred_socket_path(
                None,
                "/var/run/agentdesktop/agentdesktop.sock".into(),
                false,
                Some("/Users/test/.local/state/agentdesktop/agentdesktop.sock".into()),
            ),
            Path::new("/Users/test/.local/state/agentdesktop/agentdesktop.sock")
        );
    }

    #[test]
    fn user_launch_agent_runs_the_bundled_daemon_in_user_mode() {
        let paths = UserDaemonPaths {
            config: "/Users/test/.config/agentdesktop/config.yaml".into(),
            state_directory: "/Users/test/.local/state/agentdesktop".into(),
            socket: "/Users/test/.local/state/agentdesktop/agentdesktop.sock".into(),
            launch_agent: "/Users/test/Library/LaunchAgents/dev.agentdesktop.daemon.user.plist"
                .into(),
            log: "/Users/test/Library/Logs/Agentdesktop/daemon.log".into(),
        };
        let launch_agent = user_launch_agent(
            &paths,
            Path::new("/Applications/agentdesktop.app/Contents/MacOS/agentdesktop"),
        )
        .unwrap();

        assert_eq!(launch_agent.label, USER_LAUNCH_AGENT_LABEL);
        assert!(launch_agent.run_at_load);
        assert!(launch_agent.keep_alive);
        assert_eq!(
            launch_agent.program_arguments,
            [
                "/Applications/agentdesktop.app/Contents/MacOS/agentdesktop",
                "--socket",
                "/Users/test/.local/state/agentdesktop/agentdesktop.sock",
                "daemon",
                "--user",
                "--config",
                "/Users/test/.config/agentdesktop/config.yaml",
                "--state-dir",
                "/Users/test/.local/state/agentdesktop",
            ]
        );
    }
}
