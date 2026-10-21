//! Image workspace lifecycle ported from `codex_workspace_images.py`.
//!
//! Non-macOS callers get Python's exact ASIF platform refusal before reading or
//! writing workspace state. The lifecycle depends on `WorkspaceBackend`; tests
//! use a fake and production selects the cfg-gated ASIF backend.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::Condvar;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PLATFORM_REFUSAL: &str =
    "Image workspaces require macOS ASIF. Use a Git worktree on this server";

/// Whether this host runs the turns of image-workspace agents. On macOS, as in
/// Python (`codex_workspace_macos.py` `exec_prefix` is empty), a turn attaches
/// the agent's image and runs in `<mount>/repo` with the account's shared
/// provider; workspace preparation (`workspace_ready_git`) mounts it before
/// every turn. Other hosts refuse image workspaces, as Python's backend does.
pub const IMAGE_EXECUTION_SUPPORTED: bool = cfg!(target_os = "macos");

/// `imageWorkspacePhase` of an agent whose implicit image request fell back to a
/// Git worktree or its folder (Python `complete_image_workspace`). Such an agent
/// is no longer an image agent.
pub const FALLBACK_PHASE: &str = "fallback";

/// Whether an `imageWorkspacePhase` value still binds the agent to an image.
#[must_use]
pub fn image_phase(phase: Option<&str>) -> bool {
    phase.is_some_and(|phase| phase != FALLBACK_PHASE)
}

/// An agent whose workspace is an image, in any record shape Python wrote:
/// the image fields of older records or the explicit `workspaceMode`.
#[must_use]
pub fn is_image_workspace_agent(record: &Value) -> bool {
    record["imageWorkspace"] == true
        || record["imageWorkspaceReady"] == true
        || image_phase(record["imageWorkspacePhase"].as_str())
        || record["workspaceMode"] == "image"
}

/// Whether restart recovery may give this agent a follow-up turn. A host that
/// cannot run image turns gives an image agent none: the turn would run
/// outside its image. The agent stays interrupted with the restart error, and
/// its next input is held with `workspace-state/image-execution`.
#[must_use]
pub fn restart_follow_up_allowed(record: &Value, image_execution_supported: bool) -> bool {
    image_execution_supported || !is_image_workspace_agent(record)
}

#[cfg(test)]
mod image_execution_tests {
    use super::{is_image_workspace_agent, restart_follow_up_allowed};
    use serde_json::json;

    #[test]
    fn an_image_agent_gets_a_restart_follow_up_only_where_image_turns_run() {
        let explicit = json!({"workspaceMode":"image"});
        let older =
            json!({"imageWorkspace":true,"imageWorkspaceReady":true,"imageWorkspacePhase":"ready"});
        let worktree = json!({"workspaceMode":"worktree"});
        assert!(is_image_workspace_agent(&explicit));
        assert!(is_image_workspace_agent(&older));
        assert!(!is_image_workspace_agent(&worktree));
        for image in [&explicit, &older] {
            assert!(restart_follow_up_allowed(image, true));
            assert!(!restart_follow_up_allowed(image, false));
        }
        assert!(restart_follow_up_allowed(&worktree, false));
        let fallback = json!({"imageWorkspace":false,"imageWorkspacePhase":"fallback","workspaceMode":"worktree"});
        assert!(!is_image_workspace_agent(&fallback));
    }
}
#[cfg(target_os = "macos")]
const STORE_ENV: &str = "CODEX_WORKSPACE_STORE";
const BASE_MIN_FREE_ENV: &str = "CODEX_WORKSPACE_MIN_FREE_BYTES";
const AGENT_MIN_FREE_ENV: &str = "CODEX_WORKSPACE_AGENT_MIN_FREE_BYTES";
const DEFAULT_BASE_MIN_FREE: u64 = 20 * 1024 * 1024 * 1024;
const DEFAULT_AGENT_MIN_FREE: u64 = 5 * 1024 * 1024 * 1024;
const STATE_SCHEMA: u64 = 2;
const BASE_BUILD_COOLDOWN: Duration = Duration::from_secs(300);
const BASE_BUILD_WAIT: Duration = Duration::from_secs(900);
const BASE_BUILD_POLL: Duration = Duration::from_millis(50);
struct BuildWorkers {
    active: Mutex<BTreeSet<String>>,
    #[cfg(test)]
    changed: Condvar,
}

static BASE_BUILD_WORKERS: OnceLock<BuildWorkers> = OnceLock::new();

#[cfg(test)]
pub(crate) fn fsevents_history_lost(events: &[(u64, u32)]) -> bool {
    events
        .iter()
        .any(|(_, flags)| flags & (0x2 | 0x4 | 0x8) != 0)
}

fn build_workers() -> &'static BuildWorkers {
    BASE_BUILD_WORKERS.get_or_init(|| BuildWorkers {
        active: Mutex::new(BTreeSet::new()),
        #[cfg(test)]
        changed: Condvar::new(),
    })
}

fn reserve_build_worker(key: &str) {
    if let Ok(mut workers) = build_workers().active.lock() {
        workers.insert(key.to_owned());
    }
}

fn release_build_worker(key: &str) {
    if let Ok(mut workers) = build_workers().active.lock() {
        workers.remove(key);
        #[cfg(test)]
        build_workers().changed.notify_all();
    }
}

fn build_worker_active(key: &str) -> bool {
    build_workers()
        .active
        .lock()
        .is_ok_and(|workers| workers.contains(key))
}

