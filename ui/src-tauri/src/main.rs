#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(any(target_os = "macos", target_os = "windows"))]
mod discovery;

use std::{
    env, fs,
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};

use agentdesktop::apps::claude;
use agentdesktop::identity::{
    enrollment::{
        EnrollmentClient, EnrollmentRecord, EnrollmentStatus, load_enrollment_for,
        save_enrollment_for,
    },
    oauth::{LoginConfig, ManagedIdentity, load_session_for, login, open_authorization_url},
    storage::{CredentialStorageMode, CredentialStore, default_storage_root},
};
use agentdesktop::organization::OrganizationBootstrap;
use agentdesktop_ui::provider_credentials;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{
    AppHandle, Manager,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};

const OPEN_MENU_ID: &str = "open";
const QUIT_MENU_ID: &str = "quit";
const CONNECTOR_STATUS_URL: &str = "http://127.0.0.1:8081/_agentdesktop/status";
const STATUS_POLL_INTERVAL: Duration = Duration::from_secs(5);
const MANAGED_ROUTING_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);
const CONNECTOR_BASE_URL: &str = "http://127.0.0.1:8080";
const PLACEHOLDER_CREDENTIAL: &str = "local-gateway-placeholder";
const IDENTITY_DIR_ENV: &str = "AGENTDESKTOP_IDENTITY_DIR";
const CREDENTIAL_STORAGE_ENV: &str = "AGENTDESKTOP_CREDENTIAL_STORAGE";
const ORGANIZATION_CONFIG_ENV: &str = "AGENTDESKTOP_ORGANIZATION_CONFIG";
const DESIRED_CLAUDE_ENVIRONMENT: [(&str, &str); 2] = [
    ("ANTHROPIC_BASE_URL", CONNECTOR_BASE_URL),
    ("ANTHROPIC_API_KEY", PLACEHOLDER_CREDENTIAL),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ClaudeConnectionState {
    NotInstalled,
    NotConnected,
    Connected,
    Conflict,
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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
struct PlatformCapabilities {
    os: String,
    native_gateway: bool,
    transparent_capture: bool,
    trust_installation: bool,
    secret_service: bool,
    protected_file_credentials: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
struct MetricsSnapshot {
    requests: u64,
    upstream_responses: u64,
    identity_failures: u64,
    overload_rejections: u64,
    upstream_timeouts: u64,
    upstream_failures: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all(serialize = "camelCase", deserialize = "snake_case"))]
struct ConnectorRuntime {
    version: String,
    mode: String,
    gateway: String,
    identity: String,
    #[serde(default)]
    in_flight: Option<usize>,
    #[serde(default)]
    max_in_flight: Option<usize>,
    #[serde(default)]
    connect_timeout_ms: Option<u64>,
    #[serde(default)]
    shutdown_timeout_ms: Option<u64>,
    platform: PlatformCapabilities,
    #[serde(default)]
    metrics: Option<MetricsSnapshot>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConnectorSnapshot {
    state: &'static str,
    detail: Option<String>,
    runtime: Option<ConnectorRuntime>,
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
struct Bootstrap {
    settings: Settings,
    version: String,
    platform: &'static str,
    manages_provider_credentials: bool,
    provider_credential_configured: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ManagedCertificateSnapshot {
    serial_number: String,
    not_before: String,
    not_after: String,
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

#[derive(Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ManagedPage {
    Support,
    Administration,
}

impl ManagedDeviceSnapshot {
    fn not_configured() -> Self {
        Self {
            configured: false,
            organization_name: None,
            support_url: None,
            admin_url: None,
            session: "not-configured",
            enrollment: "not-configured",
            enrollment_id: None,
            enrollment_created_at: None,
            device_id: None,
            public_key_fingerprint: None,
            certificate: None,
            detail: None,
        }
    }

    fn unavailable(detail: impl Into<String>) -> Self {
        Self {
            configured: true,
            detail: Some(detail.into()),
            ..Self::not_configured()
        }
    }

    fn from_bootstrap(bootstrap: &OrganizationBootstrap) -> Self {
        Self {
            configured: true,
            organization_name: Some(bootstrap.organization.display_name.clone()),
            support_url: Some(bootstrap.organization.support_url.to_string()),
            admin_url: bootstrap
                .identity
                .enrollment_url
                .join("admin/")
                .ok()
                .map(|url| url.to_string()),
            session: "signed-out",
            enrollment: "not-enrolled",
            enrollment_id: None,
            enrollment_created_at: None,
            device_id: None,
            public_key_fingerprint: None,
            certificate: None,
            detail: None,
        }
    }
}

fn apply_enrollment_snapshot(snapshot: &mut ManagedDeviceSnapshot, enrollment: EnrollmentRecord) {
    snapshot.enrollment = match enrollment.status {
        EnrollmentStatus::Pending => "pending",
        EnrollmentStatus::Issuing => "issuing",
        EnrollmentStatus::Approved => "approved",
        EnrollmentStatus::Rejected => "rejected",
    };
    snapshot.enrollment_id = Some(enrollment.enrollment_id);
    snapshot.enrollment_created_at = Some(enrollment.created_at);
    snapshot.device_id = enrollment.device_id;
    snapshot.public_key_fingerprint = Some(enrollment.public_key_fingerprint);
    snapshot.certificate = enrollment
        .certificate
        .map(|certificate| ManagedCertificateSnapshot {
            serial_number: certificate.serial_number,
            not_before: certificate.not_before,
            not_after: certificate.not_after,
        });
}

fn resolve_organization_config(
    explicit: Option<PathBuf>,
    resource_directory: Option<&Path>,
    executable: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(explicit) = explicit.filter(|path| !path.as_os_str().is_empty()) {
        return Some(explicit);
    }
    let mut candidates = Vec::new();
    if let Some(resource_directory) = resource_directory {
        candidates.push(resource_directory.join("share/organization.json"));
        candidates.push(resource_directory.join("organization.json"));
    }
    if let Some(install_root) = executable.and_then(Path::parent).and_then(Path::parent) {
        candidates.push(install_root.join("share/organization.json"));
    }
    candidates.into_iter().find(|path| path.is_file())
}

fn organization_config_path(app: &AppHandle) -> Option<PathBuf> {
    let explicit = env::var_os(ORGANIZATION_CONFIG_ENV).map(PathBuf::from);
    let resource_directory = app.path().resource_dir().ok();
    let executable = env::current_exe().ok();
    resolve_organization_config(
        explicit,
        resource_directory.as_deref(),
        executable.as_deref(),
    )
}

fn load_organization_bootstrap(app: &AppHandle) -> Result<OrganizationBootstrap, String> {
    let path = organization_config_path(app)
        .ok_or_else(|| "this installation is not configured for an organization".to_owned())?;
    let encoded = fs::read(&path)
        .map_err(|_| "the organization configuration could not be read".to_owned())?;
    OrganizationBootstrap::parse(&encoded)
        .map_err(|_| "the organization configuration is invalid".to_owned())
}

fn managed_device_snapshot(app: &AppHandle) -> ManagedDeviceSnapshot {
    let Some(path) = organization_config_path(app) else {
        return ManagedDeviceSnapshot::not_configured();
    };
    let bootstrap = match fs::read(path)
        .map_err(anyhow::Error::from)
        .and_then(|encoded| OrganizationBootstrap::parse(&encoded))
    {
        Ok(bootstrap) => bootstrap,
        Err(_) => {
            return ManagedDeviceSnapshot::unavailable(
                "The organization configuration could not be loaded.",
            );
        }
    };
    let mut snapshot = ManagedDeviceSnapshot::from_bootstrap(&bootstrap);
    let storage_root = match env::var_os(IDENTITY_DIR_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map_or_else(default_storage_root, Ok)
    {
        Ok(storage_root) => storage_root,
        Err(_) => {
            snapshot.session = "unavailable";
            snapshot.enrollment = "unavailable";
            snapshot.detail = Some("The managed credential location is unavailable.".to_owned());
            return snapshot;
        }
    };
    if !storage_root.exists() {
        return snapshot;
    }
    match identity_storage_is_empty(&storage_root) {
        Ok(true) => return snapshot,
        Ok(false) => {}
        Err(_) => {
            snapshot.session = "unavailable";
            snapshot.enrollment = "unavailable";
            snapshot.detail = Some("The managed credential location is unavailable.".to_owned());
            return snapshot;
        }
    }
    let store = match CredentialStore::load(&storage_root) {
        Ok(store) => store,
        Err(_) => {
            snapshot.session = "unavailable";
            snapshot.enrollment = "unavailable";
            snapshot.detail = Some("Managed credentials could not be opened.".to_owned());
            return snapshot;
        }
    };
    snapshot.session =
        match load_session_for(&bootstrap.identity.issuer, &bootstrap.gateway.url, &store) {
            Ok(session) => match session.is_expired() {
                Ok(true) => "refresh-required",
                Ok(false) => "ready",
                Err(_) => "unavailable",
            },
            Err(_) => "signed-out",
        };
    let enrollment =
        match load_enrollment_for(&bootstrap.identity.issuer, &bootstrap.gateway.url, &store) {
            Ok(enrollment) => enrollment,
            Err(_) => return snapshot,
        };
    apply_enrollment_snapshot(&mut snapshot, enrollment);
    snapshot
}

fn identity_storage_is_empty(storage_root: &Path) -> std::io::Result<bool> {
    if storage_root.join("credential-storage").exists() {
        return Ok(false);
    }
    Ok(fs::read_dir(storage_root)?.next().is_none())
}

fn managed_credential_store() -> Result<CredentialStore, String> {
    let storage_root = env::var_os(IDENTITY_DIR_ENV)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .map_or_else(default_storage_root, Ok)
        .map_err(|error| error.to_string())?;
    if storage_root.join("credential-storage").is_file() {
        CredentialStore::load(&storage_root).map_err(|error| error.to_string())
    } else {
        let mode = match env::var(CREDENTIAL_STORAGE_ENV).as_deref() {
            Ok("auto") | Err(env::VarError::NotPresent) => CredentialStorageMode::Auto,
            Ok("file") => CredentialStorageMode::File,
            Ok("secret-service") => CredentialStorageMode::SecretService,
            Ok(value) => {
                return Err(format!(
                    "{CREDENTIAL_STORAGE_ENV} must be auto, file, or secret-service, got {value:?}"
                ));
            }
            Err(env::VarError::NotUnicode(_)) => {
                return Err(format!("{CREDENTIAL_STORAGE_ENV} is not valid text"));
            }
        };
        CredentialStore::setup(mode, &storage_root).map_err(|error| error.to_string())
    }
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
    let path = settings_path(app)?;
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Settings::default()),
        Err(error) => return Err(format!("cannot read settings: {error}")),
    };
    serde_json::from_str(&contents).map_err(|error| format!("cannot parse settings: {error}"))
}

impl ConnectorSnapshot {
    fn from_runtime(runtime: ConnectorRuntime) -> Self {
        let needs_identity = matches!(runtime.identity.as_str(), "unavailable" | "not-configured");
        let state = if runtime.gateway == "reachable" && !needs_identity {
            "ready"
        } else {
            "attention"
        };
        Self {
            state,
            detail: None,
            runtime: Some(runtime),
        }
    }

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
            _ => "Status: connector offline",
        }
    }
}

fn connector_client() -> Result<Client, reqwest::Error> {
    Client::builder().timeout(Duration::from_secs(2)).build()
}

async fn read_connector_status(client: &Client) -> ConnectorSnapshot {
    let response = match client.get(CONNECTOR_STATUS_URL).send().await {
        Ok(response) => response,
        Err(_) => {
            return ConnectorSnapshot::offline(
                "The connector is not responding on its loopback endpoint.",
            );
        }
    };
    if !response.status().is_success() {
        return ConnectorSnapshot::offline(format!(
            "The connector status endpoint returned HTTP {}.",
            response.status().as_u16()
        ));
    }
    match response.json::<ConnectorRuntime>().await {
        Ok(runtime) => ConnectorSnapshot::from_runtime(runtime),
        Err(_) => ConnectorSnapshot::offline("The connector returned an invalid status response."),
    }
}

fn claude_snapshot() -> Result<ClaudeSnapshot, String> {
    let status = claude_connection_status()?;
    Ok(match status {
        ClaudeConnectionState::NotInstalled => ClaudeSnapshot {
            state: "not-installed",
            installed: false,
            can_connect: false,
            detail: "Claude Code was not found on this device.",
        },
        ClaudeConnectionState::NotConnected => ClaudeSnapshot {
            state: "not-connected",
            installed: true,
            can_connect: true,
            detail: "Claude Code can be routed through the local connector.",
        },
        ClaudeConnectionState::Connected => ClaudeSnapshot {
            state: "connected",
            installed: true,
            can_connect: false,
            detail: "Claude Code is configured to use Agent Desktop.",
        },
        ClaudeConnectionState::Conflict => ClaudeSnapshot {
            state: "conflict",
            installed: true,
            can_connect: false,
            detail: "Claude Code already has a different provider or gateway configuration.",
        },
    })
}

fn claude_connection_status() -> Result<ClaudeConnectionState, String> {
    if !claude::is_installed().map_err(|error| error.to_string())? {
        return Ok(ClaudeConnectionState::NotInstalled);
    }
    let home = env::var_os("HOME").ok_or_else(|| "HOME is not set".to_owned())?;
    claude_connection_status_for(&PathBuf::from(home).join(".claude/settings.json"))
}

fn claude_connection_status_for(path: &Path) -> Result<ClaudeConnectionState, String> {
    if !path.exists() {
        return Ok(ClaudeConnectionState::NotConnected);
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "Claude Code settings {} is not a regular file",
            path.display()
        ));
    }
    let settings =
        serde_json::from_slice::<Value>(&fs::read(path).map_err(|error| error.to_string())?)
            .map_err(|_| format!("Claude Code settings {} is not valid JSON", path.display()))?;
    let root = settings.as_object().ok_or_else(|| {
        format!(
            "Claude Code settings {} must contain a JSON object",
            path.display()
        )
    })?;
    let Some(environment) = root.get("env") else {
        return Ok(ClaudeConnectionState::NotConnected);
    };
    let environment = environment.as_object().ok_or_else(|| {
        format!(
            "Claude Code settings {} has a non-object env setting",
            path.display()
        )
    })?;
    if DESIRED_CLAUDE_ENVIRONMENT
        .iter()
        .all(|(name, value)| environment.get(*name).and_then(Value::as_str) == Some(*value))
    {
        return Ok(ClaudeConnectionState::Connected);
    }
    if DESIRED_CLAUDE_ENVIRONMENT.iter().any(|(name, value)| {
        environment
            .get(*name)
            .is_some_and(|existing| existing.as_str() != Some(*value))
    }) {
        return Ok(ClaudeConnectionState::Conflict);
    }
    Ok(ClaudeConnectionState::NotConnected)
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
    let managed = organization_config_path(&app).is_some();
    let owns_local_gateway = env::var_os("AGENTDESKTOP_GATEWAY_BINARY").is_some()
        && env::var_os("AGENTDESKTOP_GATEWAY_CONFIG").is_some();
    let local_provider_credential_configured = if managed {
        false
    } else {
        provider_credentials::is_configured().map_err(|error| error.to_string())?
    };
    let (manages_provider_credentials, provider_credential_configured) = provider_access_state(
        managed,
        owns_local_gateway,
        local_provider_credential_configured,
    );
    Ok(Bootstrap {
        settings: load_settings(&app)?,
        version: app.package_info().version.to_string(),
        platform: match std::env::consts::OS {
            "macos" => "macOS",
            "windows" => "Windows",
            "linux" => "Linux",
            _ => std::env::consts::OS,
        },
        manages_provider_credentials,
        provider_credential_configured,
    })
}

