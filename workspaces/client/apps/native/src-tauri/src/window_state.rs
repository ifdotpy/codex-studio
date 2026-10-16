//! Remember normal window geometry without writing on every resize frame.
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tauri::{Manager, PhysicalPosition, PhysicalSize, WindowEvent};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
struct Bounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Saved {
    version: u8,
    bounds: Bounds,
    maximized: bool,
}
struct Pending {
    path: PathBuf,
    value: Saved,
    timer: Option<tauri::async_runtime::JoinHandle<()>>,
}
pub struct WindowState(Arc<Mutex<Pending>>);

// All rectangles here use physical pixels, including monitor work areas.
fn fit(bounds: Bounds, areas: &[Bounds]) -> Option<Bounds> {
    let area = areas
        .iter()
        .filter(|area| area.width > 0 && area.height > 0)
        .max_by_key(|area| {
            let width = ((bounds.x as i64 + bounds.width as i64)
                .min(area.x as i64 + area.width as i64)
                - (bounds.x as i64).max(area.x as i64))
            .max(0);
            let height = ((bounds.y as i64 + bounds.height as i64)
                .min(area.y as i64 + area.height as i64)
                - (bounds.y as i64).max(area.y as i64))
            .max(0);
            width * height
        })?;
    let width = bounds.width.clamp(420.min(area.width), area.width);
    let height = bounds.height.clamp(600.min(area.height), area.height);
    Some(Bounds {
        x: (bounds.x as i64).clamp(area.x as i64, area.x as i64 + (area.width - width) as i64)
            as i32,
        y: (bounds.y as i64).clamp(area.y as i64, area.y as i64 + (area.height - height) as i64)
            as i32,
        width,
        height,
    })
}
fn write(pending: &Pending) -> Result<(), String> {
    let temporary = pending.path.with_extension("json.tmp");
    let data = serde_json::to_vec(&pending.value).map_err(|e| e.to_string())?;
    std::fs::write(&temporary, data).map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, &pending.path).map_err(|e| e.to_string())
}
pub fn install(
    window: &tauri::WebviewWindow,
    directory: &std::path::Path,
    hidden: bool,
) -> Result<(), String> {
    // Automated checks must neither restore a maximized visible window nor save
    // their hidden geometry into any profile.
    if hidden {
        return Ok(());
    }
    let path = directory.join("native-window-state.json");
    let saved = match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice::<Saved>(&bytes)
            .ok()
            .filter(|s| s.version == 1 && s.bounds.width > 0 && s.bounds.height > 0),
        Err(_) => None,
    };
    if let Some(saved) = &saved {
        let areas = window
            .available_monitors()
            .map_err(|e| e.to_string())?
            .iter()
            .map(|monitor| {
                let area = monitor.work_area();
                Bounds {
                    x: area.position.x,
                    y: area.position.y,
                    width: area.size.width,
                    height: area.size.height,
                }
            })
            .collect::<Vec<_>>();
        if let Some(bounds) = fit(saved.bounds, &areas) {
            window
                .set_size(PhysicalSize::new(bounds.width, bounds.height))
                .map_err(|e| e.to_string())?;
            window
                .set_position(PhysicalPosition::new(bounds.x, bounds.y))
                .map_err(|e| e.to_string())?;
        }
    }
    let position = window.outer_position().map_err(|e| e.to_string())?;
    let size = window.inner_size().map_err(|e| e.to_string())?;
    let state = WindowState(Arc::new(Mutex::new(Pending {
        path,
        timer: None,
        value: Saved {
            version: 1,
            bounds: Bounds {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
            },
            maximized: saved.is_some_and(|s| s.maximized),
        },
    })));
    let maximize = state
        .0
        .lock()
        .map_err(|_| "Window state stopped.")?
        .value
        .maximized;
    window.app_handle().manage(state);
    if maximize {
        window.maximize().map_err(|e| e.to_string())?;
    }
    Ok(())
}
pub fn event(window: &tauri::Window, event: &WindowEvent) {
    if window.label() != "main" || !matches!(event, WindowEvent::Moved(_) | WindowEvent::Resized(_))
    {
        return;
    }
    let Some(state) = window.app_handle().try_state::<WindowState>() else {
        return;
    };
    if window.is_minimized().unwrap_or(true) || window.is_fullscreen().unwrap_or(true) {
        return;
    }
    let Ok(maximized) = window.is_maximized() else {
        return;
    };
    let Ok(mut pending) = state.0.lock() else {
        return;
    };
    pending.value.maximized = maximized;
    if !maximized {
        if let (Ok(position), Ok(size)) = (window.outer_position(), window.inner_size()) {
            if size.width == 0 || size.height == 0 {
                return;
            }
            pending.value.bounds = Bounds {
                x: position.x,
                y: position.y,
                width: size.width,
                height: size.height,
            };
        }
    }
    if let Some(timer) = pending.timer.take() {
        timer.abort();
    }
    let shared = state.0.clone();
    pending.timer = Some(tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        if let Ok(pending) = shared.lock() {
            if let Err(error) = write(&pending) {
                eprintln!("Cannot save window geometry: {error}");
            }
        }
    }));
}
pub fn flush(app: &tauri::AppHandle) {
    let Some(state) = app.try_state::<WindowState>() else {
        return;
    };
    if let Ok(mut pending) = state.0.lock() {
        if let Some(timer) = pending.timer.take() {
            timer.abort();
        }
        if let Err(error) = write(&pending) {
            eprintln!("Cannot save window geometry: {error}");
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disconnected_display_and_oversized_bounds_remain_accessible() {
        let area = Bounds {
            x: -1440,
            y: 30,
            width: 1440,
            height: 870,
        };
        assert_eq!(
            fit(
                Bounds {
                    x: 6000,
                    y: -900,
                    width: 3000,
                    height: 2000
                },
                &[area]
            ),
            Some(area)
        );
        let saved = Bounds {
            x: -1300,
            y: 100,
            width: 800,
            height: 650,
        };
        assert_eq!(fit(saved, &[area]), Some(saved));
        assert_eq!(fit(saved, &[]), None);
    }
}