#[cfg(test)]
fn wait_for_build_worker(key: &str, timeout: Duration) -> bool {
    let workers = build_workers();
    let Ok(mut active) = workers.active.lock() else {
        return false;
    };
    let deadline = std::time::Instant::now() + timeout;
    while active.contains(key) {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let Ok((next, result)) = workers.changed.wait_timeout(active, remaining) else {
            return false;
        };
        active = next;
        if result.timed_out() && active.contains(key) {
            return false;
        }
    }
    true
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceBaseStaging {
    pub root: PathBuf,
    pub token: Option<Value>,
    pub version_path: PathBuf,
    pub image: Option<PathBuf>,
    pub mount: Option<PathBuf>,
    pub version: String,
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceMount {
    pub mount: String,
    pub layer: Option<String>,
    pub base_image: Option<String>,
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDelta {
    pub token: Option<Value>,
    pub changed_paths: Vec<String>,
    pub history_lost: bool,
    pub scan_paths: Vec<String>,
    pub refresh_base: bool,
}

/// Port of the methods in `codex_workspace_images.WorkspaceBackend` (lines 49-63).
pub trait WorkspaceBackend: Send + Sync {
    fn supported(&self, root: &Path) -> Result<(), ImageWorkspaceError>;
    fn current_event_id(&self, root: &Path) -> Result<Option<Value>, ImageWorkspaceError>;
    fn open_base_staging(
        &self,
        root: &Path,
        key: &str,
        version: &str,
    ) -> Result<WorkspaceBaseStaging, ImageWorkspaceError>;
    fn copy_base_tree(
        &self,
        root: &Path,
        destination: &Path,
        excludes: &[String],
        check: &dyn Fn() -> Result<(), ImageWorkspaceError>,
    ) -> Result<(), ImageWorkspaceError>;
    fn seal_base(&self, staging: &WorkspaceBaseStaging) -> Result<Value, ImageWorkspaceError>;
    fn clone_workspace(
        &self,
        image: &Path,
        agent_dir: &Path,
    ) -> Result<PathBuf, ImageWorkspaceError>;
    fn mount_workspace(
        &self,
        layer: &Path,
        mount: &Path,
        base_image: Option<&Path>,
    ) -> Result<WorkspaceMount, ImageWorkspaceError>;
    fn sync_delta(
        &self,
        root: &Path,
        target: &Path,
        token: Option<&Value>,
        excludes: &[String],
    ) -> Result<WorkspaceDelta, ImageWorkspaceError>;
    fn unmount_workspace(&self, mount: &Path, force: bool) -> Result<Value, ImageWorkspaceError>;
    /// Whether a volume is mounted at `path` (a mount point boundary, not its identity).
    fn is_mounted(&self, path: &Path) -> Result<bool, ImageWorkspaceError>;
    /// Every attached disk image with its mount points (`hdiutil info`).
    fn attachments(&self) -> Result<Vec<WorkspaceAttachment>, ImageWorkspaceError>;
    fn remove_layer(&self, agent_dir: &Path) -> Result<(), ImageWorkspaceError>;
    fn remove_base_version(&self, path: &Path) -> Result<(), ImageWorkspaceError>;
    fn private_bytes(&self, path: &Path) -> Result<Option<u64>, ImageWorkspaceError>;
    /// Available bytes on the host filesystem containing `path`.
    ///
    /// The production default preserves the host free-space preflight. Tests
    /// may override this boundary so lifecycle behavior does not depend on the
    /// machine running them.
    fn available_bytes(&self, path: &Path) -> Result<u64, ImageWorkspaceError> {
        let stats = rustix::fs::statvfs(path)
            .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
        Ok(stats.f_bavail.saturating_mul(stats.f_frsize))
    }
    fn exec_prefix(&self) -> Vec<String>;
    fn base_needs_refresh(
        &self,
        _root: &Path,
        _state: &Value,
    ) -> Result<bool, ImageWorkspaceError> {
        Ok(false)
    }
    fn open_base_refresh(
        &self,
        root: &Path,
        key: &str,
        version: &str,
        _old_state: &Value,
    ) -> Result<WorkspaceBaseStaging, ImageWorkspaceError> {
        self.open_base_staging(root, key, version)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImageWorkspaceError {
    #[error("Image workspaces require macOS ASIF. Use a Git worktree on this server")]
    UnsupportedPlatform,
    #[error("Invalid workspace identifier")]
    InvalidId,
    #[error("Image workspace base is not ready")]
    BaseNotReady,
    #[error("Workspace identifier is already used by another folder")]
    IdentityConflict,
    #[error("Not enough free disk space for {purpose}: {free} free, {required} required (bytes)")]
    FreeSpace {
        purpose: &'static str,
        free: u64,
        required: u64,
    },
    #[error(
        "Image workspace mount check failed for {agent_id}: {status}: {detail}. Studio did not use or delete this workspace"
    )]
    MountCheck {
        agent_id: String,
        status: &'static str,
        detail: String,
    },
    #[error("Workspace mount is unavailable: {0}")]
    MountUnavailable(PathBuf),
    #[error("Workspace backend failed: {0}")]
    Backend(String),
    #[error("Workspace state is invalid")]
    InvalidState,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub struct WorkspaceLifecycle<B> {
    store: PathBuf,
    backend: Arc<B>,
}

impl<B: WorkspaceBackend + 'static> WorkspaceLifecycle<B> {
    /// Testable lifecycle constructor; callers provide an isolated store and backend.
    pub fn with_backend(store: impl Into<PathBuf>, backend: B) -> Self {
        Self {
            store: store.into(),
            backend: Arc::new(backend),
        }
    }

    pub fn supported(&self, root: &Path) -> Result<(), ImageWorkspaceError> {
        let root = root_path(root)?;
        self.backend.supported(&root)
    }

    pub fn base_status(&self, root: &Path) -> Result<Value, ImageWorkspaceError> {
        let root = root_path(root)?;
        let key = repo_key(&root);
        let state = read_json(&self.base_state(&key))?;
        if state.get("schema").and_then(Value::as_u64) != Some(STATE_SCHEMA)
            || state.get("changeDetector").and_then(Value::as_str) != Some("git-v1")
        {
            return Ok(json!({"state":"missing","version":null,"error":null}));
        }
        // Python's status lookup treats a build owned by another process as
        // missing. That lets provisioning recover after a server restart
        // instead of trusting a durable `building` row whose worker is gone.
        if state.get("state").and_then(Value::as_str) == Some("building")
            && state.get("builderPid").and_then(Value::as_u64) != Some(std::process::id() as u64)
        {
            return Ok(json!({"state":"missing","version":null,"error":null}));
        }
        Ok(json!({
            "state":state.get("state").cloned().unwrap_or(Value::Null),
            "version":state.get("version").cloned().unwrap_or(Value::Null),
            "error":state.get("error").cloned().unwrap_or(Value::Null),
        }))
    }

    pub fn start_base_build(&self, root: &Path) -> Result<Value, ImageWorkspaceError> {
        self.start_base_build_retry(root, false)
    }

    pub fn start_base_build_retry(
        &self,
        root: &Path,
        retry_failed: bool,
    ) -> Result<Value, ImageWorkspaceError> {
        let root = root_path(root)?;
        self.backend.supported(&root)?;
        let key = repo_key(&root);
        let Some(lock) = FileLock::try_exclusive(&self.base_dir(&key).join(".build.lock"))? else {
            return Ok(status_view(&read_json(&self.base_state(&key))?));
        };
        let state_path = self.base_state(&key);
        let current = read_json(&state_path)?;
        if current.get("state").and_then(Value::as_str) == Some("ready")
            && current
                .get("image")
                .and_then(Value::as_str)
                .is_some_and(|p| Path::new(p).exists())
        {
            if !self.backend.base_needs_refresh(&root, &current)?
                || current.get("refreshingVersion").is_some()
            {
                return Ok(status_view(&current));
            }
            let version = format!("v-{}", now_ns());
            let mut refreshing = current.clone();
            refreshing["refreshingVersion"] = json!(version);
            write_json(&state_path, &refreshing)?;
            reserve_build_worker(&key);
            drop(lock);
            if let Err(error) = self.spawn_base_build(root, key.clone(), version, Some(current)) {
                release_build_worker(&key);
                let mut failed = read_json(&state_path)?;
                failed
                    .as_object_mut()
                    .map(|object| object.remove("refreshingVersion"));
                failed["refreshError"] = json!(error.to_string());
                failed["refreshFailedAt"] = json!(now_seconds());
                write_json(&state_path, &failed)?;
                return Err(error);
            }
            return Ok(status_view(&refreshing));
        }
        if current.get("state").and_then(Value::as_str) == Some("building") {
            let owner_pid = current.get("builderPid").and_then(Value::as_u64);
            if owner_pid == Some(u64::from(std::process::id())) && build_worker_active(&key) {
                return Ok(status_view(&current));
            }
        }
        if current.get("state").and_then(Value::as_str) == Some("failed") && !retry_failed {
            let failed_at = current
                .get("failedAt")
                .and_then(Value::as_f64)
                .unwrap_or_default();
            if now_seconds() - failed_at < BASE_BUILD_COOLDOWN.as_secs_f64() {
                return Ok(status_view(&current));
            }
        }
        let version = format!("v-{}", now_ns());
        self.require_space(DEFAULT_BASE_MIN_FREE, BASE_MIN_FREE_ENV, "image base", None)?;
        let pending = json!({
            "schema":STATE_SCHEMA,"state":"building","repoRoot":root,
            "repoKey":key,"changeDetector":"git-v1","version":version,
            "error":null,"startedAt":now_seconds(),"builderPid":std::process::id()
        });
        write_json(&state_path, &pending)?;
        reserve_build_worker(&key);
        drop(lock);
        if let Err(error) = self.spawn_base_build(root, key.clone(), version, None) {
            release_build_worker(&key);
            let mut failed = read_json(&state_path)?;
            failed["state"] = json!("failed");
            failed["error"] = json!(error.to_string());
            failed["failedAt"] = json!(now_seconds());
            write_json(&state_path, &failed)?;
            return Err(error);
        }
        Ok(status_view(&pending))
    }

    fn spawn_base_build(
        &self,
        root: PathBuf,
        key: String,
        version: String,
        refresh_from: Option<Value>,
    ) -> Result<(), ImageWorkspaceError> {
        let lifecycle = Self {
            store: self.store.clone(),
            backend: Arc::clone(&self.backend),
        };
        thread::Builder::new()
            .name(format!("image-base-{}", &key[..8]))
            .spawn(move || {
                let _guard = BuildWorkerGuard(key.clone());
                lifecycle.build_base(&root, &key, &version, refresh_from.as_ref());
            })
            .map(|_| ())
            .map_err(|error| ImageWorkspaceError::Backend(error.to_string()))
    }

    fn build_base(&self, root: &Path, key: &str, version: &str, refresh_from: Option<&Value>) {
        let result = self.build_base_inner(root, key, version, refresh_from);
        if let Err(error) = result {
            let state_path = self.base_state(key);
            if let Ok(mut state) = read_json(&state_path) {
                if refresh_from.is_some()
                    && state.get("state").and_then(Value::as_str) == Some("ready")
                {
                    state
                        .as_object_mut()
                        .map(|object| object.remove("refreshingVersion"));
                    state["refreshError"] = json!(error.to_string());
                    state["refreshFailedAt"] = json!(now_seconds());
                } else {
                    state["state"] = json!("failed");
                    state["error"] = json!(error.to_string());
                    state["failedAt"] = json!(now_seconds());
                }
                let _write_result = write_json(&state_path, &state);
            }
        }
    }

    fn build_base_inner(
        &self,
        root: &Path,
        key: &str,
        version: &str,
        refresh_from: Option<&Value>,
    ) -> Result<(), ImageWorkspaceError> {
        let _lock = FileLock::exclusive(&self.base_dir(key).join(".build.lock"))?;
        let state_path = self.base_state(key);
        let mut state = read_json(&state_path)?;
        let expected = if refresh_from.is_some() {
            state.get("refreshingVersion").and_then(Value::as_str)
        } else {
            state.get("version").and_then(Value::as_str)
        };
        if expected != Some(version) {
            return Ok(());
        }

        let mut staging = None;
        let result = (|| {
            let baseline_repositories = git_capture(root).map_err(|error| {
                ImageWorkspaceError::Backend(format!("Initial Git capture failed: {error}"))
            })?;
            let mut stage = match refresh_from {
                Some(old_state) => self
                    .backend
                    .open_base_refresh(root, key, version, old_state)?,
                None => self.backend.open_base_staging(root, key, version)?,
            };
            if stage.token.is_none() {
                stage.token = self.backend.current_event_id(root)?;
            }
            staging = Some(stage.clone());
            self.require_space(
                DEFAULT_BASE_MIN_FREE,
                BASE_MIN_FREE_ENV,
                "image base",
                Some(&stage),
            )?;
            let excludes = self.workspace_excludes(root);
            let delta_excludes = index_excludes(&excludes, &baseline_repositories);
            let check = || {
                self.require_space(
                    DEFAULT_BASE_MIN_FREE,
                    BASE_MIN_FREE_ENV,
                    "image base",
                    Some(&stage),
                )
            };
            self.backend
                .copy_base_tree(root, &stage.root, &excludes, &check)
                .map_err(|error| {
                    ImageWorkspaceError::Backend(format!("Base tree copy failed: {error}"))
                })?;
            check()?;
            let delta = self
                .backend
                .sync_delta(root, &stage.root, stage.token.as_ref(), &delta_excludes)
                .map_err(|error| {
                    ImageWorkspaceError::Backend(format!("Base delta sync failed: {error}"))
                })?;
            repair_git_indexes(root, &stage.root, &baseline_repositories).map_err(|error| {
                ImageWorkspaceError::Backend(format!("Git index repair failed: {error}"))
            })?;
            let repositories = git_capture(root).map_err(|error| {
                ImageWorkspaceError::Backend(format!("Git capture failed: {error}"))
            })?;
            let repositories = repositories
                .iter()
                .map(|repository| {
                    let mut record = repository.clone();
                    record["indexFingerprint"] = repository
                        .get("indexFingerprint")
                        .cloned()
                        .unwrap_or(Value::Null);
                    record
                })
                .collect::<Vec<_>>();
            let sealed = self.backend.seal_base(&stage)?;
            let mut ready = json!({
                "schema":STATE_SCHEMA,"state":"ready","repoRoot":root,
                "repoKey":key,"changeDetector":"git-v1","version":version,
                "error":null,"image":sealed.get("image").cloned().unwrap_or(Value::Null),
                "versionPath":sealed.get("versionPath").cloned().unwrap_or(json!(stage.version_path)),
                "token":delta.token.or(stage.token.clone()),"excludes":delta_excludes,
                "repositories":repositories,"createdAt":now_seconds()
            });
            if refresh_from.is_some() {
                ready["refreshOf"] = refresh_from
                    .unwrap_or(&Value::Null)
                    .get("version")
                    .cloned()
                    .unwrap_or(Value::Null);
            }
            write_json(&stage.version_path.join("version.json"), &ready)?;
            write_json(&state_path, &ready)?;
            if let Err(error) = self.prune_base_versions(key) {
                ready["pruneError"] = json!(error.to_string());
                write_json(&state_path, &ready)?;
            }
            Ok::<(), ImageWorkspaceError>(())
        })();
        if let Err(error) = result {
            if let Some(stage) = staging {
                self.backend.remove_base_version(&stage.version_path)?;
            }
            return Err(error);
        }
        state = read_json(&state_path)?;
        if state.get("state").and_then(Value::as_str) == Some("ready") {
            Ok(())
        } else {
            Err(ImageWorkspaceError::InvalidState)
        }
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "image workspace creation runs on dedicated image-job workers"
    )]
    fn wait_for_base_ready(&self, root: &Path) -> Result<Value, ImageWorkspaceError> {
        let key = repo_key(root);
        let deadline = std::time::Instant::now() + BASE_BUILD_WAIT;
        loop {
            let state = read_json(&self.base_state(&key))?;
            match state.get("state").and_then(Value::as_str) {
                Some("ready") => return Ok(state),
                Some("failed") => {
                    return Err(ImageWorkspaceError::Backend(
                        state
                            .get("error")
                            .and_then(Value::as_str)
                            .unwrap_or("base build failed")
                            .to_owned(),
                    ));
                }
                _ if std::time::Instant::now() >= deadline => {
                    return Err(ImageWorkspaceError::Backend(
                        "Image workspace base build timed out".into(),
                    ));
                }
                _ => thread::sleep(BASE_BUILD_POLL),
            }
        }
    }

    pub fn create_workspace(
        &self,
        root: &Path,
        agent_id: &str,
    ) -> Result<Value, ImageWorkspaceError> {
        let root = root_path(root)?;
        let agent_id = safe_id(agent_id)?;
        self.backend.supported(&root)?;
        let base_status = self.start_base_build(&root)?;
        if base_status.get("state").and_then(Value::as_str) != Some("ready") {
            self.wait_for_base_ready(&root)?;
        }
        let key = repo_key(&root);
        let agent_dir = self.store.join("agents").join(agent_id);
        let state_path = agent_dir.join("agent.json");
        let mount = self.store.join("mnt").join(agent_id);
        let _lock = FileLock::exclusive(&agent_dir.join(".workspace.lock"))?;
        let mut state = read_json(&state_path)?;
        if !state.is_null() && state.get("schema").and_then(Value::as_u64) != Some(STATE_SCHEMA) {
            self.detach_for_removal(agent_id, &state, "the reset of an old workspace record")?;
            self.backend.remove_layer(&agent_dir)?;
            let _cleanup = fs::remove_dir(&mount);
            state = Value::Null;
        }
        if state
            .get("repoKey")
            .and_then(Value::as_str)
            .is_some_and(|v| v != key)
        {
            return Err(ImageWorkspaceError::IdentityConflict);
        }
        if matches!(
            state.get("state").and_then(Value::as_str),
            Some("ready" | "archived")
        ) {
            drop(_lock);
            return self.ensure_mounted(agent_id);
        }
        self.require_space(
            DEFAULT_AGENT_MIN_FREE,
            AGENT_MIN_FREE_ENV,
            "image workspace",
            None,
        )?;
        let base_state_path = self.base_state(&key);
        let base = if let Some(version) = state.get("baseVersion").and_then(Value::as_str) {
            read_json(
                &self
                    .base_dir(&key)
                    .join("versions")
                    .join(version)
                    .join("version.json"),
            )?
        } else {
            read_json(&base_state_path)?
        };
        if base.get("state").and_then(Value::as_str) != Some("ready") {
            return Err(ImageWorkspaceError::BaseNotReady);
        }
        fs::create_dir_all(&agent_dir)?;
        if state.is_null() {
            state = json!({
                "schema":STATE_SCHEMA,"repoRoot":root,"repoKey":key,"agentId":agent_id,
                "baseVersion":base.get("version").cloned().unwrap_or(Value::Null),
                "baseImage":base.get("image").cloned().unwrap_or(Value::Null),
                "image":null,"mount":mount,"state":"creating","createdAt":now_seconds()
            });
            write_json(&state_path, &state)?;
        }
        if state.get("image").is_none_or(Value::is_null) {
            let base_image = base
                .get("image")
                .and_then(Value::as_str)
                .ok_or(ImageWorkspaceError::BaseNotReady)?;
            let image = self
                .backend
                .clone_workspace(Path::new(base_image), &agent_dir)?;
            state["image"] = json!(image);
            state["baseImage"] = json!(base_image);
            write_json(&state_path, &state)?;
        }
        let base_image = state
            .get("baseImage")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        self.attach_verified(agent_id, &mut state, base_image.as_deref())?;
        write_json(&state_path, &state)?;
        let workspace_root = mount.join("repo");
        let excludes = base
            .get("excludes")
            .and_then(Value::as_array)
            .map(|xs| {
                xs.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| self.workspace_excludes(&root));
        let delta =
            self.backend
                .sync_delta(&root, &workspace_root, base.get("token"), &excludes)?;
        if delta.refresh_base || delta.history_lost {
            let _refresh_status = self.start_base_build(&root)?;
        }
        let repositories = git_capture(&root)?;
        repair_git_indexes(&root, &workspace_root, &repositories)?;
        state["path"] = json!(workspace_root);
        state["mount"] = json!(mount);
        state["state"] = json!("ready");
        state["mounted"] = json!(true);
        state["repositories"] = json!(repository_index_metadata(&repositories));
        state["deltaFallbackReason"] = Value::Null;
        write_json(&state_path, &state)?;
        let _ = delta;
        Ok(json!({"mount":mount,"path":workspace_root}))
    }

    /// Persist a creating row and hand all base/image work to a worker thread.
    pub fn start_workspace_provision(
        &self,
        root: &Path,
        agent_id: &str,
    ) -> Result<Value, ImageWorkspaceError> {
        let root = root_path(root)?;
        let agent_id = safe_id(agent_id)?.to_owned();
        self.backend.supported(&root)?;
        let key = repo_key(&root);
        let agent_key = format!("agent:{agent_id}");
        let agent_dir = self.store.join("agents").join(&agent_id);
        let state_path = agent_dir.join("agent.json");
        let mount = self.store.join("mnt").join(&agent_id);
        let _lock = FileLock::exclusive(&agent_dir.join(".workspace.lock"))?;
        let mut state = read_json(&state_path)?;
        if state.is_null() {
            state = json!({
                "schema":STATE_SCHEMA,"repoRoot":root,"repoKey":key,"agentId":agent_id,
                "baseVersion":null,"baseImage":null,"image":null,"mount":mount,
                "state":"creating","createdAt":now_seconds()
            });
            write_json(&state_path, &state)?;
        }
        if build_worker_active(&agent_key) {
            return Ok(json!({"state":state.get("state"),"mount":mount,"path":mount.join("repo")}));
        }
        reserve_build_worker(&agent_key);
        drop(_lock);
        let lifecycle = Self {
            store: self.store.clone(),
            backend: Arc::clone(&self.backend),
        };
        let failed_path = state_path.clone();
        let worker_key = agent_key.clone();
        let spawn = thread::Builder::new()
            .name(format!("image-agent-{agent_id}"))
            .spawn(move || {
                let _guard = BuildWorkerGuard(worker_key);
                if let Err(error) = lifecycle.create_workspace(&root, &agent_id)
                    && let Ok(mut state) = read_json(&failed_path)
                {
                    state["state"] = json!("failed");
                    state["error"] = json!(error.to_string());
                    let _persist = write_json(&failed_path, &state);
                }
            });
        if let Err(error) = spawn {
            release_build_worker(&agent_key);
            return Err(ImageWorkspaceError::Backend(error.to_string()));
        }
        Ok(json!({"state":"creating","mount":mount,"path":mount.join("repo")}))
    }

    pub fn ensure_mounted(&self, agent_id: &str) -> Result<Value, ImageWorkspaceError> {
        let agent_id = safe_id(agent_id)?;
        let agent_dir = self.store.join("agents").join(agent_id);
        let state_path = agent_dir.join("agent.json");
        let mut state = read_json(&state_path)?;
        if state.is_null() || state.get("image").is_none_or(Value::is_null) {
            return Err(ImageWorkspaceError::InvalidState);
        }
        let mount = state
            .get("mount")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.store.join("mnt").join(agent_id));
        let _lock = FileLock::exclusive(&agent_dir.join(".workspace.lock"))?;
        let current = read_json(&state_path)?;
        if !current.is_null() {
            state = current;
        }
        let base = state
            .get("baseImage")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        self.attach_verified(agent_id, &mut state, base.as_deref())?;
        if state.get("state").and_then(Value::as_str) == Some("archived") {
            state["state"] = json!("ready");
        }
        let path = mount.join("repo");
        state["mounted"] = json!(true);
        state["path"] = json!(path);
        write_json(&state_path, &state)?;
        Ok(json!({"mount":mount,"path":path}))
    }

    pub fn archive_workspace(&self, agent_id: &str) -> Result<Value, ImageWorkspaceError> {
        let agent_id = safe_id(agent_id)?;
        let agent_dir = self.store.join("agents").join(agent_id);
        let state_path = agent_dir.join("agent.json");
        let mut state = read_json(&state_path)?;
        if state.is_null() {
            return Ok(json!({"freedBytes":0,"state":"removed"}));
        }
        let _lock = FileLock::exclusive(&agent_dir.join(".workspace.lock"))?;
        let current = read_json(&state_path)?;
        if !current.is_null() {
            state = current;
        }
        let unmount = self.detach_for_removal(agent_id, &state, "the archive")?;
        state["state"] = json!("archived");
        state["mounted"] = json!(false);
        if let Some(object) = state.as_object_mut() {
            object.remove("mountIdentity");
        }
        write_json(&state_path, &state)?;
        Ok(json!({"freedBytes":0,"state":"archived","unmount":unmount}))
    }

    pub fn remove_workspace(
        &self,
        agent_id: &str,
        _force: bool,
    ) -> Result<Value, ImageWorkspaceError> {
        let agent_id = safe_id(agent_id)?;
        let agent_dir = self.store.join("agents").join(agent_id);
        let state_path = agent_dir.join("agent.json");
        let mut state = read_json(&state_path)?;
        if state.is_null() {
            return Ok(json!({"freedBytes":0,"state":"removed"}));
        }
        let _lock = FileLock::exclusive(&agent_dir.join(".workspace.lock"))?;
        let current = read_json(&state_path)?;
        if !current.is_null() {
            state = current;
        }
        let mount = state
            .get("mount")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.store.join("mnt").join(agent_id));
        let unmount = self.detach_for_removal(agent_id, &state, "the removal")?;
        self.backend.remove_layer(&agent_dir)?;
        // Only an empty mount point folder is removed; files in it stay.
        let _cleanup = fs::remove_dir(&mount);
        if let Some(key) = state.get("repoKey").and_then(Value::as_str) {
            self.prune_base_versions(key)?;
        }
        Ok(json!({"freedBytes":null,"state":"removed","unmount":unmount}))
    }

    pub fn list_workspaces(&self) -> Result<Vec<Value>, ImageWorkspaceError> {
        let mut values = Vec::new();
        let root = self.store.join("agents");
        if !root.exists() {
            return Ok(values);
        }
        for entry in fs::read_dir(root)? {
            let path = entry?.path().join("agent.json");
            let value = read_json(&path)?;
            if !value.is_null() {
                values.push(value);
            }
        }
        Ok(values)
    }

    fn prune_base_versions(&self, key: &str) -> Result<(), ImageWorkspaceError> {
        let state = read_json(&self.base_state(key))?;
        let current = state.get("version").and_then(Value::as_str);
        let refreshing = state.get("refreshingVersion").and_then(Value::as_str);
        let mut used = BTreeSet::new();
        for agent in self.list_workspaces()? {
            if agent.get("repoKey").and_then(Value::as_str) == Some(key)
                && let Some(version) = agent.get("baseVersion").and_then(Value::as_str)
            {
                used.insert(version.to_owned());
            }
        }
        let versions = self.base_dir(key).join("versions");
        if !versions.exists() {
            return Ok(());
        }
        for entry in fs::read_dir(versions)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if Some(name.as_str()) != current
                && Some(name.as_str()) != refreshing
                && !used.contains(&name)
            {
                self.backend.remove_base_version(&path)?;
            }
        }
        Ok(())
    }

    fn require_space(
        &self,
        default: u64,
        variable: &str,
        purpose: &'static str,
        staging: Option<&WorkspaceBaseStaging>,
    ) -> Result<(), ImageWorkspaceError> {
        let required = minimum_free(variable, default)?;
        self.require_space_bytes(required, purpose, staging)
    }

    fn require_space_bytes(
        &self,
        required: u64,
        purpose: &'static str,
        staging: Option<&WorkspaceBaseStaging>,
    ) -> Result<(), ImageWorkspaceError> {
        let mut existing = self.store.as_path();
        while !existing.exists() {
            existing = existing.parent().ok_or(ImageWorkspaceError::InvalidState)?;
        }
        let free = self.backend.available_bytes(existing)?;
        if free < required {
            return Err(ImageWorkspaceError::FreeSpace {
                purpose,
                free,
                required,
            });
        }
        if let Some(stage) = staging
            && let Some(mount) = &stage.mount
        {
            #[cfg(unix)]
            {
                let mounted = fs::metadata(mount)?.dev();
                let parent = fs::metadata(mount.parent().unwrap_or(mount))?.dev();
                if mounted == parent {
                    return Err(ImageWorkspaceError::MountUnavailable(mount.clone()));
                }
            }
        }
        Ok(())
    }

    fn base_dir(&self, key: &str) -> PathBuf {
        self.store.join("bases").join(key)
    }
    fn base_state(&self, key: &str) -> PathBuf {
        self.base_dir(key).join("base.json")
    }
    fn workspace_excludes(&self, root: &Path) -> Vec<String> {
        let mut result = vec![".worktrees".to_owned()];
        if let Ok(relative) = self.store.strip_prefix(root) {
            result.push(relative.to_string_lossy().replace('\\', "/"));
        }
        result.sort();
        result.dedup();
        result
    }
}

