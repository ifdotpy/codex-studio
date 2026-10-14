use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Value, json};

use super::try_with_state;

const STARTUP_MARK_LIMIT: usize = 64;

/// Closed startup stage set prevents caller supplied text entering diagnostics.
#[derive(Clone, Copy, Debug, Ord, PartialOrd, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartupMemoryMark {
    ProcessStarted,
    StoreOpened,
    HttpReady,
}

#[derive(Debug, Default)]
struct StartupState {
    marks: BTreeMap<StartupMemoryMark, Value>,
}

static STARTUP: OnceLock<Mutex<StartupState>> = OnceLock::new();

fn state() -> &'static Mutex<StartupState> {
    STARTUP.get_or_init(|| Mutex::new(StartupState::default()))
}

/// Record an opt-in startup checkpoint once per stage.
///
/// Like the Python collector, collection is disabled unless
/// `CODEX_CANVAS_STARTUP_MEMORY=1`. Current RSS is read from Linux procfs only;
/// peak RSS and allocation traces remain unavailable in this small port.
pub fn mark_startup_memory(stage: StartupMemoryMark) {
    if std::env::var_os("CODEX_CANVAS_STARTUP_MEMORY").as_deref() != Some(std::ffi::OsStr::new("1"))
    {
        return;
    }
    let value = json!({
        "kind": "startupMemory",
        "stage": stage,
        "at": epoch_seconds(),
        "rssBytes": current_rss_bytes(),
        "maxRssBytes": Value::Null,
    });
    let _ = try_with_state(state(), |state| {
        if state.marks.len() < STARTUP_MARK_LIMIT {
            state.marks.entry(stage).or_insert(value);
        }
    });
}

/// Return the bounded startup checkpoint history.
pub fn startup_memory_marks() -> Vec<Value> {
    let mut marks = Vec::new();
    let _ = try_with_state(state(), |state| {
        marks = state.marks.values().cloned().collect()
    });
    marks
}

fn current_rss_bytes() -> Option<u64> {
    let mut status = String::new();
    File::open("/proc/self/status")
        .ok()?
        .take(16 * 1024)
        .read_to_string(&mut status)
        .ok()?;
    let kilobytes = status.lines().find_map(|line| {
        line.strip_prefix("VmRSS:")?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()
    })?;
    kilobytes.checked_mul(1024)
}

fn epoch_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_probe_only_returns_a_byte_count_or_unavailable() {
        assert!(current_rss_bytes().is_none_or(|bytes| bytes > 0));
    }
}
