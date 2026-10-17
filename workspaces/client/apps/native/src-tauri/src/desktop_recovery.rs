//! The installed desktop shares Electron's guarded LaunchAgent configuration.
use std::{path::PathBuf, process::Command};
use tauri::{
    Manager,
    menu::{CheckMenuItem, MenuItem, MenuItemKind},
};

pub struct Intent {
    path: PathBuf,
    value: serde_json::Value,
}
fn write_intent(intent: &Intent, open: bool) -> Result<(), String> {
    let mut value = intent.value.clone();
    value["desiredOpen"] = open.into();
    let temporary = intent.path.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_vec(&value).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(temporary, &intent.path).map_err(|e| e.to_string())
}
fn preference(state: &std::path::Path) -> Result<bool, String> {
    match std::fs::read(state.join("background-recovery.json")) {
        Ok(bytes) => {
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
            value["enabled"]
                .as_bool()
                .ok_or("Invalid background recovery setting.".into())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.to_string()),
    }
}
pub fn install(app: &tauri::App, profile: &std::path::Path, hidden: bool) -> Result<(), String> {
    if hidden {
        return Ok(());
    }
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    if executable
        != std::path::Path::new(
            "/Applications/Codex Studio.app/Contents/MacOS/studio-native-client",
        )
    {
        return Ok(());
    }
    if std::env::var_os("CODEX_DESKTOP_PROFILE").is_some()
        && !std::env::args().any(|arg| arg == "--background-recovery")
    {
        return Ok(());
    }
    let state = crate::local_backend::state_directory()?;
    std::fs::create_dir_all(&state).map_err(|e| e.to_string())?;
    let state = state.canonicalize().map_err(|e| e.to_string())?;
    let process = Command::new("/bin/ps")
        .args(["-p", &std::process::id().to_string(), "-o", "lstart="])
        .env("LC_ALL", "C")
        .output()
        .map_err(|e| e.to_string())?;
    let started = String::from_utf8_lossy(&process.stdout).trim().to_owned();
    if !process.status.success() || started.is_empty() {
        return Err("Cannot identify the desktop process.".into());
    }
    let intent = Intent {
        path: state.join("desktop-recovery.json"),
        value: serde_json::json!({
            "version": 1, "desiredOpen": true, "pid": std::process::id(), "startedAt": started, "executable": executable, "profile": profile
        }),
    };
    write_intent(&intent, true)?;
    app.manage(intent);
    let enabled = preference(&state);
    let status = MenuItem::with_id(
        app,
        "background-recovery-status",
        match &enabled {
            Ok(true) => "Background recovery: On",
            Ok(false) => "Background recovery: Off",
            Err(_) => "Background recovery: Unavailable",
        },
        false,
        None::<&str>,
    )
    .map_err(|e| e.to_string())?;
    let toggle = CheckMenuItem::with_id(
        app,
        "background-recovery-toggle",
        "Restore server after login or failure",
        true,
        enabled.unwrap_or(false),
        None::<&str>,
    )
    .map_err(|e| e.to_string())?;
    let menu = app.menu().ok_or("Desktop menu is unavailable.")?;
    let first = menu.items().map_err(|e| e.to_string())?.into_iter().next();
    if let Some(MenuItemKind::Submenu(submenu)) = first {
        submenu
            .insert_items(&[&status, &toggle], 1)
            .map_err(|e| e.to_string())?;
    }
    let resources = app.path().resource_dir().map_err(|e| e.to_string())?;
    app.on_menu_event(move |app, event| {
        if event.id().as_ref() != "background-recovery-toggle" {
            return;
        }
        let requested = toggle.is_checked().unwrap_or(false);
        let _ = toggle.set_enabled(false);
        let _ = status.set_text("Background recovery: Updating…");
        let (toggle, status, resources, executable, state, app) = (
            toggle.clone(),
            status.clone(),
            resources.clone(),
            executable.clone(),
            state.clone(),
            app.clone(),
        );
        tauri::async_runtime::spawn_blocking(move || {
            let result = configure(&resources, &executable, requested);
            let actual = preference(&state);
            let _ = toggle.set_checked(actual.as_ref().copied().unwrap_or(false));
            let _ = toggle.set_enabled(true);
            let _ = status.set_text(match (&result, &actual) {
                (Ok(()), Ok(true)) => "Background recovery: On",
                (Ok(()), Ok(false)) => "Background recovery: Off",
                _ => "Background recovery: Unavailable",
            });
            if let Err(error) = result {
                use tauri::Emitter;
                eprintln!("Cannot change background recovery: {error}");
                let _ = app.emit(
                    "studio-desktop-error",
                    serde_json::json!({"message": error}),
                );
            }
        });
    });
    Ok(())
}
fn configure(
    resources: &std::path::Path,
    executable: &std::path::Path,
    enabled: bool,
) -> Result<(), String> {
    let mut candidates = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join("node"))
        .collect::<Vec<_>>();
    candidates.extend(["/opt/homebrew/bin/node", "/usr/local/bin/node"].map(PathBuf::from));
    let node = candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or("Node.js is unavailable for desktop recovery configuration.")?;
    let output = Command::new(node)
        .arg(resources.join("helpers/recovery.cjs"))
        .arg(executable)
        .arg(resources)
        .arg(enabled.to_string())
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let receipt: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| e.to_string())?;
    if receipt["enabled"] != enabled {
        return Err("Recovery configuration returned an unexpected setting.".into());
    }
    Ok(())
}
pub fn close(app: &tauri::AppHandle) {
    if let Some(intent) = app.try_state::<Intent>()
        && let Err(error) = write_intent(&intent, false)
    {
        eprintln!("Cannot record desktop close: {error}");
        // Withdrawing our intent is safer than reopening after explicit Quit.
        if let Err(error) = std::fs::remove_file(&intent.path) {
            eprintln!("Cannot withdraw desktop intent: {error}");
        }
    }
}