#[cfg(not(target_os = "macos"))]
fn require_platform() -> Result<(), ImageWorkspaceError> {
    Err(ImageWorkspaceError::UnsupportedPlatform)
}

#[cfg(target_os = "macos")]
fn require_platform() -> Result<(), ImageWorkspaceError> {
    Ok(())
}

/// Production entry points refuse on non-macOS before touching workspace state.
#[cfg(not(target_os = "macos"))]
pub fn platform_refusal() -> ImageWorkspaceError {
    ImageWorkspaceError::UnsupportedPlatform
}

#[cfg(target_os = "macos")]
pub fn production() -> WorkspaceLifecycle<MacAsifBackend> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let store = std::env::var_os(STORE_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".local/state/codex-agents/workspaces"));
    WorkspaceLifecycle::with_backend(store, MacAsifBackend)
}

pub fn create_workspace(root: &Path, agent_id: &str) -> Result<Value, ImageWorkspaceError> {
    require_platform()?;
    #[cfg(target_os = "macos")]
    {
        production().create_workspace(root, agent_id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (root, agent_id);
        Err(ImageWorkspaceError::UnsupportedPlatform)
    }
}

pub fn start_workspace_provision(
    root: &Path,
    agent_id: &str,
) -> Result<Value, ImageWorkspaceError> {
    require_platform()?;
    #[cfg(target_os = "macos")]
    {
        production().start_workspace_provision(root, agent_id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (root, agent_id);
        Err(ImageWorkspaceError::UnsupportedPlatform)
    }
}

pub fn supported(root: &Path) -> Result<(), ImageWorkspaceError> {
    require_platform()?;
    #[cfg(target_os = "macos")]
    {
        production().supported(root)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = root;
        Err(ImageWorkspaceError::UnsupportedPlatform)
    }
}
pub fn base_status(root: &Path) -> Result<Value, ImageWorkspaceError> {
    require_platform()?;
    #[cfg(target_os = "macos")]
    {
        production().base_status(root)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = root;
        Err(ImageWorkspaceError::UnsupportedPlatform)
    }
}
pub fn start_base_build(root: &Path) -> Result<Value, ImageWorkspaceError> {
    start_base_build_retry(root, false)
}
pub fn start_base_build_retry(
    root: &Path,
    retry_failed: bool,
) -> Result<Value, ImageWorkspaceError> {
    require_platform()?;
    #[cfg(target_os = "macos")]
    {
        production().start_base_build_retry(root, retry_failed)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (root, retry_failed);
        Err(ImageWorkspaceError::UnsupportedPlatform)
    }
}
pub fn ensure_mounted(agent_id: &str) -> Result<Value, ImageWorkspaceError> {
    require_platform()?;
    #[cfg(target_os = "macos")]
    {
        production().ensure_mounted(agent_id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = agent_id;
        Err(ImageWorkspaceError::UnsupportedPlatform)
    }
}
pub fn archive_workspace(agent_id: &str) -> Result<Value, ImageWorkspaceError> {
    require_platform()?;
    #[cfg(target_os = "macos")]
    {
        production().archive_workspace(agent_id)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = agent_id;
        Err(ImageWorkspaceError::UnsupportedPlatform)
    }
}
pub fn remove_workspace(agent_id: &str, force: bool) -> Result<Value, ImageWorkspaceError> {
    require_platform()?;
    #[cfg(target_os = "macos")]
    {
        production().remove_workspace(agent_id, force)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (agent_id, force);
        Err(ImageWorkspaceError::UnsupportedPlatform)
    }
}
pub fn list_workspaces() -> Result<Vec<Value>, ImageWorkspaceError> {
    require_platform()?;
    #[cfg(target_os = "macos")]
    {
        production().list_workspaces()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(ImageWorkspaceError::UnsupportedPlatform)
    }
}
pub fn exec_prefix() -> Result<Vec<String>, ImageWorkspaceError> {
    require_platform()?;
    #[cfg(target_os = "macos")]
    {
        Ok(production().backend.exec_prefix())
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(ImageWorkspaceError::UnsupportedPlatform)
    }
}

fn safe_id(value: &str) -> Result<&str, ImageWorkspaceError> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return Err(ImageWorkspaceError::InvalidId);
    }
    Ok(value)
}
fn root_path(path: &Path) -> Result<PathBuf, ImageWorkspaceError> {
    let root = path.canonicalize()?;
    if !root.is_dir() {
        return Err(ImageWorkspaceError::Backend(
            "Repository folder does not exist".into(),
        ));
    }
    Ok(root)
}
fn repo_key(root: &Path) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(
        root.as_os_str().to_string_lossy().as_bytes(),
    ))[..32]
        .to_owned()
}
fn minimum_free(variable: &str, default: u64) -> Result<u64, ImageWorkspaceError> {
    let Some(value) = std::env::var_os(variable) else {
        return Ok(default);
    };
    let value = value
        .to_str()
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| {
            ImageWorkspaceError::Backend(format!("{variable} must be a non-negative integer"))
        })?;
    Ok(value)
}
fn status_view(state: &Value) -> Value {
    json!({"state":state.get("state").cloned().unwrap_or(Value::Null),"version":state.get("version").cloned().unwrap_or(Value::Null),"error":state.get("error").cloned().unwrap_or(Value::Null)})
}
fn read_json(path: &Path) -> Result<Value, ImageWorkspaceError> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Value::Null),
        Err(error) => Err(error.into()),
    }
}
fn write_json(path: &Path, value: &Value) -> Result<(), ImageWorkspaceError> {
    fs::create_dir_all(path.parent().ok_or(ImageWorkspaceError::InvalidState)?)?;
    crate::atomic_file::replace(path, &serde_json::to_vec(value)?, None, true)?;
    Ok(())
}
fn now_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(unix)]
struct FileLock(File);
#[cfg(unix)]
impl FileLock {
    #[allow(deprecated)]
    fn exclusive(path: &Path) -> Result<Self, ImageWorkspaceError> {
        use nix::fcntl::{FlockArg, flock};
        use std::os::fd::AsRawFd;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        flock(file.as_raw_fd(), FlockArg::LockExclusive).map_err(std::io::Error::other)?;
        Ok(Self(file))
    }

