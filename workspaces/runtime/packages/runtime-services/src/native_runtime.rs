//! Native Codex executable lifecycle over Python-compatible files.
//!
//! Python behavior ported from `codex_native_runtime.py` lines 74-150, 201-238,
//! 606-690 and `codex_native_binary.py` lines 177-264, 480-548.  Discovery,
//! compatibility execution, publication, selection and release are explicit
//! ports so tests never run a real provider.  Session leases are process-local;
//! production callers must hold one for the full provider-session lifetime.
//! Windows npm shim expansion remains the named unsupported case inherited
//! from `provider_binaries`.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::provider_binaries::{
    BinaryCandidate, CandidateStatus, DiscoveryInputs, NativePlatform, binary_file_identity,
    bundle_digest, companion_identities, discovery_paths, order_candidates, parse_native_version,
};

mod state;
pub use state::{NativeRuntimeState, read_state};

const STATUS_RELATIVE_PATH: &str = "native-runtime/status.json";
const BUILDS_RELATIVE_PATH: &str = "native-runtime/builds";
const APPROVAL_REVISION: u64 = 2;

static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum NativeRuntimeError {
    #[error("native runtime I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("native runtime state is invalid")]
    InvalidState,
    #[error("native executable candidate is invalid")]
    InvalidCandidate,
    #[error("native executable compatibility check failed")]
    Incompatible,
    #[error("native executable version is invalid")]
    InvalidVersion,
    #[error("native bundle identity is invalid")]
    InvalidBundle,
    #[error("native bundle is currently used by a provider session")]
    InUse,
    #[error("native runtime durable step failed: {0}")]
    FailPoint(&'static str),
    #[error("native runtime state lock was poisoned")]
    Poisoned,
    #[error("native runtime JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    StateRead(#[from] state::StateReadError),
}

/// Fail points surround every durable record write and rename. Production uses
/// `NoFailPoints`; tests inject a one-shot failure and reopen the state root.
pub trait FailPoints: Send + Sync {
    fn before(&self, step: DurableStep) -> Result<(), NativeRuntimeError>;
    fn after(&self, step: DurableStep) -> Result<(), NativeRuntimeError>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DurableStep {
    BundleStagingWrite,
    StatusRecordWrite,
    StatusRecordRename,
    BundleDirectoryRename,
    RetiredBundleRename,
    RetiredBundleRemoval,
}

#[derive(Default)]
pub struct NoFailPoints;

impl FailPoints for NoFailPoints {
    fn before(&self, _step: DurableStep) -> Result<(), NativeRuntimeError> {
        Ok(())
    }

    fn after(&self, _step: DurableStep) -> Result<(), NativeRuntimeError> {
        Ok(())
    }
}

/// Effect port for the only external operation required during approval.
/// Implementations may run a candidate in a bounded, credential-free home.
/// Neither method is called inside a store transaction.
pub trait CompatibilityProbe: Send + Sync {
    fn version(&self, executable: &Path) -> Result<String, NativeRuntimeError>;
    fn check(&self, bundle: &Path, platform: NativePlatform) -> Result<Value, NativeRuntimeError>;
}

/// Injectable time source for deterministic persisted timestamps.
pub trait Clock: Send + Sync {
    fn unix_seconds(&self) -> Result<f64, NativeRuntimeError>;
}

#[derive(Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn unix_seconds(&self) -> Result<f64, NativeRuntimeError> {
        Ok(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| NativeRuntimeError::InvalidState)?
            .as_secs_f64())
    }
}

struct Inner {
    root: PathBuf,
    probe: Arc<dyn CompatibilityProbe>,
    clock: Arc<dyn Clock>,
    fail_points: Arc<dyn FailPoints>,
    state: Mutex<()>,
    leases: Mutex<BTreeMap<String, usize>>,
}

/// Native executable discovery, approval, retention, switching and release.
#[derive(Clone)]
pub struct NativeRuntime {
    inner: Arc<Inner>,
}

