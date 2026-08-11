use std::{
    env,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use agentplane_client as client;
use agentplane_core::{
    DEFAULT_SOCKET_PATH,
    model::{Discovery, EnrollmentStatus, Health},
};
use tauri::{
    Manager,
    image::Image,
    menu::{MenuBuilder, MenuItem, MenuItemBuilder},
    tray::TrayIconBuilder,
};

const REFRESH_ID: &str = "refresh";
const ENROLL_ID: &str = "enroll";
const QUIT_ID: &str = "quit";

type AuthorizationUrl = Arc<Mutex<Option<String>>>;

#[derive(Clone)]
struct TrayItems {
    daemon: MenuItem<tauri::Wry>,
    enrollment: MenuItem<tauri::Wry>,
    codex: MenuItem<tauri::Wry>,
    opencode: MenuItem<tauri::Wry>,
    claude_code: MenuItem<tauri::Wry>,
    vscode: MenuItem<tauri::Wry>,
}

impl TrayItems {
    fn discoveries(&self) -> [(&str, &str, &MenuItem<tauri::Wry>); 4] {
        [
            ("codex", "Codex", &self.codex),
            ("opencode", "OpenCode", &self.opencode),
            ("claude-code", "Claude Code", &self.claude_code),
            ("vscode", "VS Code", &self.vscode),
        ]
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let daemon = MenuItemBuilder::new("Daemon: connecting…")
                .enabled(false)
                .build(app)?;
            let enrollment = MenuItemBuilder::with_id(ENROLL_ID, "Enrollment: checking…")
                .enabled(false)
                .build(app)?;
            let codex = MenuItemBuilder::new("Codex: checking…")
                .enabled(false)
                .build(app)?;
            let opencode = MenuItemBuilder::new("OpenCode: checking…")
                .enabled(false)
                .build(app)?;
            let claude_code = MenuItemBuilder::new("Claude Code: checking…")
                .enabled(false)
                .build(app)?;
            let vscode = MenuItemBuilder::new("VS Code: checking…")
                .enabled(false)
                .build(app)?;
            let refresh_item = MenuItemBuilder::with_id(REFRESH_ID, "Refresh").build(app)?;
            let quit = MenuItemBuilder::with_id(QUIT_ID, "Quit Agentplane").build(app)?;
            let menu = MenuBuilder::new(app)
                .items(&[
                    &daemon,
                    &enrollment,
                    &codex,
                    &opencode,
                    &claude_code,
                    &vscode,
                ])
                .separator()
                .item(&refresh_item)
                .separator()
                .item(&quit)
                .build()?;

            let items = TrayItems {
                daemon,
                enrollment,
                codex,
                opencode,
                claude_code,
                vscode,
            };
            let authorization_url: AuthorizationUrl = Arc::new(Mutex::new(None));
            let refresh_items = items.clone();
            let menu_authorization_url = authorization_url.clone();
            let tray = TrayIconBuilder::new()
                .icon(tray_icon())
                .icon_as_template(cfg!(target_os = "macos"))
                .tooltip("Agentplane")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    REFRESH_ID => {
                        spawn_refresh(refresh_items.clone(), menu_authorization_url.clone())
                    }
                    ENROLL_ID => {
                        let url = menu_authorization_url
                            .lock()
                            .ok()
                            .and_then(|url| url.clone());
                        if let Some(url) = url
                            && let Err(error) = open::that(url)
                        {
                            eprintln!("failed to open enrollment URL: {error}");
                        }
                    }
                    QUIT_ID => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            app.manage(tray);

            spawn_refresh(items.clone(), authorization_url.clone());
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(5));
                interval.tick().await;
                loop {
                    interval.tick().await;
                    refresh(&items, &authorization_url).await;
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("run Agentplane tray application");
}

fn spawn_refresh(items: TrayItems, authorization_url: AuthorizationUrl) {
    tauri::async_runtime::spawn(async move {
        refresh(&items, &authorization_url).await;
    });
}

async fn refresh(items: &TrayItems, authorization_url: &AuthorizationUrl) {
    let socket = socket_path();
    let health = client::get::<Health>(&socket, "/v1/health").await;
    match health {
        Ok(health) => {
            let _ = items.daemon.set_text(format!("Daemon: {}", health.status));
        }
        Err(_) => {
            let _ = items.daemon.set_text("Daemon: unavailable");
            set_enrollment_status(items, authorization_url, None);
            set_discovery_status(items, None, "daemon unavailable");
            return;
        }
    }

    let enrollment = client::get::<EnrollmentStatus>(&socket, "/v1/enrollment")
        .await
        .ok();
    set_enrollment_status(items, authorization_url, enrollment.as_ref());

    match client::get::<Discovery>(&socket, "/v1/discovery").await {
        Ok(discovery) => {
            set_discovery_status(items, Some(&discovery), "not found");
        }
        Err(_) => {
            set_discovery_status(items, None, "unavailable");
        }
    }
}

fn set_enrollment_status(
    items: &TrayItems,
    authorization_url: &AuthorizationUrl,
    enrollment: Option<&EnrollmentStatus>,
) {
    let url = enrollment.and_then(|status| status.authorization_url.clone());
    if let Ok(mut current) = authorization_url.lock() {
        *current = url.clone();
    }

    let (text, enabled) = match enrollment.map(|status| status.status.as_str()) {
        Some("awaitingAuthentication") => ("Enroll with SSO…", url.is_some()),
        Some("enrolled") => ("Enrollment: complete", false),
        Some("enrolling") => ("Enrollment: completing…", false),
        Some("starting") => ("Enrollment: starting…", false),
        Some("notConfigured") => ("Enrollment: not configured", false),
        Some("failed") => ("Enrollment: failed", false),
        _ => ("Enrollment: unavailable", false),
    };
    let _ = items.enrollment.set_text(text);
    let _ = items.enrollment.set_enabled(enabled);
}

fn set_discovery_status(items: &TrayItems, discovery: Option<&Discovery>, missing: &str) {
    for (kind, label, item) in items.discoveries() {
        let status = discovery
            .and_then(|discovery| discovery.agents.iter().find(|agent| agent.kind == kind))
            .map(|agent| agent.version.as_deref().unwrap_or("discovered"))
            .unwrap_or(missing);
        let _ = item.set_text(format!("{label}: {status}"));
    }
}

fn socket_path() -> PathBuf {
    env::var_os("AGENTPLANE_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH))
}

fn tray_icon() -> Image<'static> {
    Image::from_bytes(include_bytes!("../icons/32x32.png"))
        .expect("decode embedded Agentplane tray icon")
}