fn provider_access_state(
    managed: bool,
    owns_local_gateway: bool,
    local_credential_configured: bool,
) -> (bool, bool) {
    if managed {
        (false, true)
    } else {
        (owns_local_gateway, local_credential_configured)
    }
}

fn validate_provider_credential_input(managed: bool, has_api_key: bool) -> Result<(), String> {
    if managed && has_api_key {
        return Err("provider credentials are managed by the organization".into());
    }
    Ok(())
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
async fn get_connector_status() -> Result<ConnectorSnapshot, String> {
    let client = connector_client().map_err(|error| error.to_string())?;
    Ok(read_connector_status(&client).await)
}

#[tauri::command]
fn get_claude_status() -> Result<ClaudeSnapshot, String> {
    claude_snapshot()
}

#[tauri::command]
fn get_managed_device_status(app: AppHandle) -> ManagedDeviceSnapshot {
    managed_device_snapshot(&app)
}

#[tauri::command]
async fn setup_managed_device(app: AppHandle) -> Result<ManagedDeviceSnapshot, String> {
    let bootstrap = load_organization_bootstrap(&app)?;
    let store = managed_credential_store()?;
    let gateway_origin = bootstrap.gateway.url.clone();
    let session = match load_session_for(&bootstrap.identity.issuer, &gateway_origin, &store) {
        Ok(session) => session,
        Err(_) => login(
            &LoginConfig {
                issuer: bootstrap.identity.issuer.clone(),
                client_id: bootstrap.identity.client_id.clone(),
                audience: bootstrap.identity.audience.clone(),
                scope: bootstrap.identity.scope.clone(),
                gateway_origin: gateway_origin.clone(),
            },
            &store,
            open_authorization_url,
        )
        .await
        .map_err(|error| error.to_string())?,
    };
    let identity = ManagedIdentity::new(session, store.clone());
    let client = EnrollmentClient::new(&bootstrap.identity.enrollment_url)
        .map_err(|error| error.to_string())?;
    let enrollment = match load_enrollment_for(&bootstrap.identity.issuer, &gateway_origin, &store)
    {
        Ok(enrollment) if enrollment.status == EnrollmentStatus::Approved => enrollment,
        Ok(enrollment) => client
            .status(&identity, &enrollment)
            .await
            .map_err(|error| error.to_string())?,
        Err(_) => client
            .request(&identity)
            .await
            .map_err(|error| error.to_string())?,
    };
    save_enrollment_for(
        &bootstrap.identity.issuer,
        &gateway_origin,
        &store,
        &enrollment,
    )
    .map_err(|error| error.to_string())?;
    if enrollment.status == EnrollmentStatus::Approved {
        let _ = claude::connect_installed();
    }

    let mut snapshot = ManagedDeviceSnapshot::from_bootstrap(&bootstrap);
    snapshot.session = "ready";
    apply_enrollment_snapshot(&mut snapshot, enrollment);
    Ok(snapshot)
}

async fn run_managed_routing_reconciler(bootstrap: OrganizationBootstrap, identity_root: PathBuf) {
    loop {
        if identity_root.join("credential-storage").is_file()
            && let Ok(store) = CredentialStore::load(&identity_root)
            && load_enrollment_for(&bootstrap.identity.issuer, &bootstrap.gateway.url, &store)
                .is_ok_and(|enrollment| enrollment.status == EnrollmentStatus::Approved)
        {
            let _ = claude::connect_installed();
        }
        tokio::time::sleep(MANAGED_ROUTING_RECONCILE_INTERVAL).await;
    }
}

#[tauri::command]
fn open_managed_page(app: AppHandle, page: ManagedPage) -> Result<(), String> {
    let bootstrap = load_organization_bootstrap(&app)?;
    let url = match page {
        ManagedPage::Support => bootstrap.organization.support_url,
        ManagedPage::Administration => bootstrap
            .identity
            .enrollment_url
            .join("admin/")
            .map_err(|_| "the administration URL is invalid".to_owned())?,
    };
    open::that(url.as_str()).map_err(|error| format!("could not open the system browser: {error}"))
}

#[tauri::command]
fn connect_claude(app: AppHandle, api_key: Option<String>) -> Result<ClaudeSnapshot, String> {
    if claude_connection_status()? == ClaudeConnectionState::Conflict {
        return Err("Claude Code already has a different provider or gateway configuration".into());
    }
    validate_provider_credential_input(
        organization_config_path(&app).is_some(),
        api_key.is_some(),
    )?;
    if let Some(api_key) = api_key {
        provider_credentials::store(api_key).map_err(|error| error.to_string())?;
    }
    claude::connect_installed().map_err(|error| error.to_string())?;
    claude_snapshot()
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(
            |app, _arguments, _cwd| {
                show_main_window(app);
            },
        ))
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let open =
                MenuItem::with_id(app, OPEN_MENU_ID, "Open Agent Desktop", true, None::<&str>)?;
            let separator = PredefinedMenuItem::separator(app)?;
            let status = MenuItem::with_id(app, "status", "Desktop ready", false, None::<&str>)?;
            let quit = MenuItem::with_id(app, QUIT_MENU_ID, "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open, &separator, &status, &quit])?;

            let mut tray = TrayIconBuilder::with_id("main")
                .tooltip("Agent Desktop")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    OPEN_MENU_ID => show_main_window(app),
                    QUIT_MENU_ID => app.exit(0),
                    _ => {}
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
                #[cfg(target_os = "macos")]
                {
                    tray = tray.icon_as_template(true);
                }
            }
            tray.build(app)?;

            let status_item = status.clone();
            let status_client = connector_client()?;
            tauri::async_runtime::spawn(async move {
                loop {
                    let snapshot = read_connector_status(&status_client).await;
                    let _ = status_item.set_text(snapshot.tray_text());
                    tokio::time::sleep(STATUS_POLL_INTERVAL).await;
                }
            });

            if let Ok(bootstrap) = load_organization_bootstrap(app.handle()) {
                let identity_root = env::var_os(IDENTITY_DIR_ENV)
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from)
                    .map_or_else(default_storage_root, Ok)?;
                tauri::async_runtime::spawn(run_managed_routing_reconciler(
                    bootstrap.clone(),
                    identity_root.clone(),
                ));
                #[cfg(any(target_os = "macos", target_os = "windows"))]
                tauri::async_runtime::spawn(discovery::run_reporter(bootstrap, identity_root));
            }

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
            setup_managed_device,
            open_managed_page,
            connect_claude
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Agent Desktop");
}

