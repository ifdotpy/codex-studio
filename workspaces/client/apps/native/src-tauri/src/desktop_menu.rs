//! Desktop commands retained from the Electron host.
use std::sync::Arc;
use std::sync::Mutex;
use tauri::{
    Manager, WebviewWindow,
    menu::{Menu, MenuItem, MenuItemKind, PredefinedMenuItem},
};

// User zoom level, in powers of Chromium's 1.2 scale factor; not display DPI.
#[derive(Default)]
pub struct Zoom(Mutex<i32>);

/// False requires a native clipboard result with no files. True consumes file
/// paste, including a held result. The owner reports any later uncertain result.
pub type FilePasteHandler = Arc<dyn Fn(&tauri::AppHandle) -> Result<bool, String> + Send + Sync>;

struct FrozenPopup {
    id: String,
    lease: String,
    capture: FilePasteHandler,
}
#[derive(Default)]
struct PopupRoutes {
    pending: Option<FrozenPopup>,
    menu: Option<Menu<tauri::Wry>>,
    menu_intercepted: bool,
}
impl PopupRoutes {
    fn arm(&mut self, id: String, lease: String, capture: FilePasteHandler) {
        self.menu = None;
        self.pending = Some(FrozenPopup { id, lease, capture });
    }
    fn take(&mut self, id: &str) -> Option<FilePasteHandler> {
        if self.pending.as_ref()?.id != id {
            return None;
        }
        self.menu = None;
        self.pending.take().map(|pending| pending.capture)
    }
    fn revoke(&mut self, lease: &str) -> bool {
        if self
            .pending
            .as_ref()
            .is_none_or(|pending| pending.lease != lease)
        {
            return false;
        }
        self.pending = None;
        self.menu = None;
        true
    }
}
#[derive(Default)]
struct ContextMenus(Mutex<PopupRoutes>);

/// This proves the main menu hook only. Context-menu and drop hooks need their
/// own caller-bound installation proofs before attachment activation.
pub fn menu_paste_hook_installed(app: &tauri::AppHandle) -> bool {
    let Some(state) = app.try_state::<ContextMenus>() else {
        return false;
    };
    state.0.lock().is_ok_and(|state| state.menu_intercepted)
}

/// The root supplies the actual caller window, private permit, acknowledged
/// target lease, and frozen capture callback. No renderer path or event name enters.
pub fn popup_attachment_edit(
    window: &WebviewWindow,
    expected_window: isize,
    permit: u64,
    lease: String,
    capture: FilePasteHandler,
) -> Result<String, String> {
    crate::trusted(window)?;
    if lease.is_empty() {
        return Err("The attachment target has no acknowledged native lease.".into());
    }
    studio_client_core::desktop::consume_attachment_context_menu_permission(
        permit,
        expected_window,
    )?;
    let id = format!(
        "{}{}",
        studio_client_core::attachment_selections::contract()["popupPasteActionPrefix"]
            .as_str()
            .expect("attachment popup action prefix"),
        studio_client_core::server_keys::random_id()?
    );
    let label = PredefinedMenuItem::paste(window, None)
        .map_err(|error| error.to_string())?
        .text()
        .map_err(|error| error.to_string())?;
    let menu = Menu::with_items(
        window,
        &[
            &PredefinedMenuItem::undo(window, None).map_err(|error| error.to_string())?,
            &PredefinedMenuItem::redo(window, None).map_err(|error| error.to_string())?,
            &PredefinedMenuItem::separator(window).map_err(|error| error.to_string())?,
            &PredefinedMenuItem::cut(window, None).map_err(|error| error.to_string())?,
            &PredefinedMenuItem::copy(window, None).map_err(|error| error.to_string())?,
            &MenuItem::with_id(window, id.as_str(), label, true, Some("CmdOrCtrl+V"))
                .map_err(|error| error.to_string())?,
            &PredefinedMenuItem::select_all(window, None).map_err(|error| error.to_string())?,
        ],
    )
    .map_err(|error| error.to_string())?;
    let routes = window
        .app_handle()
        .try_state::<ContextMenus>()
        .ok_or("The native attachment menu owner is unavailable.")?;
    {
        let mut routes = routes
            .0
            .lock()
            .map_err(|_| "The native attachment menu owner stopped.")?;
        routes.arm(id.clone(), lease.clone(), capture);
        routes.menu = Some(menu.clone());
    }
    if let Err(error) = window.popup_menu(&menu) {
        if let Ok(mut routes) = routes.0.lock() {
            routes.revoke(&lease);
        }
        return Err(error.to_string());
    }
    Ok(id)
}

/// A delayed close of an older target cannot release a newer popup owner.
pub fn revoke_attachment_popup(app: &tauri::AppHandle, lease: &str) -> Result<bool, String> {
    let routes = app
        .try_state::<ContextMenus>()
        .ok_or("The native attachment menu owner is unavailable.")?;
    let result = routes
        .0
        .lock()
        .map_err(|_| "The native attachment menu owner stopped.")?
        .revoke(lease);
    Ok(result)
}

