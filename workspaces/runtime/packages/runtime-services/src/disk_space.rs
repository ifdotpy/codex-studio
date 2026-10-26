//! One storage admission owner for state, worker workspace and build filesystems.
//!
//! State thresholds and the error pattern are owned by
//! `studio_runtime_schema::shared_values`; generated Python values share them.
//! Worker workspace and build volumes use the same disk-full floor as the
//! state volume: a worker waits only when its volume is really full. Below
//! `DISK_FULL_ENTER_FREE_BYTES` the disk is full: no new turn starts, and an
//! agent whose turn failed for lack of space waits as a capacity retry with the
//! cause `diskFull`. The full state ends at or above `DISK_FULL_EXIT_FREE_BYTES`.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use studio_runtime_schema::desktop::{DiskSpaceLevel, DiskSpaceStatus};
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
fn level(free: u64, full: bool) -> Option<DiskSpaceLevel> {
    if full || free < DISK_FULL_ENTER_FREE_BYTES {
        Some(DiskSpaceLevel::Full)
    } else if free < DISK_SPACE_CRITICAL_FREE_BYTES {
        Some(DiskSpaceLevel::Critical)
    } else if free < DISK_SPACE_WARNING_FREE_BYTES {
        Some(DiskSpaceLevel::Warning)
    } else {
        None
    }
}