#[cfg(test)]
mod tests {
    use super::{
        ClaudeConnectionState, ConnectorRuntime, ConnectorSnapshot, ManagedDeviceSnapshot,
        MetricsSnapshot, OrganizationBootstrap, PlatformCapabilities, Settings,
        claude_connection_status_for, provider_access_state, resolve_organization_config,
        validate_provider_credential_input,
    };

    #[test]
    fn migrates_legacy_desktop_settings() {
        let settings: Settings =
            serde_json::from_str(r#"{"gatewayUrl":"http://127.0.0.1:4000","openOnStartup":false}"#)
                .unwrap();
        assert!(!settings.open_on_startup);
    }

    #[test]
    fn classifies_connector_attention_states() {
        let runtime = |gateway: &str, identity: &str| ConnectorRuntime {
            version: "0.1.0".to_owned(),
            mode: "standalone".to_owned(),
            gateway: gateway.to_owned(),
            identity: identity.to_owned(),
            in_flight: None,
            max_in_flight: None,
            connect_timeout_ms: None,
            shutdown_timeout_ms: None,
            platform: PlatformCapabilities {
                os: "macos".to_owned(),
                native_gateway: true,
                transparent_capture: false,
                trust_installation: false,
                secret_service: false,
                protected_file_credentials: false,
            },
            metrics: Some(MetricsSnapshot {
                requests: 0,
                upstream_responses: 0,
                identity_failures: 0,
                overload_rejections: 0,
                upstream_timeouts: 0,
                upstream_failures: 0,
            }),
        };

        assert_eq!(
            ConnectorSnapshot::from_runtime(runtime("reachable", "not-required")).state,
            "ready"
        );
        assert_eq!(
            ConnectorSnapshot::from_runtime(runtime("unreachable", "not-required")).state,
            "attention"
        );
        assert_eq!(
            ConnectorSnapshot::from_runtime(runtime("reachable", "unavailable")).state,
            "attention"
        );
        assert_eq!(
            ConnectorSnapshot::from_runtime(runtime("reachable", "refresh-required")).state,
            "ready"
        );
    }

    #[test]
    fn accepts_the_current_connector_status_shape() {
        let runtime: ConnectorRuntime = serde_json::from_str(
            r#"{
                "version":"0.1.0",
                "mode":"managed",
                "gateway":"reachable",
                "identity":"ready",
                "platform":{
                    "os":"macos",
                    "native_gateway":true,
                    "transparent_capture":false,
                    "trust_installation":false,
                    "secret_service":false,
                    "protected_file_credentials":false
                }
            }"#,
        )
        .unwrap();

        assert!(runtime.metrics.is_none());
        assert_eq!(ConnectorSnapshot::from_runtime(runtime).state, "ready");
    }