pub fn install(app: &tauri::App) -> tauri::Result<()> {
    install_with_file_paste(app, None)
}

/// The host supplies the exact view/target capture owner. A missing callback
/// leaves the existing native menu intact and cannot advertise file interception.
pub fn install_with_file_paste(
    app: &tauri::App,
    file_paste: Option<FilePasteHandler>,
) -> tauri::Result<()> {
    if app.try_state::<ContextMenus>().is_some() {
        return Err(
            std::io::Error::other("The native desktop menu owner is already installed.").into(),
        );
    }
    let menu = Menu::default(app.handle())?;
    let paste_action = studio_client_core::attachment_selections::contract()["pasteMenuAction"]
        .as_str()
        .expect("native paste menu identity")
        .to_owned();
    let native_paste_label = PredefinedMenuItem::paste(app, None)?.text()?;
    let mut replacements = 0;
    for item in menu.items()? {
        let MenuItemKind::Submenu(submenu) = item else {
            continue;
        };
        if submenu.text()? == app.package_info().name {
            submenu.set_text("LAYR Studio")?;
        }
        if file_paste.is_some() && submenu.text()? == "Edit" {
            for (index, item) in submenu.items()?.into_iter().enumerate() {
                if let MenuItemKind::Predefined(item) = item
                    && item.text()? == native_paste_label
                {
                    submenu.remove(&item)?;
                    submenu.insert(
                        &MenuItem::with_id(
                            app,
                            paste_action.as_str(),
                            &native_paste_label,
                            true,
                            Some("CmdOrCtrl+V"),
                        )?,
                        index,
                    )?;
                    replacements += 1;
                }
            }
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
    if file_paste.is_some() && replacements != 1 {
        return Err(std::io::Error::other(
            "The native Paste menu could not be bound exactly once.",
        )
        .into());
    }
    app.set_menu(menu)?;
    app.manage(Zoom::default());
    app.manage(ContextMenus(Mutex::new(PopupRoutes {
        menu_intercepted: file_paste.is_some(),
        ..Default::default()
    })));
    app.on_menu_event(move |app, event| {
        let result: Result<(), String> = (|| {
            let id = event.id().as_ref();
            if id.starts_with(
                studio_client_core::attachment_selections::contract()["popupPasteActionPrefix"]
                    .as_str()
                    .expect("attachment popup action prefix"),
            ) {
                let capture = app
                    .state::<ContextMenus>()
                    .0
                    .lock()
                    .map_err(|_| "The native attachment menu owner stopped.")?
                    .take(id)
                    .ok_or("The native attachment popup target is no longer active.")?;
                return dispatch_paste(
                    || capture(app),
                    studio_client_core::desktop::forward_text_paste,
                );
            }
            if id == paste_action {
                let capture = file_paste
                    .as_ref()
                    .ok_or("The native file paste owner is unavailable.")?;
                return dispatch_paste(
                    || capture(app),
                    studio_client_core::desktop::forward_text_paste,
                );
            }
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

fn dispatch_paste(
    capture: impl FnOnce() -> Result<bool, String>,
    text: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    if !capture()? {
        text()?;
    }
    Ok(())
}

#[cfg(test)]
mod attachment_paste_tests {
    use super::{FilePasteHandler, PopupRoutes, dispatch_paste};
    #[test]
    fn native_attachment_popup_keeps_its_frozen_lease_and_rejects_stale_callbacks() {
        let first: FilePasteHandler = std::sync::Arc::new(|_| Ok(true));
        let second: FilePasteHandler = std::sync::Arc::new(|_| Ok(false));
        let mut routes = PopupRoutes::default();
        routes.arm("popup-a".into(), "lease-a".into(), first);
        routes.arm("popup-b".into(), "lease-b".into(), second.clone());
        assert!(!routes.revoke("lease-a"));
        assert!(routes.take("popup-a").is_none());
        assert!(std::sync::Arc::ptr_eq(
            &routes.take("popup-b").unwrap(),
            &second
        ));
        assert!(routes.take("popup-b").is_none());
        routes.arm("popup-c".into(), "lease-c".into(), second);
        assert!(routes.revoke("lease-c"));
        assert!(routes.take("popup-c").is_none());
    }
    #[test]
    fn native_attachment_menu_leaves_text_with_its_responder_and_holds_unknown_file_capture() {
        let forwarded = std::cell::Cell::new(0);
        dispatch_paste(
            || Ok(false),
            || {
                forwarded.set(forwarded.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(forwarded.get(), 1);
        dispatch_paste(
            || Ok(true),
            || {
                forwarded.set(forwarded.get() + 1);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            forwarded.get(),
            1,
            "captured file paste must not fall through to the WebView"
        );
        assert!(
            dispatch_paste(
                || Err("Unknown native capture".into()),
                || {
                    forwarded.set(forwarded.get() + 1);
                    Ok(())
                }
            )
            .is_err()
        );
        assert_eq!(
            forwarded.get(),
            1,
            "unknown native capture must preserve the hold"
        );
    }
}