fn message(free: u64, level: Option<DiskSpaceLevel>) -> Option<String> {
    let amount = format!("{:.1} GB", free as f64 / GIB as f64);
    match level? {
        DiskSpaceLevel::Full => Some(admission_message(Some(free))),
        DiskSpaceLevel::Critical => Some(format!(
            "The disk has only {amount} free. VMs, Docker and Studio can fail to write. Free disk space now."
        )),
        DiskSpaceLevel::Warning => Some(format!(
            "The disk has only {amount} free. Free disk space soon."
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
    build_roots: WorkerStorageConfig,
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
            build_roots: WorkerStorageConfig::default(),
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
            crate::resource_notify::publish_bounded_source(
                crate::resource_notify::BoundedSource::Desktop,
            );
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
        crate::resource_notify::publish_bounded_source(
            crate::resource_notify::BoundedSource::Desktop,
        );
    }

    /// The `/api/desktop` `diskSpace` object.
    pub fn status(&self) -> DiskSpaceStatus {
        let full = self.full_free_bytes();
        let free = self.lock().free;
        status_value(free, full.is_some())
    }
}

fn status_value(free: Option<u64>, full: bool) -> DiskSpaceStatus {
    let Some(free) = free else {
        return DiskSpaceStatus::default();
    };
    let level = level(free, full);
    DiskSpaceStatus {
        level,
        message: message(free, level),
    }
}

/// Minimum available bytes before starting implementer work on a workspace or
/// build-output filesystem: the disk-full floor of the state volume. Studio does
/// not reserve extra space for agent workloads; it holds work only on a full disk.
pub const WORKER_STORAGE_MIN_FREE_BYTES: u64 = DISK_FULL_ENTER_FREE_BYTES;

/// Additional build-output locations, captured once when the server starts.
#[derive(Clone, Debug, Default)]
pub struct WorkerStorageConfig {
    build_roots: Vec<PathBuf>,
}

impl WorkerStorageConfig {
    pub fn from_environment() -> Result<Self, std::io::Error> {
        Self::parse(
            std::env::var_os("CODEX_WORKER_BUILD_ROOTS"),
            std::env::var_os("CARGO_TARGET_DIR"),
        )
    }

    fn parse(
        roots: Option<std::ffi::OsString>,
        target: Option<std::ffi::OsString>,
    ) -> Result<Self, std::io::Error> {
        let mut build_roots = Vec::new();
        if let Some(roots) = roots {
            for root in std::env::split_paths(&roots) {
                if root.as_os_str().is_empty() || !root.is_absolute() || root.to_str().is_none() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "CODEX_WORKER_BUILD_ROOTS must contain absolute, nonempty UTF-8 paths",
                    ));
                }
                build_roots.push(root);
            }
        }
        if let Some(target) = target {
            let target = PathBuf::from(target);
            if target.to_str().is_none() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "Server CARGO_TARGET_DIR must be a UTF-8 path",
                ));
            }
            build_roots.push(if target.is_absolute() {
                target
            } else {
                std::env::current_dir()?.join(target)
            });
        }
        build_roots.sort();
        build_roots.dedup();
        Ok(Self { build_roots })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum StoragePurpose {
    State,
    Workspace,
    BuildOutput,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StorageRule {
    pub purpose: StoragePurpose,
    pub minimum_free_bytes: u64,
    pub resume_free_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FilesystemSpace {
    pub filesystem_id: Option<String>,
    pub paths: Vec<PathBuf>,
    pub free_bytes: Option<u64>,
    pub rules: Vec<StorageRule>,
    pub state_full: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StorageReport {
    pub filesystems: Vec<FilesystemSpace>,
}

impl StorageReport {
    /// One decision for every watched filesystem. Leads/reviewers retain access
    /// to cleanup while worker storage is low; state-volume ENOSPC holds all roles.
    pub fn hold_reason(&self, role: crate::thread_prepare::Role) -> Option<String> {
        self.filesystems.iter().find_map(|volume| {
            if volume.state_full {
                return Some(held_reason(volume.free_bytes));
            }
            let worker_floor = volume.rules.iter()
                .filter(|rule| rule.purpose != StoragePurpose::State)
                .map(|rule| rule.minimum_free_bytes).max()?;
            if role != crate::thread_prepare::Role::Implementer
                || volume.free_bytes.is_some_and(|free| free >= worker_floor) {
                return None;
            }
            Some(format!("Worker storage held: {} free on {} ({} bytes required). Free space or archive finished workers; queued input is retained.",
                mib(volume.free_bytes), volume.paths.iter().map(|path| path.display().to_string()).collect::<Vec<_>>().join(", "), worker_floor))
        })
    }
}

impl DiskGate {
    pub fn with_config(root: PathBuf, config: WorkerStorageConfig) -> Self {
        let mut gate = Self::new(root);
        gate.build_roots = config;
        gate
    }

    /// Sample outside SQLite callbacks. Paths on the same filesystem share one
    /// observation; a missing new workspace uses its nearest existing ancestor.
    pub fn report(&self, workspaces: &[PathBuf]) -> StorageReport {
        self.report_with_filesystem(workspaces, filesystem_path)
    }

    fn report_with_filesystem(
        &self,
        workspaces: &[PathBuf],
        filesystem: impl Fn(&Path) -> Option<(PathBuf, String)>,
    ) -> StorageReport {
        let state_full = self.full_free_bytes().is_some();
        let mut volumes = BTreeMap::<String, FilesystemSpace>::new();
        for (path, purpose) in std::iter::once((&self.root, StoragePurpose::State))
            .chain(
                workspaces
                    .iter()
                    .map(|path| (path, StoragePurpose::Workspace)),
            )
            .chain(
                self.build_roots
                    .build_roots
                    .iter()
                    .map(|path| (path, StoragePurpose::BuildOutput)),
            )
        {
            let observation = filesystem(path);
            let filesystem_id = observation.as_ref().map(|(_, id)| id.clone());
            let key = filesystem_id
                .clone()
                .unwrap_or_else(|| format!("unknown:{}", path.display()));
            let volume = volumes.entry(key).or_insert_with(|| FilesystemSpace {
                filesystem_id,
                paths: Vec::new(),
                free_bytes: observation
                    .as_ref()
                    .and_then(|(existing, _)| (self.sampler)(existing)),
                rules: Vec::new(),
                state_full: false,
            });
            if !volume.paths.contains(path) {
                volume.paths.push(path.clone());
            }
            if !volume.rules.iter().any(|rule| rule.purpose == purpose) {
                let (minimum_free_bytes, resume_free_bytes) = if purpose == StoragePurpose::State {
                    (DISK_FULL_ENTER_FREE_BYTES, DISK_FULL_EXIT_FREE_BYTES)
                } else {
                    (WORKER_STORAGE_MIN_FREE_BYTES, WORKER_STORAGE_MIN_FREE_BYTES)
                };
                volume.rules.push(StorageRule {
                    purpose,
                    minimum_free_bytes,
                    resume_free_bytes,
                });
            }
            volume.state_full |= purpose == StoragePurpose::State && state_full;
        }
        StorageReport {
            filesystems: volumes.into_values().collect(),
        }
    }
}

fn filesystem_path(path: &Path) -> Option<(PathBuf, String)> {
    for ancestor in path.ancestors() {
        match std::fs::metadata(ancestor) {
            Ok(metadata) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    return Some((ancestor.to_path_buf(), format!("unix:{}", metadata.dev())));
                }
                #[cfg(not(unix))]
                {
                    let _metadata = metadata;
                    return None;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return None,
        }
    }
    None
}

/// Host-wide worker folders: the gate watches storage across teams, including
/// idle workers that may still hold build output. Archived workspaces are excluded.
pub(crate) fn workspace_paths(
    connection: &rusqlite::Connection,
) -> Result<Vec<PathBuf>, studio_runtime_store::StoreError> {
    let mut statement = connection.prepare_cached("SELECT DISTINCT cwd FROM runtime_agents WHERE cwd IS NOT NULL AND cwd<>'' AND (deleted_at IS NULL OR deleted_at IN (0,'')) ORDER BY cwd")?;
    Ok(statement
        .query_map([], |row| row.get::<_, String>(0))?
        .map(|path| path.map(PathBuf::from))
        .collect::<Result<Vec<_>, _>>()?)
}

/// All status and admission callers use the installed gate and its startup config.
pub fn storage_report(root: &Path, workspaces: &[PathBuf]) -> StorageReport {
    match GATE.get() {
        Some(gate) => gate.report(workspaces),
        None => DiskGate::new(root.to_path_buf()).report(workspaces),
    }
}

/// Context is parsed from the agent record at the store boundary; queued input
/// and its epoch are never rewritten by a storage hold.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct TurnStorageContext {
    cwd: PathBuf,
    role: Option<crate::thread_prepare::Role>,
    #[serde(default)]
    is_lead: bool,
}

impl TurnStorageContext {
    fn role(&self) -> crate::thread_prepare::Role {
        self.role.unwrap_or(if self.is_lead {
            crate::thread_prepare::Role::Orchestrator
        } else {
            crate::thread_prepare::Role::Implementer
        })
    }
}

const STORAGE_HOLD_PREFIX: &str = "Storage hold: ";

/// Admission before claiming a turn. Publish the visible reason while retaining
/// the original queued event. The account worker rechecks using its disk wait.
pub async fn hold_agent_turn(
    store: &studio_runtime_store::RuntimeStore,
    agent_id: &str,
    epoch: i64,
    #[cfg(test)] gate_override: Option<&DiskGate>,
) -> Result<bool, studio_runtime_store::StoreError> {
    #[cfg(test)]
    let gate = gate_override.or_else(|| GATE.get());
    #[cfg(not(test))]
    let gate = GATE.get();
    hold_agent_turn_with_gate(store, agent_id, epoch, gate).await
}

async fn hold_agent_turn_with_gate(
    store: &studio_runtime_store::RuntimeStore,
    agent_id: &str,
    epoch: i64,
    gate: Option<&DiskGate>,
) -> Result<bool, studio_runtime_store::StoreError> {
    let id = studio_runtime_store::AgentId::from(agent_id);
    let read_id = id.clone();
    let (context, workspaces) = store
        .read_async("runtime-services/disk_space/context", move |tx| {
            let agent = studio_runtime_store::AgentRepository::get(tx, &read_id)?;
            let context = agent
                .filter(|agent| agent.epoch == epoch)
                .map(|agent| {
                    serde_json::from_value::<TurnStorageContext>(agent.value())
                        .map_err(studio_runtime_store::StoreError::from)
                })
                .transpose()?;
            Ok::<_, studio_runtime_store::StoreError>((context, workspace_paths(tx)?))
        })
        .await?;
    let Some(context) = context else {
        return Ok(true);
    };
    let report = match gate {
        Some(gate) => gate.report(&workspaces),
        None => DiskGate::new(store.root().to_path_buf()).report(&workspaces),
    };
    let reason = report
        .hold_reason(context.role())
        .map(|reason| format!("{STORAGE_HOLD_PREFIX}{reason}"));
    let held = reason.is_some();
    store
        .transaction_with_error(move |tx| {
            let Some(mut agent) = studio_runtime_store::AgentRepository::get(tx, &id)? else {
                return Ok(true);
            };
            if agent.epoch != epoch
                || serde_json::from_value::<TurnStorageContext>(agent.value())? != context
            {
                return Ok(true);
            }
            let previous = agent.extra.get("error").and_then(Value::as_str);
            let changed = match &reason {
                Some(reason) if previous != Some(reason.as_str()) => {
                    agent.extra.insert("error".into(), json!(reason));
                    true
                }
                None if previous.is_some_and(|reason| reason.starts_with(STORAGE_HOLD_PREFIX)) => {
                    agent.extra.remove("error");
                    true
                }
                _ => false,
            };
            use studio_runtime_schema::agent_progress::{
                AgentStartWait, START_WAIT_FIELD, StartWaitReason,
            };
            let previous_wait = agent
                .extra
                .get(START_WAIT_FIELD)
                .cloned()
                .map(serde_json::from_value::<AgentStartWait>)
                .transpose()?;
            let wait_changed = if held {
                let value =
                    serde_json::to_value(AgentStartWait::new(StartWaitReason::Disk, epoch))?;
                if agent.extra.get(START_WAIT_FIELD) != Some(&value) {
                    agent.extra.insert(START_WAIT_FIELD.into(), value);
                    true
                } else {
                    false
                }
            } else if previous_wait
                .is_some_and(|wait| wait.epoch == epoch && wait.reason == StartWaitReason::Disk)
            {
                agent.extra.remove(START_WAIT_FIELD);
                true
            } else {
                false
            };
            if changed || wait_changed {
                studio_runtime_store::AgentRepository::put(tx, &agent)?;
                crate::agents_create::publish_agent(tx, &agent.value())?;
            }
            Ok::<_, studio_runtime_store::StoreError>(held)
        })
        .await
}

static GATE: OnceLock<DiskGate> = OnceLock::new();

/// Install the process gate for the state directory. The first call wins.
pub fn install(root: PathBuf, config: WorkerStorageConfig) -> &'static DiskGate {
    GATE.get_or_init(|| DiskGate::with_config(root, config))
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
pub fn status(root: &Path) -> DiskSpaceStatus {
    match GATE.get() {
        Some(gate) if gate.root == root => gate.status(),
        _ => status_value(free_bytes(root), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn platform_neutral_disk_warnings_keep_urgency_and_free_space() {
        assert_eq!(
            status_value(Some(3 * GIB), false).message.as_deref(),
            Some(
                "The disk has only 3.0 GB free. VMs, Docker and Studio can fail to write. Free disk space now."
            )
        );
        assert_eq!(
            status_value(Some(10 * GIB), false).message.as_deref(),
            Some("The disk has only 10.0 GB free. Free disk space soon.")
        );
        assert_eq!(
            status_value(Some(100 * GIB), false),
            DiskSpaceStatus::default()
        );
    }

    fn synthetic_filesystem(path: &Path) -> Option<(PathBuf, String)> {
        Some((
            path.to_path_buf(),
            if path.starts_with("/build") {
                "outputs"
            } else {
                "root"
            }
            .into(),
        ))
    }

    #[test]
    fn worker_storage_report_is_typed_and_groups_workspace_and_build_filesystems() {
        let mut gate =
            DiskGate::with_sampler(PathBuf::from("/state"), Arc::new(|_| Some(100 * GIB)));
        gate.build_roots = WorkerStorageConfig {
            build_roots: vec![PathBuf::from("/build/target")],
        };
        let report = gate.report_with_filesystem(
            &[PathBuf::from("/workspace/a"), PathBuf::from("/workspace/b")],
            synthetic_filesystem,
        );
        assert_eq!(report.filesystems.len(), 2);
        let root = report
            .filesystems
            .iter()
            .find(|volume| volume.filesystem_id.as_deref() == Some("root"))
            .expect("root filesystem");
        assert_eq!(root.paths.len(), 3);
        assert_eq!(root.free_bytes, Some(100 * GIB));
        assert_eq!(
            root.rules
                .iter()
                .find(|rule| rule.purpose == StoragePurpose::Workspace)
                .expect("workspace rule")
                .minimum_free_bytes,
            WORKER_STORAGE_MIN_FREE_BYTES
        );
        assert_eq!(
            serde_json::from_value::<StorageReport>(
                serde_json::to_value(&report).expect("serialize")
            )
            .expect("typed report"),
            report
        );
        assert!(
            serde_json::from_value::<StorageReport>(json!({"filesystems":[], "unknown":true}))
                .is_err()
        );
    }

    #[test]
    fn worker_storage_is_not_held_while_the_disk_is_not_full() {
        // 2026-10-22: a 64 GiB reserve held every implementer at 12 GiB free.
        let mut gate =
            DiskGate::with_sampler(PathBuf::from("/state"), Arc::new(|_| Some(12 * GIB)));
        gate.build_roots = WorkerStorageConfig {
            build_roots: vec![PathBuf::from("/build/target")],
        };
        let report =
            gate.report_with_filesystem(&[PathBuf::from("/workspace")], synthetic_filesystem);
        assert!(
            report
                .hold_reason(crate::thread_prepare::Role::Implementer)
                .is_none()
        );
        assert_eq!(WORKER_STORAGE_MIN_FREE_BYTES, DISK_FULL_ENTER_FREE_BYTES);
    }

    #[test]
    fn worker_storage_hold_checks_build_volume_and_resumes_at_one_floor() {
        let output_free = Arc::new(AtomicU64::new(WORKER_STORAGE_MIN_FREE_BYTES - 1));
        let samples = Arc::clone(&output_free);
        let mut gate = DiskGate::with_sampler(
            PathBuf::from("/state"),
            Arc::new(move |path| {
                Some(if path.starts_with("/build") {
                    samples.load(Ordering::SeqCst)
                } else {
                    100 * GIB
                })
            }),
        );
        gate.build_roots = WorkerStorageConfig {
            build_roots: vec![PathBuf::from("/build/target")],
        };
        let report =
            || gate.report_with_filesystem(&[PathBuf::from("/workspace")], synthetic_filesystem);
        use crate::thread_prepare::Role;
        assert!(
            report()
                .hold_reason(Role::Implementer)
                .expect("held build volume")
                .contains("/build/target")
        );
        assert!(report().hold_reason(Role::Reviewer).is_none());
        assert!(report().hold_reason(Role::Orchestrator).is_none());
        output_free.store(WORKER_STORAGE_MIN_FREE_BYTES, Ordering::SeqCst);
        assert!(report().hold_reason(Role::Implementer).is_none());
        let unknown = gate.report_with_filesystem(&[PathBuf::from("/workspace")], |_| None);
        assert!(unknown.hold_reason(Role::Implementer).is_some());
    }

    #[test]
    fn worker_storage_config_rejects_invalid_roots_and_accepts_absolute_paths() {
        assert!(WorkerStorageConfig::parse(Some("relative".into()), None).is_err());
        assert!(WorkerStorageConfig::parse(Some("".into()), None).is_err());
        assert!(WorkerStorageConfig::parse(Some("/build::/other".into()), None).is_err());
        let config =
            WorkerStorageConfig::parse(Some("/build:/other".into()), Some("/build/target".into()))
                .expect("absolute output roots");
        assert_eq!(config.build_roots.len(), 3);
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let invalid = std::ffi::OsString::from_vec(b"/build/\xff".to_vec());
            assert!(WorkerStorageConfig::parse(Some(invalid.clone()), None).is_err());
            assert!(WorkerStorageConfig::parse(None, Some(invalid)).is_err());
        }
    }

    #[test]
    fn worker_storage_new_workspace_samples_existing_parent() {
        let root = tempfile::tempdir().expect("fixture folder");
        assert_eq!(
            filesystem_path(&root.path().join("future/worker")),
            filesystem_path(root.path())
        );
    }

    #[tokio::test]
    async fn worker_storage_turn_hold_preserves_queued_input_and_clears_only_its_reason() {
        use studio_runtime_store::{
            AgentId, AgentRecord, AgentRepository, QueueEventId, QueueEventRepository,
            RuntimeStore, StoreError,
        };
        let root = tempfile::tempdir().expect("state root");
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        let cwd = root.path().to_path_buf();
        store
            .transaction_with_error(move |tx| {
                let agent =
                    AgentRecord::from_value(json!({"id":"worker", "rootId":"worker", "epoch":1,
                    "status":"queued", "role":"implementer", "autoWake":true, "cwd":cwd}))?;
                AgentRepository::put(tx, &agent)?;
                crate::event_write::enqueue_event(
                    tx,
                    &agent,
                    "original-input".into(),
                    "followup",
                    "build the change".into(),
                    10.0,
                )
                .expect("queued input");
                Ok::<_, StoreError>(())
            })
            .await
            .expect("agent");
        let before = store
            .read_async("test/storage-queue-before", |tx| {
                QueueEventRepository::get(tx, &QueueEventId::from("original-input"))
            })
            .await
            .expect("queued identity")
            .expect("pending input");
        let free = Arc::new(AtomicU64::new(WORKER_STORAGE_MIN_FREE_BYTES - 1));
        let sample = Arc::clone(&free);
        let gate = DiskGate::with_sampler(
            root.path().to_path_buf(),
            Arc::new(move |_| Some(sample.load(Ordering::SeqCst))),
        );
        assert!(
            hold_agent_turn_with_gate(&store, "worker", 1, Some(&gate))
                .await
                .expect("held")
        );
        let agent = store
            .read_async("test/storage-hold", |tx| {
                AgentRepository::get(tx, &AgentId::from("worker"))
            })
            .await
            .expect("read")
            .expect("worker");
        assert_eq!(agent.status, studio_runtime_store::AgentStatus::Queued);
        assert_eq!(agent.epoch, 1);
        assert_eq!(agent.extra["startWait"], json!({"reason":"disk","epoch":1}));
        assert!(
            agent.extra["error"]
                .as_str()
                .expect("visible reason")
                .starts_with(STORAGE_HOLD_PREFIX)
        );
        // One sampler serves the state volume too, and a full state volume keeps
        // its sample and latch until its recheck; a new gate sees the freed disk.
        free.store(DISK_FULL_EXIT_FREE_BYTES, Ordering::SeqCst);
        let sample = Arc::clone(&free);
        let gate = DiskGate::with_sampler(
            root.path().to_path_buf(),
            Arc::new(move |_| Some(sample.load(Ordering::SeqCst))),
        );
        assert!(
            !hold_agent_turn_with_gate(&store, "worker", 1, Some(&gate))
                .await
                .expect("resumed")
        );
        let agent = store
            .read_async("test/storage-recovered", |tx| {
                AgentRepository::get(tx, &AgentId::from("worker"))
            })
            .await
            .expect("read")
            .expect("worker");
        assert!(!agent.extra.contains_key("error"));
        assert!(!agent.extra.contains_key("startWait"));
        assert_eq!(agent.status, studio_runtime_store::AgentStatus::Queued);
        let after = store
            .read_async("test/storage-queue-after", |tx| {
                QueueEventRepository::get(tx, &QueueEventId::from("original-input"))
            })
            .await
            .expect("queued identity after hold and recovery")
            .expect("pending input");
        assert_eq!(
            before, after,
            "input identity and pending state do not change"
        );
    }

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
    fn desktop_status_ignores_healthy_free_byte_changes() {
        let free = Arc::new(AtomicU64::new(DISK_SPACE_WARNING_FREE_BYTES));
        let gate = gate(&free);
        let first = serde_json::to_vec(&gate.status()).expect("desktop disk status");
        free.store(DISK_SPACE_WARNING_FREE_BYTES + 1, Ordering::SeqCst);
        // Expire only the injected sampler's cache; no wall-clock delay or disk writes.
        gate.lock().sampled_at = None;
        let second = serde_json::to_vec(&gate.status()).expect("desktop disk status");
        assert_eq!(
            first, second,
            "healthy disk writes must not change served identity"
        );
        assert_eq!(first, br#"{"level":null,"message":null}"#);
    }

    #[test]
    fn status_and_messages_match_the_python_owner() {
        let free = Arc::new(AtomicU64::new(130 * MIB));
        assert_eq!(
            gate(&free).status(),
            DiskSpaceStatus {
                level: Some(DiskSpaceLevel::Full),
                message: Some("Disk is full: 130 MiB free. Studio starts no new turns and resumes held agents when 2 GiB is free. Free disk space now.".into())
            }
        );
        assert_eq!(
            held_reason(Some(130 * MIB)),
            "Disk is full: 130 MiB free. Studio resumes this agent when free space returns."
        );
        assert_eq!(
            status_value(Some(3 * GIB), false).message.as_deref(),
            Some(
                "The disk has only 3.0 GB free. VMs, Docker and Studio can fail to write. Free disk space now."
            )
        );
        assert_eq!(
            status_value(Some(100 * GIB), false),
            DiskSpaceStatus::default()
        );
        assert_eq!(status_value(None, false), DiskSpaceStatus::default());
        assert_eq!(
            level(DISK_SPACE_WARNING_FREE_BYTES - 1, false),
            Some(DiskSpaceLevel::Warning)
        );
    }
}