impl NativeRuntime {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>, probe: Arc<dyn CompatibilityProbe>) -> Self {
        Self::with_ports(root, probe, Arc::new(SystemClock), Arc::new(NoFailPoints))
    }

    #[must_use]
    pub fn with_fail_points(
        root: impl Into<PathBuf>,
        probe: Arc<dyn CompatibilityProbe>,
        fail_points: Arc<dyn FailPoints>,
    ) -> Self {
        Self::with_ports(root, probe, Arc::new(SystemClock), fail_points)
    }

    #[must_use]
    pub fn with_ports(
        root: impl Into<PathBuf>,
        probe: Arc<dyn CompatibilityProbe>,
        clock: Arc<dyn Clock>,
        fail_points: Arc<dyn FailPoints>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                root: root.into(),
                probe,
                clock,
                fail_points,
                state: Mutex::new(()),
                leases: Mutex::new(BTreeMap::new()),
            }),
        }
    }

    #[must_use]
    pub fn status_path(&self) -> PathBuf {
        self.inner.root.join(STATUS_RELATIVE_PATH)
    }

    /// Discover candidates in the existing Python precedence order. Version
    /// execution is injected and invalid candidates are retained as rejections.
    pub fn discover(
        &self,
        inputs: DiscoveryInputs,
    ) -> Result<Vec<BinaryCandidate>, NativeRuntimeError> {
        let mut candidates = Vec::new();
        for path in discovery_paths(inputs) {
            let candidate = match binary_file_identity(&path) {
                Ok(identity) => match self.inner.probe.version(&identity.path) {
                    Ok(version) if parse_native_version(&version).is_some() => BinaryCandidate {
                        path: identity.path,
                        status: CandidateStatus::Discovered,
                        version: Some(version),
                        error: None,
                    },
                    Ok(_) => rejected(path, "invalid native version"),
                    Err(_) => rejected(path, "native version probe failed"),
                },
                Err(_) => rejected(path, "candidate is not an executable file"),
            };
            candidates.push(candidate);
        }
        order_candidates(&mut candidates);
        Ok(candidates)
    }

    /// Copy, validate and atomically publish a complete immutable bundle.
    /// Retrying after a lost result is safe: an existing digest directory is
    /// verified and reused, never overwritten or repaired in place.
    pub fn approve(
        &self,
        candidate: &BinaryCandidate,
        platform: NativePlatform,
    ) -> Result<Value, NativeRuntimeError> {
        if candidate.error.is_some() || candidate.status == CandidateStatus::Rejected {
            return Err(NativeRuntimeError::InvalidCandidate);
        }
        let source = binary_file_identity(&candidate.path)
            .map_err(|_| NativeRuntimeError::InvalidCandidate)?;
        let companions = companion_identities(&source.path, platform)
            .map_err(|_| NativeRuntimeError::InvalidCandidate)?;
        let root = self.inner.root.canonicalize().or_else(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                fs::create_dir_all(&self.inner.root)?;
                self.inner.root.canonicalize()
            } else {
                Err(error)
            }
        })?;
        let builds = root.join(BUILDS_RELATIVE_PATH);
        fs::create_dir_all(&builds)?;
        let staging = unique_path(&builds, ".validate-");
        fs::create_dir(&staging)?;
        let staged_bundle = staging.join("bundle");
        let result = (|| {
            fs::create_dir(&staged_bundle)?;
            self.inner
                .fail_points
                .before(DurableStep::BundleStagingWrite)?;
            let executable_name = platform.executable_name();
            let executable = staged_bundle.join(executable_name);
            fs::copy(&source.path, &executable)?;
            for (name, identity) in &companions {
                fs::copy(&identity.path, staged_bundle.join(name))?;
            }
            sync_bundle(&staged_bundle)?;
            self.inner
                .fail_points
                .after(DurableStep::BundleStagingWrite)?;
            if binary_file_identity(&source.path)
                .map_err(|_| NativeRuntimeError::InvalidCandidate)?
                != source
                || companion_identities(&source.path, platform)
                    .map_err(|_| NativeRuntimeError::InvalidCandidate)?
                    != companions
            {
                return Err(NativeRuntimeError::InvalidCandidate);
            }

            let names = std::iter::once(executable_name.to_owned())
                .chain(companions.iter().map(|(name, _)| name.clone()))
                .collect::<Vec<_>>();
            let hashes = hash_bundle(&staged_bundle, &names)?;
            let binary_hash = hashes
                .iter()
                .find_map(|(name, digest)| (name == executable_name).then_some(digest.clone()))
                .ok_or(NativeRuntimeError::InvalidBundle)?;
            let bundle_hash = bundle_digest(&hashes)?;
            let version = self.inner.probe.version(&executable)?;
            if parse_native_version(&version).is_none() {
                return Err(NativeRuntimeError::InvalidVersion);
            }
            let checks = self.inner.probe.check(&staged_bundle, platform)?;
            if checks.as_object().is_none_or(Map::is_empty) {
                return Err(NativeRuntimeError::Incompatible);
            }
            if hash_bundle(&staged_bundle, &names)? != hashes {
                return Err(NativeRuntimeError::InvalidCandidate);
            }

            let target = builds.join(&bundle_hash);
            if target.exists() {
                verify_bundle(&target, &names, &hashes, platform)?;
            } else {
                self.inner
                    .fail_points
                    .before(DurableStep::BundleDirectoryRename)?;
                match fs::rename(&staged_bundle, &target) {
                    Ok(()) => self
                        .inner
                        .fail_points
                        .after(DurableStep::BundleDirectoryRename)?,
                    Err(_error) if target.exists() => {
                        verify_bundle(&target, &names, &hashes, platform)?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            let approved_path = target.join(executable_name);
            let record = approval_record(ApprovalRecordInput {
                path: &approved_path,
                source_identity: &source,
                version: &version,
                executable_hash: binary_hash,
                bundle_hash: &bundle_hash,
                checks,
                hashes: &hashes,
                companions: &companions,
                validated_at: self.inner.clock.unix_seconds()?,
            })?;
            Ok(record)
        })();
        let _cleanup = fs::remove_dir_all(&staging);
        result
    }

    /// Scan candidates, approve the newest compatible bundle and persist the
    /// Python status record once. Rejected candidates remain visible.
    pub fn check(
        &self,
        inputs: DiscoveryInputs,
        platform: NativePlatform,
    ) -> Result<NativeRuntimeState, NativeRuntimeError> {
        let _state = self.lock_state()?;
        let mut state = self.load_or_default()?;
        let mut selected = state
            .get("selected")
            .filter(|record| validate_approved_record(record, &self.inner.root, platform).is_ok())
            .cloned();
        let existing = selected.as_ref().and_then(binary_candidate);
        let discovered = self.discover(inputs)?;
        let mut candidate_rows = Vec::new();
        for candidate in discovered {
            let source_identity = binary_file_identity(&candidate.path).ok();
            let source_companions = source_identity
                .as_ref()
                .and_then(|identity| companion_identities(&identity.path, platform).ok());
            let Some(version) = candidate.version.as_deref() else {
                let mut row = json!({
                    "path": candidate.path,
                    "status": "rejected",
                    "error": candidate.error.unwrap_or_else(|| "candidate was rejected".to_owned()),
                });
                add_candidate_identity(
                    &mut row,
                    source_identity.as_ref().map(file_identity_json),
                    source_companions.as_deref().map(companion_identities_json),
                );
                candidate_rows.push(row);
                continue;
            };
            match self.approve(&candidate, platform) {
                Ok(record) => {
                    let approved = BinaryCandidate {
                        path: PathBuf::from(
                            record["path"]
                                .as_str()
                                .ok_or(NativeRuntimeError::InvalidState)?,
                        ),
                        status: CandidateStatus::Approved,
                        version: record["version"].as_str().map(str::to_owned),
                        error: None,
                    };
                    let mut row = json!({
                        "path": candidate.path,
                        "status": "approved",
                        "version": version,
                    });
                    add_candidate_identity(
                        &mut row,
                        record.get("sourceIdentity").cloned(),
                        record
                            .get("sourceIdentity")
                            .and_then(|identity| identity.get("companions"))
                            .cloned(),
                    );
                    candidate_rows.push(row);
                    if crate::provider_binaries::select_approved_candidate(
                        existing.as_ref(),
                        std::slice::from_ref(&approved),
                    )
                    .is_some_and(|chosen| chosen.path == approved.path)
                    {
                        selected = Some(record);
                    }
                    break;
                }
                Err(_) => {
                    let mut row = json!({
                        "path": candidate.path,
                        "status": "rejected",
                        "error": "native compatibility approval failed",
                    });
                    add_candidate_identity(
                        &mut row,
                        source_identity.as_ref().map(file_identity_json),
                        source_companions.as_deref().map(companion_identities_json),
                    );
                    candidate_rows.push(row);
                }
            }
        }
        state["status"] = Value::String(
            if selected.is_some() {
                "ready"
            } else {
                "failed"
            }
            .to_owned(),
        );
        state["checkedAt"] = json!(self.inner.clock.unix_seconds()?);
        state["selected"] = selected.clone().unwrap_or(Value::Null);
        state["candidates"] = Value::Array(candidate_rows);
        if selected.is_none() {
            state["error"] =
                Value::String("No installed Codex version passed the protocol checks".to_owned());
        } else if let Some(object) = state.as_object_mut() {
            object.remove("error");
        }
        self.write_state(&state)?;
        Ok(read_state(&self.status_path())?)
    }

    /// Read the Python-written state record without discarding accounts,
    /// rotation receipts, rejected candidates, or unknown extension fields.
    pub fn read_state(&self) -> Result<Option<NativeRuntimeState>, NativeRuntimeError> {
        let path = self.status_path();
        if !path.exists() {
            return Ok(None);
        }
        Ok(read_state(&path).map(Some)?)
    }

    /// Atomically switch the selected executable. Active session leases remain
    /// attached to their exact old bundle until their guards are dropped.
    pub fn switch(&self, approved: &Value) -> Result<(), NativeRuntimeError> {
        let _state = self.lock_state()?;
        validate_approved_record(approved, &self.inner.root, current_platform()?)?;
        let mut state = self.load_or_default()?;
        state["selected"] = approved.clone();
        state["status"] = Value::String("ready".to_owned());
        self.write_state(&state)
    }

    /// Retain the selected executable for one provider-session lifetime.
    pub fn retain_selected(&self) -> Result<NativeExecutableLease, NativeRuntimeError> {
        let _state = self.lock_state()?;
        let state = self.load_or_default()?;
        let selected = state
            .get("selected")
            .and_then(Value::as_object)
            .ok_or(NativeRuntimeError::InvalidState)?;
        let bundle = selected
            .get("bundleSha256")
            .and_then(Value::as_str)
            .filter(|hash| is_sha256(hash))
            .ok_or(NativeRuntimeError::InvalidState)?
            .to_owned();
        let path = selected
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or(NativeRuntimeError::InvalidState)?;
        let expected = self
            .inner
            .root
            .canonicalize()?
            .join(BUILDS_RELATIVE_PATH)
            .join(&bundle)
            .join(current_platform()?.executable_name());
        if path != expected {
            return Err(NativeRuntimeError::InvalidState);
        }
        let parent = expected.parent().ok_or(NativeRuntimeError::InvalidState)?;
        if parent.is_symlink() || !parent.is_dir() {
            return Err(NativeRuntimeError::InvalidBundle);
        }
        let platform = current_platform()?;
        let names = bundle_names_for_directory(parent, platform)?;
        let hashes = hash_bundle(parent, &names)?;
        validate_approved_record(&Value::Object(selected.clone()), &self.inner.root, platform)?;
        if bundle_digest(&hashes)? != bundle {
            return Err(NativeRuntimeError::InvalidBundle);
        }
        let mut leases = self.lock_leases()?;
        let count = leases.entry(bundle.clone()).or_default();
        *count = count
            .checked_add(1)
            .ok_or(NativeRuntimeError::InvalidState)?;
        drop(leases);
        Ok(NativeExecutableLease {
            inner: Arc::clone(&self.inner),
            bundle,
            path,
        })
    }

    /// Release one approved bundle. The rename hides it atomically before
    /// recursive cleanup; a concurrent session is serialized with this check.
    pub fn release(&self, bundle: &str) -> Result<bool, NativeRuntimeError> {
        if !is_sha256(bundle) {
            return Err(NativeRuntimeError::InvalidBundle);
        }
        let _state = self.lock_state()?;
        let leases = self.lock_leases()?;
        if leases.get(bundle).copied().unwrap_or_default() > 0 {
            return Err(NativeRuntimeError::InUse);
        }
        let directory = self
            .inner
            .root
            .canonicalize()?
            .join(BUILDS_RELATIVE_PATH)
            .join(bundle);
        if !directory.exists() {
            return Ok(false);
        }
        let platform = current_platform()?;
        let names = bundle_names_for_directory(&directory, platform)?;
        let hashes = hash_bundle(&directory, &names)?;
        if bundle_digest(&hashes)? != bundle {
            return Err(NativeRuntimeError::InvalidBundle);
        }
        verify_bundle(&directory, &names, &hashes, platform)?;
        let state = self.load_or_default()?;
        if state
            .get("selected")
            .and_then(Value::as_object)
            .and_then(|selected| selected.get("bundleSha256"))
            .and_then(Value::as_str)
            == Some(bundle)
        {
            return Err(NativeRuntimeError::InUse);
        }
        let retired = unique_path(
            directory
                .parent()
                .ok_or(NativeRuntimeError::InvalidBundle)?,
            ".retired-",
        );
        self.inner
            .fail_points
            .before(DurableStep::RetiredBundleRename)?;
        fs::rename(&directory, &retired)?;
        self.inner
            .fail_points
            .after(DurableStep::RetiredBundleRename)?;
        drop(leases);
        self.inner
            .fail_points
            .before(DurableStep::RetiredBundleRemoval)?;
        fs::remove_dir_all(retired)?;
        self.inner
            .fail_points
            .after(DurableStep::RetiredBundleRemoval)?;
        Ok(true)
    }

    /// Retry cleanup after a crash that happened after the atomic retirement
    /// rename. Retired directories are never visible as approved bundles.
    pub fn recover_retired(&self) -> Result<usize, NativeRuntimeError> {
        let builds = self.inner.root.join(BUILDS_RELATIVE_PATH);
        if !builds.exists() {
            return Ok(0);
        }
        let mut removed = 0;
        for entry in fs::read_dir(builds)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with(".retired-") {
                self.inner
                    .fail_points
                    .before(DurableStep::RetiredBundleRemoval)?;
                fs::remove_dir_all(entry.path())?;
                self.inner
                    .fail_points
                    .after(DurableStep::RetiredBundleRemoval)?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    fn load_or_default(&self) -> Result<Value, NativeRuntimeError> {
        let path = self.status_path();
        if path.exists() {
            let value: Value = serde_json::from_slice(&fs::read(path)?)?;
            if !value.is_object() {
                return Err(NativeRuntimeError::InvalidState);
            }
            return Ok(value);
        }
        Ok(json!({
            "status": "checking",
            "checkedAt": null,
            "selected": null,
            "candidates": [],
            "accounts": {}
        }))
    }

    fn write_state(&self, state: &Value) -> Result<(), NativeRuntimeError> {
        if !state.is_object() {
            return Err(NativeRuntimeError::InvalidState);
        }
        let path = self.status_path();
        let parent = path.parent().ok_or(NativeRuntimeError::InvalidState)?;
        fs::create_dir_all(parent)?;
        let temporary = unique_path(parent, ".status-");
        let result = (|| {
            self.inner
                .fail_points
                .before(DurableStep::StatusRecordWrite)?;
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&serde_json::to_vec(state)?)?;
            file.sync_all()?;
            self.inner
                .fail_points
                .after(DurableStep::StatusRecordWrite)?;
            drop(file);
            self.inner
                .fail_points
                .before(DurableStep::StatusRecordRename)?;
            fs::rename(&temporary, &path)?;
            self.inner
                .fail_points
                .after(DurableStep::StatusRecordRename)?;
            if let Ok(directory) = File::open(parent) {
                directory.sync_all()?;
            }
            Ok(())
        })();
        if result.is_err() {
            let _ignored = fs::remove_file(temporary);
        }
        result
    }

    fn lock_leases(&self) -> Result<MutexGuard<'_, BTreeMap<String, usize>>, NativeRuntimeError> {
        self.inner
            .leases
            .lock()
            .map_err(|_| NativeRuntimeError::Poisoned)
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, ()>, NativeRuntimeError> {
        self.inner
            .state
            .lock()
            .map_err(|_| NativeRuntimeError::Poisoned)
    }
}

/// RAII session ownership for one immutable bundle. A session constructor
/// acquires this before launching; release cannot unlink its executable until
/// the guard drops.
pub struct NativeExecutableLease {
    inner: Arc<Inner>,
    bundle: String,
    path: PathBuf,
}

impl NativeExecutableLease {
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub fn bundle_hash(&self) -> &str {
        &self.bundle
    }
}

impl Drop for NativeExecutableLease {
    fn drop(&mut self) {
        if let Ok(mut leases) = self.inner.leases.lock()
            && let Some(count) = leases.get_mut(&self.bundle)
        {
            *count -= 1;
            if *count == 0 {
                leases.remove(&self.bundle);
            }
        }
    }
}

fn rejected(path: PathBuf, reason: &'static str) -> BinaryCandidate {
    BinaryCandidate {
        path,
        status: CandidateStatus::Rejected,
        version: None,
        error: Some(reason.to_owned()),
    }
}

fn binary_candidate(record: &Value) -> Option<BinaryCandidate> {
    Some(BinaryCandidate {
        path: PathBuf::from(record.get("path")?.as_str()?),
        status: CandidateStatus::Approved,
        version: Some(record.get("version")?.as_str()?.to_owned()),
        error: None,
    })
}

fn add_candidate_identity(row: &mut Value, identity: Option<Value>, companions: Option<Value>) {
    if let Some(identity) = identity {
        row["identity"] = identity;
    }
    if let Some(companions) = companions {
        row["companionIdentity"] = companions;
    }
}

fn unique_path(parent: &Path, prefix: &str) -> PathBuf {
    let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    parent.join(format!("{prefix}{}-{sequence}", std::process::id()))
}

fn hash_file(path: &Path) -> Result<String, NativeRuntimeError> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(data_encoding::HEXLOWER.encode(&digest.finalize()))
}

fn hash_bundle(
    directory: &Path,
    names: &[String],
) -> Result<Vec<(String, String)>, NativeRuntimeError> {
    names
        .iter()
        .map(|name| Ok((name.clone(), hash_file(&directory.join(name))?)))
        .collect()
}

fn sync_bundle(directory: &Path) -> Result<(), NativeRuntimeError> {
    for entry in fs::read_dir(directory)? {
        File::open(entry?.path())?.sync_all()?;
    }
    File::open(directory)?.sync_all()?;
    Ok(())
}

fn verify_bundle(
    directory: &Path,
    names: &[String],
    expected: &[(String, String)],
    platform: NativePlatform,
) -> Result<(), NativeRuntimeError> {
    if directory.is_symlink() || !directory.is_dir() {
        return Err(NativeRuntimeError::InvalidBundle);
    }
    for name in names {
        let path = directory.join(name);
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(NativeRuntimeError::InvalidBundle);
        }
    }
    if hash_bundle(directory, names)? != expected {
        return Err(NativeRuntimeError::InvalidBundle);
    }
    binary_file_identity(&directory.join(platform.executable_name()))
        .map_err(|_| NativeRuntimeError::InvalidBundle)?;
    companion_identities(&directory.join(platform.executable_name()), platform)
        .map_err(|_| NativeRuntimeError::InvalidBundle)?;
    Ok(())
}

