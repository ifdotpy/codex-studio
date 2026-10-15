#[cfg(desktop)]
use std::{path::PathBuf, time::Duration};
#[cfg(desktop)]
use studio_client_core::{backend::Backend, startup};

pub fn origin() -> Result<String, String> {
    let port = std::env::var("CODEX_DESKTOP_PORT").unwrap_or_else(|_| "4620".into());
    let port: u16 = port.parse().map_err(|_| "Invalid desktop backend port.")?;
    if port < 1024 {
        return Err("Backend port must be between 1024 and 65535.".into());
    }
    Ok(format!("http://127.0.0.1:{port}"))
}

#[cfg(desktop)]
pub async fn verify(
    origin: &str,
    resources: &std::path::Path,
    allow_start: bool,
) -> Result<(), String> {
    let state =
        if let Some(value) = std::env::var_os("CODEX_AGENTS_STATE_DIR").filter(|v| !v.is_empty()) {
            PathBuf::from(value)
        } else {
            let root =
                if let Some(value) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
                    PathBuf::from(value)
                } else {
                    PathBuf::from(std::env::var_os("HOME").ok_or("Missing home directory.")?)
                        .join(".local/state")
                };
            root.join("codex-agents")
        };
    if allow_start {
        std::fs::create_dir_all(&state).map_err(|error| error.to_string())?;
    }
    let state = state
        .canonicalize()
        .map_err(|e| format!("Cannot locate the backend state directory: {e}"))?;
    if allow_start {
        let backend = Backend::desktop_client(origin)?;
        startup::ensure(&backend, &state, resources).await?;
        return Ok(());
    }
    let saved: serde_json::Value = match std::fs::read(state.join("background-recovery.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|e| format!("Cannot read backend recovery settings: {e}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::Value::Null,
        Err(error) => return Err(error.to_string()),
    };
    if saved
        .get("supervisorEnabled")
        .is_some_and(|value| !value.is_boolean())
    {
        return Err("The saved supervisor setting is invalid.".into());
    }
    let supervisor = std::env::var("CODEX_AGENTS_SUPERVISOR_MODE")
        .map(|value| value == "1")
        .unwrap_or(saved["supervisorEnabled"] == true);
    let backend = Backend::desktop_client(origin)?;
    let identity = startup::identity(
        &backend,
        &state,
        Duration::from_secs(5),
        Duration::from_secs(15),
    )
    .await?
    .ok_or(
        "The local backend is not running. Start the installed Studio backend before connecting.",
    )?;
    if supervisor && identity["supervisorMode"] != true && identity["supervisorFallback"] != true {
        return Err("Supervisor mode is enabled, but the running backend does not use it. Use the coordinated restart preflight before attaching.".into());
    }
    Ok(())
}

#[cfg(mobile)]
pub async fn verify(
    _origin: &str,
    _resources: &std::path::Path,
    _allow_start: bool,
) -> Result<(), String> {
    Err("Pair this mobile application with a Studio server first.".into())
}
