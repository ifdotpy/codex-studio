use std::sync::Mutex;
use tauri::{Emitter, Manager, State, WebviewWindow};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Close,
    Reload,
}

#[derive(Default)]
struct Gate {
    enabled: bool,
    serial: u64,
    pending: Option<(u64, Action)>,
}
#[derive(Default)]
pub struct CloseState(Mutex<Gate>);

impl Gate {
    fn begin(&mut self, action: Action) -> Option<u64> {
        if !self.enabled || self.pending.is_some() {
            return None;
        }
        self.serial += 1;
        self.pending = Some((self.serial, action));
        Some(self.serial)
    }
    fn finish(&mut self, id: u64) -> Result<Action, String> {
        if self.pending.map(|pending| pending.0) != Some(id) {
            return Err("This close request is no longer active.".into());
        }
        Ok(self.pending.take().unwrap().1)
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
    let mut gate = state.0.lock().map_err(|_| "The close service stopped.")?;
    let action = gate
        .pending
        .filter(|pending| pending.0 == id)
        .map(|pending| pending.1)
        .ok_or("This close request is no longer active.")?;
    if ready && action == Action::Reload {
        // A failed reload keeps the receipt pending so the renderer can release
        // its save barrier with ready=false and show the failure.
        window.reload().map_err(|error| error.to_string())?;
        gate.enabled = false;
    }
    gate.finish(id)?;
    if ready && action == Action::Close {
        // Explicit exit code distinguishes an acknowledged close from OS quit.
        window.app_handle().exit(0);
    }
    Ok(())
}

pub fn request(app: &tauri::AppHandle) -> bool {
    request_action(app, Action::Close)
}

pub fn reload(app: &tauri::AppHandle) -> Result<(), String> {
    if !request_action(app, Action::Reload) {
        app.get_webview_window("main")
            .ok_or("The Studio window is unavailable.")?
            .reload()
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn request_action(app: &tauri::AppHandle, action: Action) -> bool {
    let state = app.state::<CloseState>();
    let Ok(mut gate) = state.0.lock() else {
        return true;
    };
    if !gate.enabled {
        return false;
    }
    if let Some(id) = gate.begin(action) {
        let delivered = app.get_webview_window("main").is_some_and(|window| {
            window
                .emit("studio-close-request", serde_json::json!({ "id": id, "action": match action { Action::Close => "close", Action::Reload => "reload" } }))
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
    use super::{Action, Gate};
    #[test]
    fn reload_acknowledges_only_its_own_request() {
        let mut gate = Gate {
            enabled: true,
            ..Default::default()
        };
        let id = gate.begin(Action::Reload).unwrap();
        assert_eq!(gate.begin(Action::Close), None);
        assert!(gate.finish(id + 1).is_err());
        assert_eq!(gate.finish(id).unwrap(), Action::Reload);
        let close = gate.begin(Action::Close).unwrap();
        assert_eq!(gate.finish(close).unwrap(), Action::Close);
    }
    #[test]
    fn closing_requires_a_fresh_acknowledgement() {
        let mut gate = Gate::default();
        assert_eq!(gate.begin(Action::Close), None);
        assert!(gate.finish(1).is_err());
        gate.enabled = true;
        let first = gate.begin(Action::Close).unwrap();
        assert_eq!(gate.begin(Action::Close), None);
        assert!(gate.finish(first + 1).is_err());
        gate.finish(first).unwrap();
        let second = gate.begin(Action::Close).unwrap();
        assert_ne!(first, second);
        assert!(gate.finish(first).is_err());
        gate.finish(second).unwrap();
        assert!(gate.finish(second).is_err());
    }
}
