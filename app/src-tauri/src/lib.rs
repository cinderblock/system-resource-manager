//! The tray app: one more client of `srmd`. It subscribes, redraws the tray and hands
//! each snapshot to the web view. Quitting it changes nothing about the broker.

mod icon;

use icon::Face;
use srm_proto::client::Client;
use srm_proto::wire::{Request, Response};
use srm_proto::{Fleet, HistoryRow};
use std::sync::Mutex;
use std::time::Duration;
use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, State, WindowEvent};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_positioner::{Position, WindowExt};

const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Default)]
struct Latest(Mutex<Option<Fleet>>);

#[tauri::command]
fn current(latest: State<'_, Latest>) -> Option<Fleet> {
    latest.0.lock().expect("latest lock").clone()
}

#[tauri::command]
async fn history() -> Result<Vec<HistoryRow>, String> {
    let mut client = Client::connect_or_spawn()
        .await
        .map_err(|e| format!("{e:#}"))?;
    match client.call(&Request::History { limit: Some(100) }).await {
        Ok(Response::History { rows }) => Ok(rows),
        Ok(Response::Error { message }) => Err(message),
        Ok(other) => Err(format!("unexpected reply: {other:?}")),
        Err(e) => Err(format!("{e:#}")),
    }
}

fn redraw(tray: &TrayIcon, face: Face, cpu_percent: f32, previous: &mut Option<Face>) {
    let _ = tray.set_tooltip(Some(face.tooltip(cpu_percent)));
    if *previous != Some(face) {
        *previous = Some(face);
        let pixels = icon::draw(face);
        let _ = tray.set_icon(Some(Image::new_owned(pixels, icon::SIZE, icon::SIZE)));
    }
}

/// Stay subscribed for the life of the app, across daemon restarts.
async fn watch(app: AppHandle, tray: TrayIcon) {
    let mut face = None;
    loop {
        if let Ok(mut client) = Client::connect_or_spawn().await {
            if client.send(&Request::Subscribe).await.is_ok() {
                while let Ok(Some(response)) = client.recv().await {
                    let Response::Snapshot { fleet } = response else {
                        continue;
                    };
                    redraw(
                        &tray,
                        Face::of(&fleet),
                        fleet.local.system.cpu_percent,
                        &mut face,
                    );
                    let _ = app.emit("fleet", &fleet);
                    *app.state::<Latest>().0.lock().expect("latest lock") = Some(*fleet);
                }
            }
        }
        let _ = app.emit("disconnected", ());
        let _ = tray.set_tooltip(Some("Lost contact with srmd"));
        tokio::time::sleep(RECONNECT_INTERVAL).await;
    }
}

fn toggle(app: &AppHandle) {
    let Some(window) = app.get_webview_window("main") else {
        return;
    };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
    } else {
        let _ = window.move_window(Position::TrayCenter);
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<TrayIcon> {
    let autostart = app.autolaunch().is_enabled().unwrap_or(false);
    let login = CheckMenuItem::with_id(app, "login", "Start at login", true, autostart, None::<&str>)?;
    let menu = Menu::with_items(
        app,
        &[
            &MenuItem::with_id(app, "open", "Open", true, None::<&str>)?,
            &login,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?,
        ],
    )?;
    let pixels = icon::draw(Face::default());

    TrayIconBuilder::with_id("main")
        .icon(Image::new_owned(pixels, icon::SIZE, icon::SIZE))
        .tooltip("Connecting to srmd…")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id().as_ref() {
            "open" => toggle(app),
            "login" => {
                let manager = app.autolaunch();
                let enabled = manager.is_enabled().unwrap_or(false);
                let changed = if enabled {
                    manager.disable()
                } else {
                    manager.enable()
                };
                let actual = if changed.is_ok() { !enabled } else { enabled };
                let _ = login.set_checked(actual);
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            let app = tray.app_handle();
            tauri_plugin_positioner::on_tray_event(app, &event);
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle(app);
            }
        })
        .build(app)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            toggle(app);
        }))
        .plugin(tauri_plugin_positioner::init())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .manage(Latest::default())
        .invoke_handler(tauri::generate_handler![current, history])
        .on_window_event(|window, event| match event {
            WindowEvent::CloseRequested { api, .. } => {
                api.prevent_close();
                let _ = window.hide();
            }
            WindowEvent::Focused(false) => {
                let _ = window.hide();
            }
            _ => {}
        })
        .setup(|app| {
            let handle = app.handle().clone();
            let tray = build_tray(&handle)?;
            tauri::async_runtime::spawn(watch(handle, tray));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running the tray app");
}
