//! Desktop commands retained from the Electron host.
use std::sync::Mutex;
use tauri::{
    Manager,
    menu::{Menu, MenuItem, MenuItemKind, PredefinedMenuItem},
};

// User zoom level, in powers of Chromium's 1.2 scale factor; not display DPI.
#[derive(Default)]
pub struct Zoom(Mutex<i32>);

pub fn install(app: &tauri::App) -> tauri::Result<()> {
    let menu = Menu::default(app.handle())?;
    for item in menu.items()? {
        let MenuItemKind::Submenu(submenu) = item else {
            continue;
        };
        if submenu.text()? == app.package_info().name {
            submenu.set_text("LAYR Studio")?;
        }
        if submenu.text()? != "View" {
            continue;
        }
        submenu.prepend_items(&[
            &MenuItem::with_id(app, "reload-workspace", "Reload", true, Some("CmdOrCtrl+R"))?,
            &PredefinedMenuItem::separator(app)?,
            &MenuItem::with_id(app, "reset-zoom", "Actual Size", true, Some("CmdOrCtrl+0"))?,
            &MenuItem::with_id(app, "zoom-in", "Zoom In", true, Some("CmdOrCtrl+Plus"))?,
            &MenuItem::with_id(app, "zoom-out", "Zoom Out", true, Some("CmdOrCtrl+-"))?,
            &PredefinedMenuItem::separator(app)?,
        ])?;
    }
    app.set_menu(menu)?;
    app.manage(Zoom::default());
    app.on_menu_event(|app, event| {
        let result: Result<(), String> = (|| {
            let id = event.id().as_ref();
            if id == "reload-workspace" {
                return crate::close::reload(app);
            }
            if !matches!(id, "reset-zoom" | "zoom-in" | "zoom-out") {
                return Ok(());
            }
            let window = app
                .get_webview_window("main")
                .ok_or("The Studio window is unavailable.")?;
            let state = app.state::<Zoom>();
            let mut level = state.0.lock().map_err(|_| "The zoom service stopped.")?;
            let next = match id {
                "reset-zoom" => 0,
                "zoom-in" => *level + 1,
                _ => *level - 1,
            }
            .clamp(-7, 9);
            window
                .set_zoom(1.2_f64.powi(next))
                .map_err(|e| e.to_string())?;
            *level = next;
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("Desktop command failed: {error}");
        }
    });
    Ok(())
}
