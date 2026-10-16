//! Opt-in hidden WebView check. Never compiled into the normal application.
use std::path::PathBuf;
use tauri::{Listener, Manager};

pub fn directory() -> Option<PathBuf> {
    std::env::var_os("STUDIO_NATIVE_CHECK_DIR").map(PathBuf::from)
}
pub fn persistent_profile() -> bool {
    directory().is_some()
        && std::env::var("STUDIO_NATIVE_CHECK_MODE").is_ok_and(|mode| {
            matches!(
                mode.as_str(),
                "profile-store-write" | "profile-store-read" | "profile-store-empty"
            )
        })
}
// Named test stores exercise persistent IndexedDB without touching the installed
// WebKit profile. Incognito WebKit cannot prepare all Blob/File writes.
pub fn data_store_identifier() -> Result<Option<[u8; 16]>, String> {
    if directory().is_none() {
        return Ok(None);
    }
    let Ok(value) = std::env::var("STUDIO_NATIVE_CHECK_STORE_ID") else {
        return Ok(None);
    };
    if value.len() != 32 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Invalid isolated WebKit store identifier.".into());
    }
    let mut bytes = [0; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte =
            u8::from_str_radix(&value[index * 2..index * 2 + 2], 16).map_err(|e| e.to_string())?;
    }
    Ok(Some(bytes))
}
pub fn setup(app: &mut tauri::App) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(directory) = directory() else {
        return Ok(false);
    };
    if !directory.is_absolute() {
        return Err("The native check requires an absolute isolated directory.".into());
    }
    eprintln!("Hidden native check: setup");
    for (method, path, status) in [
        ("GET", "/", 200),
        ("HEAD", "/index.html", 200),
        ("GET", "/assets/studio-check-missing.js", 404),
        ("GET", "/api/session", 404),
        ("POST", "/index.html", 405),
    ] {
        let request = tauri::http::Request::builder()
            .method(method)
            .uri(format!("studio-view://studio-nrxwgylm.localhost{path}"))
            .body(Vec::new())?;
        let response = crate::frame_assets::respond_with_app(app.handle(), request);
        if response.status().as_u16() != status || (method == "HEAD" && !response.body().is_empty())
        {
            return Err(format!("Bundled asset boundary failed: {method} {path}").into());
        }
    }
    app.listen("studio-native-check-step", |event| {
        eprintln!("Hidden native step: {}", event.payload());
    });
    std::fs::create_dir_all(&directory)?;
    let result_file = directory.join("webview-result.json");
    let handle = app.handle().clone();
    app.once("studio-native-check-result", move |event| {
        eprintln!("Hidden native check: {}", event.payload());
        let passed = serde_json::from_str::<serde_json::Value>(event.payload())
            .ok()
            .and_then(|value| value["passed"].as_bool())
            == Some(true);
        let written = std::fs::write(&result_file, event.payload()).is_ok();
        handle.exit(if passed && written { 0 } else { 1 });
    });
    let handle = app.handle().clone();
    let timeout = if std::env::var("STUDIO_NATIVE_CHECK_MODE").as_deref() == Ok("profile-census") {
        600
    } else {
        45
    };
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(timeout));
        eprintln!("The hidden native WebView check timed out.");
        handle.exit(2);
    });
    Ok(true)
}
pub fn loaded(window: &tauri::Webview) {
    if directory().is_none() || window.label() != "main" {
        return;
    }
    eprintln!("Hidden native check: page loaded");
    #[cfg(desktop)]
    {
        let menu = window
            .app_handle()
            .menu()
            .expect("Desktop menu must be installed");
        for id in ["reload-workspace", "reset-zoom", "zoom-in", "zoom-out"] {
            assert!(
                menu.items().unwrap().iter().any(|item| match item {
                    tauri::menu::MenuItemKind::Submenu(submenu) => submenu.get(id).is_some(),
                    _ => false,
                }),
                "Missing desktop command: {id}"
            );
        }
    }
    let mode = std::env::var("STUDIO_NATIVE_CHECK_MODE").unwrap_or_else(|_| "write".into());
    let script = format!(
        "window.__studioCheckMode = {};\n{}",
        serde_json::to_string(&mode).unwrap(),
        include_str!("native-check.js")
    );
    if let Err(error) = window.eval(script) {
        eprintln!("Cannot run the hidden WebView check: {error}");
        window.app_handle().exit(2);
    }
}
