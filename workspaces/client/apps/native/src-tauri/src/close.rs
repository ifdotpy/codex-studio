use std::sync::Mutex;
use tauri::{Emitter, Manager, State, WebviewWindow};

#[derive(Default)]
struct Gate {
    enabled: bool,
    serial: u64,
    pending: Option<u64>,
}
#[derive(Default)]
pub struct CloseState(Mutex<Gate>);

impl Gate {
    fn begin(&mut self) -> Option<u64> {
        if !self.enabled || self.pending.is_some() {
            return None;
        }
        self.serial += 1;
        self.pending = Some(self.serial);
        self.pending
    }
    fn finish(&mut self, id: u64) -> Result<(), String> {
        if self.pending != Some(id) {
            return Err("This close request is no longer active.".into());
        }
        self.pending = None;
        Ok(())
    }
}

#[tauri::command]
pub fn close_ready(window: WebviewWindow, state: State<'_, CloseState>) -> Result<(), String> {
    super::trusted(&window)?;
    state
        .0
        .lock()
        .map_err(|_| "The close service stopped.")?
        .enabled = true;
    Ok(())
}

#[tauri::command]
pub fn close_response(
    window: WebviewWindow,
    state: State<'_, CloseState>,
    id: u64,
    ready: bool,
) -> Result<(), String> {
    super::trusted(&window)?;
    state
        .0
        .lock()
        .map_err(|_| "The close service stopped.")?
        .finish(id)?;
    if ready {
        // Explicit exit code distinguishes an acknowledged close from OS quit.
        window.app_handle().exit(0);
    }
    Ok(())
}

pub fn request(app: &tauri::AppHandle) -> bool {
    let state = app.state::<CloseState>();
    let Ok(mut gate) = state.0.lock() else {
        return true;
    };
    if !gate.enabled {
        return false;
    }
    if let Some(id) = gate.begin() {
        let delivered = app.get_webview_window("main").is_some_and(|window| {
            window
                .emit("studio-close-request", serde_json::json!({ "id": id }))
                .is_ok()
        });
        if !delivered {
            // Keep the window open, but let the user retry a failed delivery.
            gate.pending = None;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::Gate;
    #[test]
    fn closing_requires_a_fresh_acknowledgement() {
        let mut gate = Gate::default();
        assert_eq!(gate.begin(), None);
        assert!(gate.finish(1).is_err());
        gate.enabled = true;
        let first = gate.begin().unwrap();
        assert_eq!(gate.begin(), None);
        assert!(gate.finish(first + 1).is_err());
        gate.finish(first).unwrap();
        let second = gate.begin().unwrap();
        assert_ne!(first, second);
        assert!(gate.finish(first).is_err());
        gate.finish(second).unwrap();
        assert!(gate.finish(second).is_err());
    }
}