fn bundle_names_for_directory(
    directory: &Path,
    platform: NativePlatform,
) -> Result<Vec<String>, NativeRuntimeError> {
    let mut names = fs::read_dir(directory)?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(NativeRuntimeError::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    if !names.iter().any(|name| name == platform.executable_name()) || names.len() < 2 {
        return Err(NativeRuntimeError::InvalidBundle);
    }
    let expected = if platform.is_windows() {
        "codex-code-mode-host.exe"
    } else {
        "codex-code-mode-host"
    };
    if names.len() != 2 || !names.iter().any(|name| name == expected) {
        return Err(NativeRuntimeError::InvalidBundle);
    }
    Ok(names)
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn validate_approved_record(
    record: &Value,
    root: &Path,
    platform: NativePlatform,
) -> Result<(), NativeRuntimeError> {
    let selected = record.as_object().ok_or(NativeRuntimeError::InvalidState)?;
    let bundle = selected
        .get("bundleSha256")
        .and_then(Value::as_str)
        .filter(|value| is_sha256(value))
        .ok_or(NativeRuntimeError::InvalidBundle)?;
    if selected.get("approvalRevision").and_then(Value::as_u64) != Some(APPROVAL_REVISION)
        || selected
            .get("version")
            .and_then(Value::as_str)
            .and_then(parse_native_version)
            .is_none()
    {
        return Err(NativeRuntimeError::InvalidState);
    }
    let expected = root
        .canonicalize()?
        .join(BUILDS_RELATIVE_PATH)
        .join(bundle)
        .join(platform.executable_name());
    if selected.get("path").and_then(Value::as_str) != expected.to_str() {
        return Err(NativeRuntimeError::InvalidBundle);
    }
    let directory = expected.parent().ok_or(NativeRuntimeError::InvalidBundle)?;
    let names = bundle_names_for_directory(directory, platform)?;
    let hashes = hash_bundle(directory, &names)?;
    let executable_hash = hashes
        .iter()
        .find_map(|(name, hash)| (name == platform.executable_name()).then_some(hash.as_str()))
        .ok_or(NativeRuntimeError::InvalidBundle)?;
    let approved_companions = selected
        .get("companions")
        .and_then(Value::as_object)
        .ok_or(NativeRuntimeError::InvalidBundle)?;
    for name in names
        .iter()
        .filter(|name| name.as_str() != platform.executable_name())
    {
        let expected_path = expected.with_file_name(name);
        let expected_hash = hashes
            .iter()
            .find_map(|(candidate, hash)| (candidate == name).then_some(hash.as_str()))
            .ok_or(NativeRuntimeError::InvalidBundle)?;
        let companion = approved_companions
            .get(name)
            .and_then(Value::as_object)
            .ok_or(NativeRuntimeError::InvalidBundle)?;
        if companion.get("path").and_then(Value::as_str) != expected_path.to_str()
            || companion.get("sha256").and_then(Value::as_str) != Some(expected_hash)
        {
            return Err(NativeRuntimeError::InvalidBundle);
        }
    }
    if bundle_digest(&hashes)? != bundle
        || selected.get("sha256").and_then(Value::as_str) != Some(executable_hash)
        || selected
            .get("checks")
            .and_then(Value::as_object)
            .is_none_or(Map::is_empty)
    {
        return Err(NativeRuntimeError::InvalidBundle);
    }
    let executable =
        binary_file_identity(&expected).map_err(|_| NativeRuntimeError::InvalidBundle)?;
    if executable.path != expected {
        return Err(NativeRuntimeError::InvalidBundle);
    }
    Ok(())
}

fn current_platform() -> Result<NativePlatform, NativeRuntimeError> {
    let architecture = std::env::consts::ARCH;
    match (std::env::consts::OS, architecture) {
        ("linux", "x86_64") => Ok(NativePlatform::LinuxX64),
        ("linux", "aarch64") => Ok(NativePlatform::LinuxArm64),
        ("macos", "x86_64") => Ok(NativePlatform::MacX64),
        ("macos", "aarch64") => Ok(NativePlatform::MacArm64),
        ("windows", "x86_64") => Ok(NativePlatform::WindowsX64),
        ("windows", "aarch64") => Ok(NativePlatform::WindowsArm64),
        _ => Err(NativeRuntimeError::InvalidCandidate),
    }
}

fn file_identity_json(identity: &crate::provider_binaries::BinaryFileIdentity) -> Value {
    json!({
        "path": identity.path,
        "size": identity.size,
        "mtimeNs": identity.modified_ns,
        "device": identity.device,
        "inode": identity.inode,
    })
}

fn companion_identities_json(
    companions: &[(String, crate::provider_binaries::BinaryFileIdentity)],
) -> Value {
    let mut result = Map::new();
    for (name, identity) in companions {
        result.insert(name.clone(), file_identity_json(identity));
    }
    Value::Object(result)
}

struct ApprovalRecordInput<'a> {
    path: &'a Path,
    source_identity: &'a crate::provider_binaries::BinaryFileIdentity,
    version: &'a str,
    executable_hash: String,
    bundle_hash: &'a str,
    checks: Value,
    hashes: &'a [(String, String)],
    companions: &'a [(String, crate::provider_binaries::BinaryFileIdentity)],
    validated_at: f64,
}

fn approval_record(input: ApprovalRecordInput<'_>) -> Result<Value, NativeRuntimeError> {
    let companion_records = input
        .companions
        .iter()
        .map(|(name, identity)| {
            let hash = input
                .hashes
                .iter()
                .find_map(|(candidate, hash)| (candidate == name).then_some(hash))
                .ok_or(NativeRuntimeError::InvalidBundle)?;
            Ok((
                name.clone(),
                json!({
                    "path": input.path.with_file_name(name),
                    "sourcePath": identity.path,
                    "sha256": hash,
                    "sourceIdentity": file_identity_json(identity),
                }),
            ))
        })
        .collect::<Result<Map<String, Value>, NativeRuntimeError>>()?;
    let mut source_identity_json = file_identity_json(input.source_identity);
    source_identity_json["companions"] = companion_identities_json(input.companions);
    Ok(json!({
        "path": input.path,
        "sourcePath": input.source_identity.path,
        "version": input.version,
        "sha256": input.executable_hash,
        "validatedAt": input.validated_at,
        "checks": input.checks,
        "bundleSha256": input.bundle_hash,
        "companions": companion_records,
        "approvalRevision": APPROVAL_REVISION,
        "sourceIdentity": source_identity_json,
    }))
}

#[cfg(test)]
mod tests;