    #[test]
    fn projects_only_public_managed_configuration() {
        let bootstrap = OrganizationBootstrap::parse(include_bytes!(
            "../../../examples/managed-walkthrough/organization.json"
        ))
        .unwrap();
        let snapshot =
            serde_json::to_value(ManagedDeviceSnapshot::from_bootstrap(&bootstrap)).unwrap();

        assert_eq!(snapshot["organizationName"], "Walkthrough Organization");
        assert!(snapshot.get("clientId").is_none());
    }

    #[test]
    fn managed_ui_never_manages_provider_credentials() {
        assert_eq!(provider_access_state(true, true, true), (false, true));
        assert_eq!(provider_access_state(true, false, false), (false, true));
        assert!(validate_provider_credential_input(true, true).is_err());
        assert!(validate_provider_credential_input(true, false).is_ok());
        assert_eq!(provider_access_state(false, true, true), (true, true));
    }

    #[test]
    fn resolves_explicit_and_installed_organization_configuration() {
        let temporary = tempfile::tempdir().unwrap();
        let resource_directory = temporary.path().join("resources");
        let installed_executable = temporary.path().join("bundle/bin/agentdesktop-ui");
        let installed_config = temporary.path().join("bundle/share/organization.json");
        std::fs::create_dir_all(installed_config.parent().unwrap()).unwrap();
        std::fs::write(&installed_config, b"{}").unwrap();

        assert_eq!(
            resolve_organization_config(
                None,
                Some(&resource_directory),
                Some(&installed_executable),
            ),
            Some(installed_config)
        );

        let explicit = temporary.path().join("development-organization.json");
        assert_eq!(
            resolve_organization_config(
                Some(explicit.clone()),
                Some(&resource_directory),
                Some(&installed_executable),
            ),
            Some(explicit)
        );
    }

