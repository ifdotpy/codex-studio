//! Free space of the state volume: the desktop warning and the disk-full state.
//!
//! This mirrors `codex_disk_space.py`. The thresholds and the error pattern are
//! generated shared values owned by `codex_shared_values.py`. Below
//! `DISK_FULL_ENTER_FREE_BYTES` the disk is full: no new turn starts, and an
//! agent whose turn failed for lack of space waits as a capacity retry with the
//! cause `diskFull`. The full state ends at or above `DISK_FULL_EXIT_FREE_BYTES`.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use studio_runtime_schema::shared_values::{
    DISK_FULL_ENTER_FREE_BYTES, DISK_FULL_ERROR_PATTERN, DISK_FULL_EXIT_FREE_BYTES,
    DISK_FULL_RECHECK_SECONDS, DISK_SPACE_CRITICAL_FREE_BYTES, DISK_SPACE_WARNING_FREE_BYTES,
};

/// The capacity-retry cause and error kind of a turn that failed because the disk was full.
pub const DISK_FULL_KIND: &str = "diskFull";
/// The chat notice for a turn that failed because the disk was full.
pub const ERROR_MESSAGE: &str =
    "Disk is full. Studio holds this agent and resumes it when free space returns.";
/// The interval between free-space checks while the disk is full.
pub const RECHECK: Duration = Duration::from_secs(DISK_FULL_RECHECK_SECONDS);
/// A free-space sample is reused for this long; dispatch can run many times a second.
const SAMPLE: Duration = Duration::from_secs(1);
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;

type Sampler = Arc<dyn Fn(&Path) -> Option<u64> + Send + Sync>;

/// Bytes available to this process on the volume that holds `path`.
pub fn free_bytes(path: &Path) -> Option<u64> {
    let stats = rustix::fs::statvfs(path).ok()?;
    Some(stats.f_bavail.saturating_mul(stats.f_frsize))
}

fn pattern() -> &'static regex::Regex {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        regex::RegexBuilder::new(DISK_FULL_ERROR_PATTERN)
            .case_insensitive(true)
            .build()
            .expect("the shared disk-full pattern is a valid regular expression")
    })
}

/// True when a provider or local error object or text means the disk is full.
pub fn is_disk_full_error(error: &Value) -> bool {
    match error {
        Value::String(text) => pattern().is_match(text),
        Value::Object(fields) => {
            fields.get("codexErrorInfo").and_then(Value::as_str) == Some(DISK_FULL_KIND)
                || fields
                    .get("data")
                    .and_then(|data| data.get("diskFull"))
                    .and_then(Value::as_bool)
                    == Some(true)
                || fields
                    .get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|text| pattern().is_match(text))
        }
        _ => false,
    }
}

fn mib(free: Option<u64>) -> String {
    free.map_or_else(
        || "unknown".to_owned(),
        |free| format!("{} MiB", free / MIB),
    )
}

/// The reason shown for an agent that waits for free space.
pub fn held_reason(free: Option<u64>) -> String {
    format!(
        "Disk is full: {} free. Studio resumes this agent when free space returns.",
        mib(free)
    )
}

/// The one desktop notice while the disk is full.
pub fn admission_message(free: Option<u64>) -> String {
    format!(
        "Disk is full: {} free. Studio starts no new turns and resumes held agents when {} GiB is free. Free disk space now.",
        mib(free),
        DISK_FULL_EXIT_FREE_BYTES / GIB
    )
}

/// The desktop level: full, critical, warning, or none.
pub fn level(free: u64, full: bool) -> Option<&'static str> {
    if full || free < DISK_FULL_ENTER_FREE_BYTES {
        Some("full")
    } else if free < DISK_SPACE_CRITICAL_FREE_BYTES {
        Some("critical")
    } else if free < DISK_SPACE_WARNING_FREE_BYTES {
        Some("warning")
    } else {
        None
    }
}