    #[allow(deprecated)]
    fn try_exclusive(path: &Path) -> Result<Option<Self>, ImageWorkspaceError> {
        use nix::fcntl::{FlockArg, flock};
        use std::os::fd::AsRawFd;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
            Ok(()) => Ok(Some(Self(file))),
            Err(nix::errno::Errno::EWOULDBLOCK) => Ok(None),
            Err(error) => Err(std::io::Error::other(error).into()),
        }
    }
}
#[cfg(unix)]
impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        #[allow(deprecated)]
        let _unlock = nix::fcntl::flock(self.0.as_raw_fd(), nix::fcntl::FlockArg::Unlock);
    }
}
#[cfg(not(unix))]
struct FileLock(File);
#[cfg(not(unix))]
impl FileLock {
    fn exclusive(path: &Path) -> Result<Self, ImageWorkspaceError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(Self(
            OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .open(path)?,
        ))
    }

    fn try_exclusive(path: &Path) -> Result<Option<Self>, ImageWorkspaceError> {
        Self::exclusive(path).map(Some)
    }
}

struct BuildWorkerGuard(String);
impl Drop for BuildWorkerGuard {
    fn drop(&mut self) {
        release_build_worker(&self.0);
    }
}

fn git_capture(root: &Path) -> Result<Vec<Value>, ImageWorkspaceError> {
    let mut repos = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let git = directory.join(".git");
        if (git.is_dir() || git.is_file())
            && let Ok(top) = git_output(&directory, &["rev-parse", "--show-toplevel"])
            && PathBuf::from(top.trim()).canonicalize().ok() == directory.canonicalize().ok()
        {
            let head = git_output(&directory, &["rev-parse", "HEAD"]).ok();
            let dirty = git_output_bytes(
                &directory,
                &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            )?;
            let index = git_output(&directory, &["rev-parse", "--git-path", "index"])
                .ok()
                .and_then(|p| {
                    let path = PathBuf::from(p.trim());
                    let path = if path.is_absolute() {
                        path
                    } else {
                        directory.join(path)
                    };
                    index_fingerprint(&path)
                });
            let relative = directory
                .strip_prefix(root)
                .unwrap_or(&directory)
                .to_string_lossy();
            let relative = if relative.is_empty() {
                "."
            } else {
                relative.as_ref()
            };
            repos.push(json!({
                "path":relative,
                "head":head.map(|s|s.trim().to_owned()),
                "dirtyPaths":dirty_paths(&dirty),
                "indexFingerprint":index,
                "gitMetadataFingerprint":git_metadata_fingerprint(&directory)?
            }));
        }
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_type()?.is_dir()
                && !entry.file_type()?.is_symlink()
                && entry.file_name() != ".git"
                && entry.file_name() != ".worktrees"
            {
                pending.push(entry.path());
            }
        }
    }
    Ok(repos)
}

