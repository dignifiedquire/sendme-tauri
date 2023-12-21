// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod upload;

#[tauri::command]
async fn upload(file: String) -> Result<String, String> {
    let path = PathBuf::from(file);
    println!("uploading {}", path.display());

    let (ticket, handle) = upload::provide(path).await.map_err(|e| e.to_string())?;
    // TODO: deal with handle

    Ok(ticket.to_string())
}

use std::path::PathBuf;

use tauri::{CustomMenuItem, SystemTray, SystemTrayEvent, SystemTrayMenu, SystemTrayMenuItem};

fn main() {
    let quit = CustomMenuItem::new("quit".to_string(), "Quit");
    let share = CustomMenuItem::new("share".to_string(), "Share with SendMe");
    let tray_menu = SystemTrayMenu::new()
        .add_item(share)
        .add_native_item(SystemTrayMenuItem::Separator)
        .add_item(quit);

    let system_tray = SystemTray::new().with_menu(tray_menu);

    tauri::Builder::default()
        .system_tray(system_tray)
        .on_system_tray_event(|app, event| match event {
            SystemTrayEvent::LeftClick {
                position: _,
                size: _,
                ..
            } => {
                println!("system tray received a left click");
            }
            SystemTrayEvent::RightClick {
                position: _,
                size: _,
                ..
            } => {
                println!("system tray received a right click");
            }
            SystemTrayEvent::DoubleClick {
                position: _,
                size: _,
                ..
            } => {
                println!("system tray received a double click");
            }
            SystemTrayEvent::MenuItemClick { id, .. } => match id.as_str() {
                "quit" => {
                    std::process::exit(0);
                }
                "share" => {
                    let local_window = tauri::WindowBuilder::new(
                        app,
                        "share",
                        tauri::WindowUrl::App("index.html".into()),
                    )
                    .build()
                    .expect("unable to create window");
                }
                _ => {}
            },
            _ => {}
        })
        .invoke_handler(tauri::generate_handler![upload])
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|_app_handle, event| match event {
            tauri::RunEvent::ExitRequested { api, .. } => {
                api.prevent_exit();
            }
            _ => {}
        })
}