    #[test]
    fn distinguishes_first_run_from_partial_managed_credential_state() {
        let temporary = tempfile::tempdir().unwrap();
        assert!(super::identity_storage_is_empty(temporary.path()).unwrap());

        std::fs::write(temporary.path().join("orphaned-credential"), b"partial").unwrap();
        assert!(!super::identity_storage_is_empty(temporary.path()).unwrap());
    }

    #[test]
    fn inspects_claude_settings_without_modifying_them() {
        let temporary = tempfile::tempdir().unwrap();
        let settings = temporary.path().join("settings.json");
        assert_eq!(
            claude_connection_status_for(&settings).unwrap(),
            ClaudeConnectionState::NotConnected
        );

        std::fs::write(
            &settings,
            br#"{"env":{"ANTHROPIC_BASE_URL":"http://127.0.0.1:8080","ANTHROPIC_API_KEY":"local-gateway-placeholder"}}"#,
        )
        .unwrap();
        assert_eq!(
            claude_connection_status_for(&settings).unwrap(),
            ClaudeConnectionState::Connected
        );

        std::fs::write(
            &settings,
            br#"{"env":{"ANTHROPIC_BASE_URL":"https://existing.example"}}"#,
        )
        .unwrap();
        assert_eq!(
            claude_connection_status_for(&settings).unwrap(),
            ClaudeConnectionState::Conflict
        );
    }
}
