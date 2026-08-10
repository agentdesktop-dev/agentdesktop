use std::{env, path::PathBuf, time::Duration};

use agentplane::{DEFAULT_SOCKET_PATH, client, discovery::Discovery};
use serde::Deserialize;
use tauri::{
    image::Image,
    menu::{MenuBuilder, MenuItem, MenuItemBuilder},
    tray::TrayIconBuilder,
};

const REFRESH_ID: &str = "refresh";
const QUIT_ID: &str = "quit";

#[derive(Deserialize)]
struct Health {
    status: String,
}

#[derive(Clone)]
struct TrayItems {
    daemon: MenuItem<tauri::Wry>,
    codex: MenuItem<tauri::Wry>,
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let daemon = MenuItemBuilder::new("Daemon: connecting…")
                .enabled(false)
                .build(app)?;
            let codex = MenuItemBuilder::new("Codex: checking…")
                .enabled(false)
                .build(app)?;
            let refresh_item = MenuItemBuilder::with_id(REFRESH_ID, "Refresh").build(app)?;
            let quit = MenuItemBuilder::with_id(QUIT_ID, "Quit Agentplane").build(app)?;
            let menu = MenuBuilder::new(app)
                .items(&[&daemon, &codex])
                .separator()
                .item(&refresh_item)
                .separator()
                .item(&quit)
                .build()?;

            let items = TrayItems { daemon, codex };
            let refresh_items = items.clone();
            TrayIconBuilder::new()
                .icon(tray_icon())
                .icon_as_template(cfg!(target_os = "macos"))
                .tooltip("Agentplane")
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    REFRESH_ID => spawn_refresh(refresh_items.clone()),
                    QUIT_ID => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            spawn_refresh(items.clone());
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(5));
                interval.tick().await;
                loop {
                    interval.tick().await;
                    refresh(&items).await;
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("run Agentplane tray application");
}

fn spawn_refresh(items: TrayItems) {
    tauri::async_runtime::spawn(async move {
        refresh(&items).await;
    });
}

async fn refresh(items: &TrayItems) {
    let socket = socket_path();
    let health = client::get::<Health>(&socket, "/v1/health").await;
    match health {
        Ok(health) => {
            let _ = items.daemon.set_text(format!("Daemon: {}", health.status));
        }
        Err(_) => {
            let _ = items.daemon.set_text("Daemon: unavailable");
            let _ = items.codex.set_text("Codex: unknown");
            return;
        }
    }

    match client::get::<Discovery>(&socket, "/v1/discovery").await {
        Ok(discovery) => {
            let text = discovery
                .agents
                .iter()
                .find(|agent| agent.kind == "codex")
                .map(|agent| {
                    agent
                        .version
                        .as_deref()
                        .map(|version| format!("Codex: {version}"))
                        .unwrap_or_else(|| "Codex: discovered".to_owned())
                })
                .unwrap_or_else(|| "Codex: not found".to_owned());
            let _ = items.codex.set_text(text);
        }
        Err(_) => {
            let _ = items.codex.set_text("Codex: unavailable");
        }
    }
}

fn socket_path() -> PathBuf {
    env::var_os("AGENTPLANE_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH))
}

fn tray_icon() -> Image<'static> {
    const SIZE: u32 = 32;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as i32 - 16;
            let dy = y as i32 - 16;
            let distance = dx * dx + dy * dy;
            let alpha = if (64..=196).contains(&distance) || distance <= 12 {
                255
            } else {
                0
            };
            rgba.extend_from_slice(&[59, 130, 246, alpha]);
        }
    }
    Image::new_owned(rgba, SIZE, SIZE)
}