fn index_excludes(excludes: &[String], repositories: &[Value]) -> Vec<String> {
    let mut values = excludes.iter().cloned().collect::<BTreeSet<_>>();
    for repository in repositories {
        if let Some(relative) = repository.get("path").and_then(Value::as_str) {
            values.insert(if relative == "." {
                ".git/index".to_owned()
            } else {
                format!("{relative}/.git/index")
            });
        }
    }
    values.into_iter().collect()
}

fn repository_index_metadata(repositories: &[Value]) -> Vec<Value> {
    repositories
        .iter()
        .filter_map(|repository| {
            Some(json!({
                "path": repository.get("path")?,
                "indexFingerprint": repository.get("indexFingerprint").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect()
}

fn repair_git_indexes(
    source_root: &Path,
    target_root: &Path,
    repositories: &[Value],
) -> Result<(), ImageWorkspaceError> {
    for repository in repositories {
        let relative = repository
            .get("path")
            .and_then(Value::as_str)
            .ok_or(ImageWorkspaceError::InvalidState)?;
        let source_repo = if relative == "." {
            source_root.to_path_buf()
        } else {
            source_root.join(relative)
        };
        let target_repo = if relative == "." {
            target_root.to_path_buf()
        } else {
            target_root.join(relative)
        };
        let source_git = source_repo.join(".git");
        let target_git = target_repo.join(".git");
        if source_git.is_file() {
            let content = fs::read_to_string(&source_git)?;
            let pointer = content
                .strip_prefix("gitdir: ")
                .map(str::trim)
                .ok_or(ImageWorkspaceError::InvalidState)?;
            let source_git_dir = Path::new(pointer);
            let source_git_dir = if source_git_dir.is_absolute() {
                source_git_dir.to_path_buf()
            } else {
                source_repo.join(source_git_dir)
            };
            let source_git_dir = source_git_dir.canonicalize()?;
            let git_relative = source_git_dir
                .strip_prefix(source_root)
                .map_err(|_| ImageWorkspaceError::InvalidState)?;
            let target_git_dir = target_root.join(git_relative);
            fs::create_dir_all(
                target_git_dir
                    .parent()
                    .ok_or(ImageWorkspaceError::InvalidState)?,
            )?;
            let pointer = relative_path(&target_repo, &target_git_dir);
            fs::write(target_git, format!("gitdir: {}\n", pointer.display()))?;
        }
        let source_index_text = git_output(&source_repo, &["rev-parse", "--git-path", "index"])?;
        let source_index = PathBuf::from(source_index_text.trim());
        let source_index = if source_index.is_absolute() {
            source_index
        } else {
            source_repo.join(source_index)
        };
        let source_index = source_index.canonicalize()?;
        let index_relative = source_index
            .strip_prefix(source_root)
            .map_err(|_| ImageWorkspaceError::InvalidState)?;
        let target_index = target_root.join(index_relative);
        fs::create_dir_all(
            target_index
                .parent()
                .ok_or(ImageWorkspaceError::InvalidState)?,
        )?;
        fs::copy(source_index, target_index)?;
    }
    Ok(())
}

fn relative_path(from: &Path, to: &Path) -> PathBuf {
    let from = from.components().collect::<Vec<_>>();
    let to = to.components().collect::<Vec<_>>();
    let shared = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut result = PathBuf::new();
    for _ in shared..from.len() {
        result.push("..");
    }
    for component in &to[shared..] {
        result.push(component.as_os_str());
    }
    result
}

fn git_metadata_fingerprint(repo: &Path) -> Result<String, ImageWorkspaceError> {
    let refs = git_output_bytes(
        repo,
        &["for-each-ref", "--format=%(refname)%00%(objectname)"],
    )?;
    let git_dir = PathBuf::from(git_output(repo, &["rev-parse", "--absolute-git-dir"])?.trim())
        .canonicalize()?;
    let common_text = git_output(repo, &["rev-parse", "--git-common-dir"])?;
    let common = PathBuf::from(common_text.trim());
    let common = if common.is_absolute() {
        common
    } else {
        repo.join(common)
    }
    .canonicalize()?;
    let mut state = Vec::new();
    for path in BTreeSet::from([git_dir, common]) {
        metadata_tree_fingerprint(&path, &mut state)?;
        let alternates = path.join("objects/info/alternates");
        if alternates.exists() {
            let metadata = fs::symlink_metadata(&alternates)?;
            let content = if metadata.file_type().is_symlink() {
                fs::read_link(&alternates)?
                    .into_os_string()
                    .as_encoded_bytes()
                    .to_vec()
            } else {
                fs::read(&alternates)?
            };
            state.push(json!([
                alternates.to_string_lossy(),
                [
                    metadata.mode(),
                    metadata.len(),
                    metadata.mtime().saturating_mul(1_000_000_000) + metadata.mtime_nsec(),
                    metadata.ctime().saturating_mul(1_000_000_000) + metadata.ctime_nsec(),
                    data_encoding::HEXLOWER.encode(&Sha256::digest(content))
                ]
            ]));
        } else {
            state.push(json!([alternates.to_string_lossy(), null]));
        }
    }
    let metadata = serde_json::to_vec(&state)?;
    let mut digest = Sha256::new();
    digest.update(refs);
    digest.update(metadata);
    Ok(data_encoding::HEXLOWER.encode(&digest.finalize()))
}

#[cfg(unix)]
fn metadata_tree_fingerprint(
    root: &Path,
    state: &mut Vec<Value>,
) -> Result<(), ImageWorkspaceError> {
    if !root.exists() {
        state.push(json!([root.to_string_lossy(), null]));
        return Ok(());
    }
    let mut directories = vec![root.to_path_buf()];
    let mut cursor = 0;
    while cursor < directories.len() {
        let directory = directories[cursor].clone();
        cursor += 1;
        let metadata = fs::symlink_metadata(&directory)?;
        let relative = directory
            .strip_prefix(root)
            .unwrap_or(&directory)
            .to_string_lossy();
        let relative = if relative.is_empty() {
            "."
        } else {
            relative.as_ref()
        };
        let label = format!("{}/{relative}", root.display());
        state.push(json!([label, ["directory", metadata.mode()]]));
        let mut entries = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        let mut child_directories = Vec::new();
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name();
            if name == "objects" || name == "index" {
                continue;
            }
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                child_directories.push(path);
            } else {
                let content = if metadata.file_type().is_symlink() {
                    fs::read_link(&path)?
                        .into_os_string()
                        .as_encoded_bytes()
                        .to_vec()
                } else if metadata.is_file() {
                    fs::read(&path)?
                } else {
                    Vec::new()
                };
                let content_hash = data_encoding::HEXLOWER.encode(&Sha256::digest(content));
                state.push(json!([
                    path.to_string_lossy(),
                    [
                        metadata.mode(),
                        metadata.len(),
                        metadata.mtime().saturating_mul(1_000_000_000) + metadata.mtime_nsec(),
                        metadata.ctime().saturating_mul(1_000_000_000) + metadata.ctime_nsec(),
                        content_hash
                    ]
                ]));
            }
        }
        directories.extend(child_directories);
    }
    state.sort_by(|left, right| left[0].as_str().cmp(&right[0].as_str()));
    Ok(())
}

#[cfg(not(unix))]
fn metadata_tree_fingerprint(
    _root: &Path,
    _state: &mut Vec<Value>,
) -> Result<(), ImageWorkspaceError> {
    Ok(())
}

fn dirty_paths(raw: &[u8]) -> Vec<String> {
    let records = raw.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut paths = Vec::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        index += 1;
        if record.len() < 4 {
            continue;
        }
        paths.push(String::from_utf8_lossy(&record[3..]).into_owned());
        if matches!(record[0], b'R' | b'C') && index < records.len() {
            paths.push(String::from_utf8_lossy(records[index]).into_owned());
            index += 1;
        }
    }
    paths.sort();
    paths.dedup();
    paths
}

#[cfg(unix)]
fn index_fingerprint(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let mtime_ns = metadata.mtime().saturating_mul(1_000_000_000) + metadata.mtime_nsec();
    let ctime_ns = metadata.ctime().saturating_mul(1_000_000_000) + metadata.ctime_nsec();
    let identity = format!(
        "[{}, {}, {}, {}, {}]",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        mtime_ns,
        ctime_ns
    );
    Some(data_encoding::HEXLOWER.encode(&Sha256::digest(identity.as_bytes())))
}

#[cfg(not(unix))]
fn index_fingerprint(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let identity = serde_json::to_vec(&json!([metadata.len(), metadata.modified().ok()?])).ok()?;
    Some(data_encoding::HEXLOWER.encode(&Sha256::digest(identity)))
}
fn git_output(repo: &Path, args: &[&str]) -> Result<String, ImageWorkspaceError> {
    String::from_utf8(git_output_bytes(repo, args)?)
        .map_err(|e| ImageWorkspaceError::Backend(e.to_string()))
}
fn git_output_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>, ImageWorkspaceError> {
    let command_args = args;
    let output = crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("git"),
        cwd: repo.as_os_str(),
        args: command_args,
        environment: &[("GIT_OPTIONAL_LOCKS", "0")],
        input: None,
        stdout_limit: 1024 * 1024,
        stderr_limit: 4096,
        timeout: Duration::from_secs(30),
    })
    .map_err(|_| ImageWorkspaceError::Backend("Git workspace capture failed".into()))?;
    Ok(output.stdout.bytes)
}