fn message(free: u64, level: Option<&str>) -> Option<String> {
    let amount = format!("{:.1} GB", free as f64 / GIB as f64);
    match level? {
        "full" => Some(admission_message(Some(free))),
        "critical" => Some(format!(
            "The Mac disk has only {amount} free. VMs, Docker and Studio can fail to write. Free disk space now."
        )),
        _ => Some(format!(
            "The Mac disk has only {amount} free. Free disk space soon."
        )),
    }
}

#[derive(Default)]
struct GateState {
    full: bool,
    observed_until: Option<Instant>,
    sampled_at: Option<Instant>,
    free: Option<u64>,
}

/// The disk-full state of one state volume, with hysteresis between the entry
/// and exit thresholds and a hold after an observed disk-full write error.
pub struct DiskGate {
    root: PathBuf,
    sampler: Sampler,
    state: Mutex<GateState>,
}

impl DiskGate {
    pub fn new(root: PathBuf) -> Self {
        Self::with_sampler(root, Arc::new(free_bytes))
    }

    /// A gate whose free-space samples come from `sampler` (tests inject values here).
    pub fn with_sampler(root: PathBuf, sampler: Sampler) -> Self {
        Self {
            root,
            sampler,
            state: Mutex::new(GateState::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, GateState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The free bytes while the disk is full, else None. An unreadable volume does not block work.
    pub fn full_free_bytes_at(&self, now: Instant) -> Option<u64> {
        let mut state = self.lock();
        if state
            .sampled_at
            .is_none_or(|at| now.saturating_duration_since(at) >= SAMPLE)
        {
            state.free = (self.sampler)(&self.root);
            state.sampled_at = Some(now);
        }
        let free = state.free?;
        let observed = state.observed_until.is_some_and(|until| now < until);
        if free < DISK_FULL_ENTER_FREE_BYTES {
            state.full = true;
        } else if free >= DISK_FULL_EXIT_FREE_BYTES && !observed {
            state.full = false;
        }
        (state.full || observed).then_some(free)
    }

    pub fn full_free_bytes(&self) -> Option<u64> {
        self.full_free_bytes_at(Instant::now())
    }

    /// Record a write that failed because the disk is full.
    pub fn observe_full_at(&self, now: Instant) {
        let mut state = self.lock();
        state.full = true;
        state.observed_until = Some(now + RECHECK);
        state.sampled_at = None;
    }

    /// The `/api/desktop` `diskSpace` object.
    pub fn status(&self) -> Value {
        let full = self.full_free_bytes();
        let free = self.lock().free;
        status_value(free, full.is_some())
    }
}

fn status_value(free: Option<u64>, full: bool) -> Value {
    let Some(free) = free else {
        return json!({"level": null, "freeBytes": null, "message": null});
    };
    let level = level(free, full);
    json!({"level": level, "freeBytes": free, "message": message(free, level)})
}

static GATE: OnceLock<DiskGate> = OnceLock::new();

/// Install the process gate for the state directory. The first call wins.
pub fn install(root: PathBuf) -> &'static DiskGate {
    GATE.get_or_init(|| DiskGate::new(root))
}

/// The free bytes while the state volume is full, else None (also when no gate is installed).
pub fn full_free_bytes() -> Option<u64> {
    GATE.get().and_then(DiskGate::full_free_bytes)
}

/// Record a write that failed because the disk is full.
pub fn observe_full() {
    if let Some(gate) = GATE.get() {
        gate.observe_full_at(Instant::now());
    }
}

/// The desktop status of `root`, through the installed gate when it serves this root.
pub fn status(root: &Path) -> Value {
    match GATE.get() {
        Some(gate) if gate.root == root => gate.status(),
        _ => status_value(free_bytes(root), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn gate(free: &Arc<AtomicU64>) -> DiskGate {
        let free = free.clone();
        DiskGate::with_sampler(
            PathBuf::from("/state"),
            Arc::new(move |_| Some(free.load(Ordering::SeqCst))),
        )
    }

    #[test]
    fn disk_full_errors_are_recognized_in_every_form() {
        let incident = "ENOSPC: no space left on device, rename '/state/sessions/55f43293.meta.json.1.tmp' -> '/state/sessions/55f43293.meta.json'";
        for error in [
            json!({"message": incident}),
            json!(incident),
            json!({"message": "Codex failed", "codexErrorInfo": "diskFull"}),
            json!({"message": "Disk is full.", "data": {"diskFull": true}}),
            json!({"message": "database or disk is full"}),
            json!({"message": "No space left on device (os error 28)"}),
        ] {
            assert!(is_disk_full_error(&error), "{error}");
        }
        for error in [
            json!({"message": "Model is at capacity", "codexErrorInfo": "serverOverloaded"}),
            json!("permission denied"),
            Value::Null,
            json!({}),
        ] {
            assert!(!is_disk_full_error(&error), "{error}");
        }
    }

    #[test]
    fn the_full_state_uses_hysteresis_and_observed_errors() {
        let free = Arc::new(AtomicU64::new(DISK_FULL_ENTER_FREE_BYTES - 1));
        let gate = gate(&free);
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        assert_eq!(
            gate.full_free_bytes_at(at(0)),
            Some(DISK_FULL_ENTER_FREE_BYTES - 1)
        );
        free.store(DISK_FULL_EXIT_FREE_BYTES - 1, Ordering::SeqCst);
        assert_eq!(
            gate.full_free_bytes_at(at(2)),
            Some(DISK_FULL_EXIT_FREE_BYTES - 1),
            "the full state ends only at the exit threshold"
        );
        free.store(DISK_FULL_EXIT_FREE_BYTES, Ordering::SeqCst);
        assert_eq!(gate.full_free_bytes_at(at(4)), None);
        free.store(DISK_FULL_ENTER_FREE_BYTES, Ordering::SeqCst);
        assert_eq!(gate.full_free_bytes_at(at(6)), None);
        gate.observe_full_at(at(8));
        assert_eq!(
            gate.full_free_bytes_at(at(8)),
            Some(DISK_FULL_ENTER_FREE_BYTES),
            "an observed ENOSPC holds starts even when the sample shows free space"
        );
        free.store(100 * GIB, Ordering::SeqCst);
        assert!(
            gate.full_free_bytes_at(at(8 + RECHECK.as_secs() - 1))
                .is_some()
        );
        assert_eq!(gate.full_free_bytes_at(at(8 + RECHECK.as_secs())), None);
        let unreadable = DiskGate::with_sampler(PathBuf::from("/state"), Arc::new(|_| None));
        assert_eq!(unreadable.full_free_bytes_at(at(0)), None);
    }

    #[test]
    fn status_and_messages_match_the_python_owner() {
        let free = Arc::new(AtomicU64::new(130 * MIB));
        assert_eq!(
            gate(&free).status(),
            json!({
                "level": "full",
                "freeBytes": 130 * MIB,
                "message": "Disk is full: 130 MiB free. Studio starts no new turns and resumes held agents when 2 GiB is free. Free disk space now."
            })
        );
        assert_eq!(
            held_reason(Some(130 * MIB)),
            "Disk is full: 130 MiB free. Studio resumes this agent when free space returns."
        );
        assert_eq!(
            status_value(Some(3 * GIB), false)["message"],
            "The Mac disk has only 3.0 GB free. VMs, Docker and Studio can fail to write. Free disk space now."
        );
        assert_eq!(
            status_value(Some(100 * GIB), false),
            json!({"level": null, "freeBytes": 100 * GIB, "message": null})
        );
        assert_eq!(
            status_value(None, false),
            json!({"level": null, "freeBytes": null, "message": null})
        );
        assert_eq!(
            level(DISK_SPACE_WARNING_FREE_BYTES - 1, false),
            Some("warning")
        );
    }
}