#[path = "image_workspace_mounts.rs"]
mod mounts;
pub use mounts::{WorkspaceAttachment, mount_diagnostics, mounts_command, run_startup_mount_check};

#[cfg(target_os = "macos")]
#[path = "image_workspace_macos.rs"]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacAsifBackend;

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::sync::{Mutex, mpsc};

    struct CopyGate {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    const ASYNC_PROVISION_TIMEOUT: Duration = Duration::from_secs(30);

    #[derive(Default)]
    struct FakeBackend {
        store: PathBuf,
        fail_once: Mutex<Option<&'static str>>,
        copy_gate: Option<CopyGate>,
        available_bytes: Option<u64>,
        /// Mount point -> image. A detach moves the volume into `<image>.volume`.
        mounted: Mutex<std::collections::BTreeMap<PathBuf, PathBuf>>,
    }

    impl FakeBackend {
        fn fail(&self, point: &'static str) -> Result<(), ImageWorkspaceError> {
            let mut failure = self.fail_once.lock().expect("failure lock");
            if *failure == Some(point) {
                *failure = None;
                return Err(ImageWorkspaceError::Backend(format!("failpoint:{point}")));
            }
            Ok(())
        }
    }

    impl WorkspaceBackend for FakeBackend {
        fn supported(&self, _: &Path) -> Result<(), ImageWorkspaceError> {
            Ok(())
        }
        fn current_event_id(&self, _: &Path) -> Result<Option<Value>, ImageWorkspaceError> {
            Ok(Some(json!(41)))
        }
        fn open_base_staging(
            &self,
            _: &Path,
            key: &str,
            version: &str,
        ) -> Result<WorkspaceBaseStaging, ImageWorkspaceError> {
            self.fail("open")?;
            let version_path = self
                .store
                .join("bases")
                .join(key)
                .join("versions")
                .join(version);
            let root = version_path.join("repo");
            fs::create_dir_all(&root)?;
            let image = version_path.join("base.asif");
            File::create(&image)?;
            Ok(WorkspaceBaseStaging {
                root,
                token: None,
                version_path,
                image: Some(image),
                mount: None,
                version: version.to_owned(),
                refresh: false,
            })
        }
        fn copy_base_tree(
            &self,
            source: &Path,
            destination: &Path,
            excludes: &[String],
            check: &dyn Fn() -> Result<(), ImageWorkspaceError>,
        ) -> Result<(), ImageWorkspaceError> {
            if let Some(gate) = &self.copy_gate {
                gate.entered
                    .send(())
                    .map_err(|error| ImageWorkspaceError::Backend(error.to_string()))?;
                gate.release
                    .lock()
                    .map_err(|error| ImageWorkspaceError::Backend(error.to_string()))?
                    .recv_timeout(ASYNC_PROVISION_TIMEOUT * 2)
                    .map_err(|error| ImageWorkspaceError::Backend(error.to_string()))?;
            }
            copy_fake(source, destination, source, excludes)?;
            check()?;
            self.fail("copy")
        }
        fn seal_base(&self, stage: &WorkspaceBaseStaging) -> Result<Value, ImageWorkspaceError> {
            self.fail("seal")?;
            Ok(json!({"image":stage.image,"versionPath":stage.version_path,"token":stage.token}))
        }
        fn clone_workspace(
            &self,
            image: &Path,
            agent_dir: &Path,
        ) -> Result<PathBuf, ImageWorkspaceError> {
            self.fail("clone")?;
            fs::create_dir_all(agent_dir)?;
            let layer = agent_dir.join("workspace.asif");
            fs::copy(image, &layer)?;
            Ok(layer)
        }
        fn mount_workspace(
            &self,
            layer: &Path,
            mount: &Path,
            _: Option<&Path>,
        ) -> Result<WorkspaceMount, ImageWorkspaceError> {
            self.fail("mount")?;
            let volume = layer.with_extension("volume");
            if volume.exists() {
                fs::remove_dir(mount)?;
                fs::rename(&volume, mount)?;
            }
            fs::create_dir_all(mount.join("repo"))?;
            self.mounted
                .lock()
                .expect("mounted volumes")
                .insert(mount.to_path_buf(), layer.to_path_buf());
            Ok(WorkspaceMount {
                mount: mount.to_string_lossy().into_owned(),
                layer: Some(layer.to_string_lossy().into_owned()),
                ..WorkspaceMount::default()
            })
        }
        fn sync_delta(
            &self,
            root: &Path,
            target: &Path,
            token: Option<&Value>,
            excludes: &[String],
        ) -> Result<WorkspaceDelta, ImageWorkspaceError> {
            self.fail("sync")?;
            copy_fake(root, target, root, excludes)?;
            Ok(WorkspaceDelta {
                token: token.cloned(),
                changed_paths: vec![".".into()],
                history_lost: false,
                scan_paths: vec![".".into()],
                refresh_base: false,
            })
        }
        fn unmount_workspace(&self, mount: &Path, _: bool) -> Result<Value, ImageWorkspaceError> {
            self.fail("unmount")?;
            let layer = self.mounted.lock().expect("mounted volumes").remove(mount);
            if let Some(layer) = layer {
                fs::rename(mount, layer.with_extension("volume"))?;
                fs::create_dir(mount)?;
            }
            Ok(json!({"method":"fake","terminatedPids":0}))
        }
        fn is_mounted(&self, path: &Path) -> Result<bool, ImageWorkspaceError> {
            Ok(self
                .mounted
                .lock()
                .expect("mounted volumes")
                .contains_key(path))
        }
        fn attachments(&self) -> Result<Vec<WorkspaceAttachment>, ImageWorkspaceError> {
            Ok(self
                .mounted
                .lock()
                .expect("mounted volumes")
                .iter()
                .map(|(mount, layer)| WorkspaceAttachment {
                    image: Some(layer.clone()),
                    mounts: vec![mount.clone()],
                    device: None,
                })
                .collect())
        }
        fn remove_layer(&self, path: &Path) -> Result<(), ImageWorkspaceError> {
            self.fail("remove")?;
            if path.exists() {
                fs::remove_dir_all(path)?;
            }
            Ok(())
        }
        fn remove_base_version(&self, path: &Path) -> Result<(), ImageWorkspaceError> {
            if path.exists() {
                fs::remove_dir_all(path)?;
            }
            Ok(())
        }
        fn private_bytes(&self, _: &Path) -> Result<Option<u64>, ImageWorkspaceError> {
            Ok(Some(0))
        }
        fn available_bytes(&self, _: &Path) -> Result<u64, ImageWorkspaceError> {
            Ok(self.available_bytes.unwrap_or(u64::MAX))
        }
        fn exec_prefix(&self) -> Vec<String> {
            Vec::new()
        }
    }

    fn copy_fake(
        source: &Path,
        target: &Path,
        root: &Path,
        excludes: &[String],
    ) -> Result<(), ImageWorkspaceError> {
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if excludes
                .iter()
                .any(|x| relative == *x || relative.starts_with(&format!("{x}/")))
            {
                continue;
            }
            let target_path = target.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                copy_fake(&path, &target_path, root, excludes)?;
            } else if entry.file_type()?.is_file() {
                fs::copy(path, target_path)?;
            }
        }
        Ok(())
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn non_macos_public_entry_refuses_exactly_before_touching_root() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = fixture.path().join("root");
        let store = fixture.path().join("store");
        fs::create_dir_all(&root).expect("root");
        let seeded = store.join("agents/agent-1/agent.json");
        write_json(&seeded, &json!({"state":"ready"})).expect("seed state");
        let before = fs::read(&seeded).expect("before state");
        let calls = [
            supported(&root).map(|_| ()),
            base_status(&root).map(|_| ()),
            start_base_build(&root).map(|_| ()),
            start_base_build_retry(&root, true).map(|_| ()),
            create_workspace(&root, "agent-1").map(|_| ()),
            ensure_mounted("agent-1").map(|_| ()),
            archive_workspace("agent-1").map(|_| ()),
            remove_workspace("agent-1", true).map(|_| ()),
            list_workspaces().map(|_| ()),
            exec_prefix().map(|_| ()),
        ];
        for result in calls {
            let error = result.expect_err("ASIF is unsupported on Linux");
            assert_eq!(error.to_string(), PLATFORM_REFUSAL);
            assert_eq!(fs::read(&seeded).expect("seed remains"), before);
        }
        assert_eq!(
            fs::read_dir(store.join("agents"))
                .expect("agents remain")
                .count(),
            1
        );
        let readiness = crate::workspace_ready_git::image_workspace_refusal_decision();
        let crate::workspace_ready::WorkspaceDecision::Refuse(refusal) = readiness else {
            panic!("image readiness must refuse on Linux");
        };
        assert_eq!(refusal.to_string(), PLATFORM_REFUSAL);
        assert_eq!(fs::read(&seeded).expect("seed remains"), before);
    }

    #[test]
    fn fsevents_id_progress_is_not_history_loss_but_dropped_history_is() {
        assert!(!fsevents_history_lost(&[(101, 0), (102, 0x1), (102, 0x10)]));
        assert!(fsevents_history_lost(&[(102, 0x2)]));
        assert!(fsevents_history_lost(&[(102, 0x4)]));
        assert!(fsevents_history_lost(&[(102, 0x8)]));
    }

    fn wait_for_build(manager: &WorkspaceLifecycle<FakeBackend>, root: &Path) -> Value {
        // The worker key is that of the canonical root, as production builds it.
        let key = repo_key(&root_path(root).expect("canonical root"));
        assert!(
            wait_for_build_worker(&key, ASYNC_PROVISION_TIMEOUT),
            "base build worker completion; status={:?}",
            manager.base_status(root)
        );
        manager.base_status(root).expect("base state")
    }

    #[test]
    fn low_space_refuses_before_starting_a_base_build() {
        let fixture = tempfile::tempdir().expect("fixture");
        let store = fixture.path().join("store");
        let manager = WorkspaceLifecycle::with_backend(
            &store,
            FakeBackend {
                store: store.clone(),
                available_bytes: Some(0),
                ..FakeBackend::default()
            },
        );
        let error = manager
            .start_base_build(fixture.path())
            .expect_err("no capacity");
        assert!(matches!(
            error,
            ImageWorkspaceError::FreeSpace { free: 0, .. }
        ));
        let key = repo_key(&fixture.path().canonicalize().expect("canonical project"));
        assert!(
            !manager.base_state(&key).exists(),
            "no pending build may be published"
        );
        assert!(
            !manager.base_dir(&key).join("versions").exists(),
            "no image staging may start"
        );
        assert!(!build_worker_active(&key), "no background copy may start");
    }

    #[test]
    fn low_space_preflight_uses_the_injected_host_probe() {
        let fixture = tempfile::tempdir().expect("fixture");
        let manager = WorkspaceLifecycle::with_backend(
            fixture.path().join("store"),
            FakeBackend {
                available_bytes: Some(0),
                ..FakeBackend::default()
            },
        );

        assert!(matches!(
            manager.require_space_bytes(1, "image base", None),
            Err(ImageWorkspaceError::FreeSpace {
                purpose: "image base",
                free: 0,
                required: 1,
            })
        ));
    }

    #[test]
    fn async_workspace_provision_returns_while_base_copy_is_blocked() {
        let fixture = tempfile::tempdir().expect("fixture");
        let store = fixture.path().join("store");
        let project = fixture.path().join("project");
        fs::create_dir_all(&project).expect("project");
        fs::write(project.join("file.txt"), "base").expect("source");
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let manager = Arc::new(WorkspaceLifecycle::with_backend(
            &store,
            FakeBackend {
                store: store.clone(),
                fail_once: Mutex::new(None),
                copy_gate: Some(CopyGate {
                    entered: entered_tx,
                    release: Mutex::new(release_rx),
                }),
                available_bytes: None,
                ..FakeBackend::default()
            },
        ));
        let (result_tx, result_rx) = mpsc::channel();
        let caller_manager = Arc::clone(&manager);
        let caller_project = project.clone();
        let caller = thread::spawn(move || {
            let result = caller_manager.start_workspace_provision(&caller_project, "async-agent");
            result_tx
                .send(result)
                .expect("provision result receiver is alive");
        });
        let entered = entered_rx.recv_timeout(ASYNC_PROVISION_TIMEOUT);
        let returned_while_copy_blocked = result_rx.recv_timeout(ASYNC_PROVISION_TIMEOUT);
        release_tx
            .send(())
            .expect("base copy gate is still waiting");
        caller.join().expect("provision caller exits");
        assert!(
            entered.is_ok(),
            "base copy did not reach gate; provision result={returned_while_copy_blocked:?}, base state={:?}, workspace rows={:?}",
            manager.base_status(&project),
            manager.list_workspaces()
        );
        let pending = returned_while_copy_blocked
            .expect("provision returned while base copy remained blocked")
            .expect("provision enqueued");
        assert_eq!(pending["state"], "creating");
        let deadline = std::time::Instant::now() + ASYNC_PROVISION_TIMEOUT;
        loop {
            let row = manager
                .list_workspaces()
                .expect("workspace rows")
                .into_iter()
                .find(|row| row["agentId"] == "async-agent")
                .expect("durable creating row");
            if row["state"] == "ready" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "async workspace ready"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn stale_foreign_builder_row_is_restarted() {
        let fixture = tempfile::tempdir().expect("fixture");
        let store = fixture.path().join("store");
        let project = fixture.path().join("project");
        fs::create_dir_all(&project).expect("project");
        fs::write(project.join("file.txt"), "base").expect("source");
        let manager = WorkspaceLifecycle::with_backend(
            &store,
            FakeBackend {
                store: store.clone(),
                fail_once: Mutex::new(None),
                copy_gate: None,
                available_bytes: None,
                ..FakeBackend::default()
            },
        );
        let key = repo_key(&project.canonicalize().expect("canonical project"));
        write_json(
            &manager.base_state(&key),
            &json!({"schema":STATE_SCHEMA,"state":"building","repoRoot":project,
                "repoKey":key,"changeDetector":"git-v1","version":"v-1",
                "error":null,"startedAt":now_seconds(),"builderPid":u64::MAX}),
        )
        .expect("seed stale builder");
        assert_eq!(
            manager.base_status(&project).expect("stale builder status")["state"],
            "missing"
        );
        assert_eq!(
            manager.start_base_build(&project).expect("restart")["state"],
            "building"
        );
        assert_eq!(wait_for_build(&manager, &project)["state"], "ready");
        let ready = read_json(&manager.base_state(&key)).expect("ready row");
        assert_ne!(ready["version"], "v-1");
    }

    #[test]
    fn fake_backend_lifecycle_recovers_failpoints_and_syncs_changed_content() {
        let fixture = tempfile::tempdir().expect("fixture");
        let store = fixture.path().join("store");
        let project = fixture.path().join("project");
        fs::create_dir_all(&project).expect("project");
        fs::write(project.join("file.txt"), "base").expect("source file");
        let backend = FakeBackend {
            store: store.clone(),
            fail_once: Mutex::new(Some("copy")),
            copy_gate: None,
            available_bytes: None,
            ..FakeBackend::default()
        };
        let manager = WorkspaceLifecycle::with_backend(&store, backend);

        assert_eq!(
            manager.start_base_build(&project).expect("build start")["state"],
            "building"
        );
        assert_eq!(wait_for_build(&manager, &project)["state"], "failed");
        assert_eq!(
            read_json(
                &manager.base_state(&repo_key(&root_path(&project).expect("canonical project")))
            )
            .expect("failed row")["error"],
            "Workspace backend failed: Base tree copy failed: Workspace backend failed: failpoint:copy"
        );
        assert_eq!(
            manager.start_base_build(&project).expect("cooldown status")["state"],
            "failed"
        );
        *manager
            .backend
            .fail_once
            .lock()
            .expect("seal failpoint lock") = Some("seal");
        assert_eq!(
            manager
                .start_base_build_retry(&project, true)
                .expect("base retry")["state"],
            "building"
        );
        assert_eq!(wait_for_build(&manager, &project)["state"], "failed");
        assert_eq!(
            manager
                .start_base_build_retry(&project, true)
                .expect("seal retry")["state"],
            "building"
        );
        assert_eq!(wait_for_build(&manager, &project)["state"], "ready");
        let base = read_json(
            &manager.base_state(&repo_key(&root_path(&project).expect("canonical project"))),
        )
        .expect("base row");
        let version = base["version"].as_str().expect("base version");
        let copied_file = store
            .join("bases")
            .join(repo_key(&root_path(&project).expect("canonical project")))
            .join("versions")
            .join(version)
            .join("repo/file.txt");
        assert_eq!(
            fs::read_to_string(copied_file).expect("copied base"),
            "base"
        );

        *manager.backend.fail_once.lock().expect("failpoint lock") = Some("clone");
        assert!(
            matches!(manager.create_workspace(&project, "agent-clone-fail"), Err(ImageWorkspaceError::Backend(message)) if message == "failpoint:clone")
        );
        assert_eq!(
            manager
                .list_workspaces()
                .expect("clone fail row")
                .iter()
                .find(|row| row["agentId"] == "agent-clone-fail")
                .expect("clone row")["state"],
            "creating"
        );
        manager
            .create_workspace(&project, "agent-clone-fail")
            .expect("clone retry");

        *manager.backend.fail_once.lock().expect("failpoint lock") = Some("mount");
        assert!(
            matches!(manager.create_workspace(&project, "agent-1"), Err(ImageWorkspaceError::Backend(message)) if message == "failpoint:mount")
        );
        // list_workspaces follows directory order, so select the row by agent.
        let agent_one = |label: &str| {
            manager
                .list_workspaces()
                .expect(label)
                .into_iter()
                .find(|row| row["agentId"] == "agent-1")
                .expect("agent-1 row")
        };
        assert_eq!(agent_one("pending state")["state"], "creating");
        fs::write(project.join("file.txt"), "changed under same repo identity")
            .expect("mutated source");

        let ready = manager
            .create_workspace(&project, "agent-1")
            .expect("workspace replay");
        assert_eq!(agent_one("ready state")["state"], "ready");
        let workspace_file = PathBuf::from(ready["path"].as_str().expect("path")).join("file.txt");
        assert_eq!(
            fs::read_to_string(workspace_file).expect("synced file"),
            "changed under same repo identity"
        );

        *manager.backend.fail_once.lock().expect("failpoint lock") = Some("sync");
        assert!(matches!(
            manager.create_workspace(&project, "agent-2"),
            Err(ImageWorkspaceError::Backend(message)) if message == "failpoint:sync"
        ));
        let agent_two = manager
            .list_workspaces()
            .expect("sync failure row")
            .into_iter()
            .find(|row| row["agentId"] == "agent-2")
            .expect("agent two row");
        assert_eq!(agent_two["state"], "creating");
        let ready_two = manager
            .create_workspace(&project, "agent-2")
            .expect("sync replay");
        assert_ne!(ready_two["path"], ready["path"]);
        assert_eq!(
            fs::read_to_string(
                PathBuf::from(ready_two["path"].as_str().expect("agent two path")).join("file.txt")
            )
            .expect("agent two sync"),
            "changed under same repo identity"
        );

        *manager.backend.fail_once.lock().expect("failpoint lock") = Some("unmount");
        assert!(manager.archive_workspace("agent-1").is_err());
        let agent_one = manager
            .list_workspaces()
            .expect("archive failure row")
            .into_iter()
            .find(|row| row["agentId"] == "agent-1")
            .expect("agent one row");
        assert_eq!(agent_one["state"], "ready");

        assert_eq!(
            manager.archive_workspace("agent-1").expect("archive")["state"],
            "archived"
        );
        assert_eq!(
            manager.ensure_mounted("agent-1").expect("reattach")["path"],
            ready["path"]
        );
        *manager.backend.fail_once.lock().expect("failpoint lock") = Some("remove");
        assert!(manager.remove_workspace("agent-1", false).is_err());
        assert_eq!(
            manager.remove_workspace("agent-1", false).expect("remove")["state"],
            "removed"
        );
        let remaining = manager.list_workspaces().expect("remaining workspaces");
        assert!(!remaining.iter().any(|row| row["agentId"] == "agent-1"));
        assert!(remaining.iter().any(|row| row["agentId"] == "agent-2"));
    }

    /// A ready workspace for each agent, attached through the fake volume model.
    fn ready_workspaces(fixture: &Path, agents: &[&str]) -> WorkspaceLifecycle<FakeBackend> {
        let store = fixture.join("store");
        let project = fixture.join("project");
        fs::create_dir_all(&project).expect("project");
        fs::write(project.join("file.txt"), "base").expect("source file");
        let manager = WorkspaceLifecycle::with_backend(
            &store,
            FakeBackend {
                store: store.clone(),
                ..FakeBackend::default()
            },
        );
        for agent in agents {
            manager
                .create_workspace(&project, agent)
                .expect("ready workspace");
        }
        manager
    }

    fn agent_row(manager: &WorkspaceLifecycle<FakeBackend>, agent: &str) -> Value {
        read_json(&manager.store.join("agents").join(agent).join("agent.json")).expect("agent row")
    }

    fn mount_status(error: ImageWorkspaceError) -> &'static str {
        match error {
            ImageWorkspaceError::MountCheck { status, .. } => status,
            other => panic!("expected a mount check refusal, got {other}"),
        }
    }

    /// Attach a volume at `mount` in the fake, with a marker that names `owner`.
    fn attach_marked_volume(
        manager: &WorkspaceLifecycle<FakeBackend>,
        mount: &Path,
        image: &Path,
        owner: &str,
    ) {
        fs::create_dir_all(mount).expect("volume root");
        fs::write(
            mount.join(studio_runtime_schema::shared_values::IMAGE_WORKSPACE_MARKER_NAME),
            json!({"schema":1,"agentId":owner,"image":image}).to_string(),
        )
        .expect("marker");
        manager
            .backend
            .mounted
            .lock()
            .expect("mounted volumes")
            .insert(mount.to_path_buf(), image.to_path_buf());
    }

    #[test]
    fn attach_records_marker_and_identity_and_reuses_only_that_volume() {
        let fixture = tempfile::tempdir().expect("fixture");
        let manager = ready_workspaces(fixture.path(), &["agent-1"]);
        let row = agent_row(&manager, "agent-1");
        let mount = PathBuf::from(row["mount"].as_str().expect("mount"));
        let marker = read_json(&mount.join(".studio-workspace.json")).expect("marker");
        let image = fs::metadata(row["image"].as_str().expect("image")).expect("image file");
        assert_eq!(marker["agentId"], "agent-1");
        assert_eq!(marker["imageInode"], image.ino());
        assert_eq!(row["mountIdentity"]["imageInode"], image.ino());
        assert_eq!(row["mountIdentity"]["mount"], row["mount"]);
        manager.ensure_mounted("agent-1").expect("same volume");
        let report = manager.check_mounts().expect("check");
        assert_eq!(report["ok"], true, "{report}");
        assert_eq!(report["agents"][0]["status"], "mounted");
    }

    #[test]
    fn plain_folder_at_the_mount_point_is_never_attached_over_or_deleted() {
        let fixture = tempfile::tempdir().expect("fixture");
        let manager = ready_workspaces(fixture.path(), &["agent-1"]);
        manager.archive_workspace("agent-1").expect("archive");
        let row = agent_row(&manager, "agent-1");
        let mount = PathBuf::from(row["mount"].as_str().expect("mount"));
        // A restore from a copy of a mounted volume leaves its files in a plain folder.
        fs::create_dir_all(mount.join("repo")).expect("restored folder");
        fs::write(mount.join("repo/work.txt"), "only copy").expect("restored file");
        for result in [
            manager.ensure_mounted("agent-1"),
            manager.archive_workspace("agent-1"),
            manager.remove_workspace("agent-1", true),
        ] {
            assert_eq!(mount_status(result.expect_err("refused")), "plain-folder");
        }
        assert!(Path::new(row["image"].as_str().expect("image")).exists());
        assert_eq!(
            fs::read_to_string(mount.join("repo/work.txt")).expect("kept"),
            "only copy"
        );
        let report = manager.check_mounts().expect("check");
        assert_eq!(report["ok"], false);
        assert_eq!(report["agents"][0]["status"], "plain-folder");
    }

    #[test]
    fn foreign_volume_at_the_mount_point_is_refused() {
        let fixture = tempfile::tempdir().expect("fixture");
        let manager = ready_workspaces(fixture.path(), &["agent-1"]);
        manager.archive_workspace("agent-1").expect("archive");
        let mount = manager.store.join("mnt/agent-1");
        attach_marked_volume(
            &manager,
            &mount,
            &fixture.path().join("other.asif"),
            "agent-2",
        );
        let error = manager
            .ensure_mounted("agent-1")
            .expect_err("foreign volume");
        assert!(error.to_string().contains("agent agent-2"), "{error}");
        assert_eq!(mount_status(error), "foreign-volume");
        assert_eq!(
            mount_status(manager.remove_workspace("agent-1", true).expect_err("kept")),
            "foreign-volume"
        );
        assert!(manager.store.join("agents/agent-1/workspace.asif").exists());
    }

    #[test]
    fn second_attachment_of_an_agent_volume_is_refused_and_reported() {
        let fixture = tempfile::tempdir().expect("fixture");
        let manager = ready_workspaces(fixture.path(), &["agent-1"]);
        manager.archive_workspace("agent-1").expect("archive");
        // The old state folder was moved away with its volume still attached.
        let moved = fixture.path().join("moved/workspaces");
        attach_marked_volume(
            &manager,
            &moved.join("mnt/agent-1"),
            &moved.join("agents/agent-1/workspace.asif"),
            "agent-1",
        );
        let error = manager
            .ensure_mounted("agent-1")
            .expect_err("second attach");
        assert!(
            error.to_string().contains("moved/workspaces/mnt/agent-1"),
            "{error}"
        );
        assert_eq!(mount_status(error), "attached-elsewhere");
        for result in [
            manager.archive_workspace("agent-1"),
            manager.remove_workspace("agent-1", true),
        ] {
            assert_eq!(
                mount_status(result.expect_err("kept")),
                "attached-elsewhere"
            );
        }
        assert!(manager.store.join("agents/agent-1/workspace.asif").exists());
        let report = manager.check_mounts().expect("check");
        assert_eq!(report["agents"][0]["status"], "attached-elsewhere");
        assert_eq!(report["strayAttachments"][0]["agentId"], "agent-1");
        mounts::record_startup_report(Ok(report));
        let summary = mount_diagnostics();
        assert_eq!(summary["state"], "checked");
        assert_eq!(
            summary["problems"],
            json!([{"agentId":"agent-1","status":"attached-elsewhere"}])
        );
        assert_eq!(summary["strayAttachments"], 1);
        assert!(!summary.to_string().contains("moved"), "{summary}");
    }

    #[test]
    fn unmarked_volume_whose_image_is_another_file_is_refused() {
        let fixture = tempfile::tempdir().expect("fixture");
        let manager = ready_workspaces(fixture.path(), &["agent-1"]);
        manager.archive_workspace("agent-1").expect("archive");
        let mount = manager.store.join("mnt/agent-1");
        let other = fixture.path().join("other.asif");
        fs::write(&other, "other image").expect("other image");
        manager
            .backend
            .mounted
            .lock()
            .expect("mounted volumes")
            .insert(mount, other);
        assert_eq!(
            mount_status(manager.ensure_mounted("agent-1").expect_err("unmarked")),
            "foreign-volume"
        );
    }

    #[test]
    fn replaced_image_file_is_refused_while_its_old_volume_is_attached() {
        let fixture = tempfile::tempdir().expect("fixture");
        let manager = ready_workspaces(fixture.path(), &["agent-1"]);
        let image = manager.store.join("agents/agent-1/workspace.asif");
        let restored = image.with_extension("restored");
        fs::copy(&image, &restored).expect("restored copy");
        fs::rename(&restored, &image).expect("restore over the image");
        for result in [
            manager.ensure_mounted("agent-1"),
            manager.archive_workspace("agent-1"),
        ] {
            assert_eq!(mount_status(result.expect_err("refused")), "image-replaced");
        }
        assert!(
            manager
                .backend
                .is_mounted(&manager.store.join("mnt/agent-1"))
                .expect("probe")
        );
    }

    #[test]
    fn repair_attaches_only_recorded_mounts_at_empty_mount_points() {
        let fixture = tempfile::tempdir().expect("fixture");
        let manager = ready_workspaces(fixture.path(), &["agent-1", "agent-2", "agent-3"]);
        // A backup step detached agent-1 and agent-3; agent.json still records them as mounted.
        for agent in ["agent-1", "agent-3"] {
            manager
                .backend
                .unmount_workspace(&manager.store.join("mnt").join(agent), false)
                .expect("detach");
        }
        fs::write(manager.store.join("mnt/agent-3/copy.txt"), "only copy").expect("plain file");
        manager.archive_workspace("agent-2").expect("archive");
        let check = manager.check_mounts().expect("check");
        let statuses = check["agents"]
            .as_array()
            .expect("agents")
            .iter()
            .map(|agent| (agent["status"].clone(), agent["repair"].clone()))
            .collect::<Vec<_>>();
        assert_eq!(
            statuses,
            vec![
                (json!("detached"), json!("attach")),
                (json!("detached"), Value::Null),
                (json!("plain-folder"), Value::Null),
            ]
        );
        let repaired = manager.repair_mounts().expect("repair");
        assert_eq!(
            repaired["actions"],
            json!([{"agentId":"agent-1","action":"attached"}])
        );
        let after = repaired["report"]["agents"].as_array().expect("agents");
        assert_eq!(after[0]["status"], "mounted");
        assert_eq!(after[1]["status"], "detached");
        assert_eq!(after[2]["status"], "plain-folder");
        assert_eq!(
            fs::read_to_string(manager.store.join("mnt/agent-3/copy.txt")).expect("kept"),
            "only copy"
        );
    }
}
