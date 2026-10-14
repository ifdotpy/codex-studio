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
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PLATFORM_REFUSAL: &str =
    "Image workspaces require macOS ASIF. Use a Git worktree on this server";
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
    fn remove_layer(&self, agent_dir: &Path) -> Result<(), ImageWorkspaceError>;
    fn remove_base_version(&self, path: &Path) -> Result<(), ImageWorkspaceError>;
    fn private_bytes(&self, path: &Path) -> Result<Option<u64>, ImageWorkspaceError>;
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
        let lock = FileLock::exclusive(&self.base_dir(&key).join(".build.lock"))?;
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
            drop(lock);
            if let Err(error) = self.spawn_base_build(root, key, version, Some(current)) {
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
            return Ok(status_view(&current));
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
        drop(lock);
        if let Err(error) = self.spawn_base_build(root, key.clone(), version, None) {
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
            .spawn(move || lifecycle.build_base(&root, &key, &version, refresh_from.as_ref()))
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
            let _unmount = self.backend.unmount_workspace(&mount, false);
            self.backend.remove_layer(&agent_dir)?;
            let _cleanup = fs::remove_dir_all(&mount);
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
        fs::create_dir_all(&mount)?;
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
        let image = state
            .get("image")
            .and_then(Value::as_str)
            .ok_or(ImageWorkspaceError::InvalidState)?;
        self.backend.mount_workspace(
            Path::new(image),
            &mount,
            state
                .get("baseImage")
                .and_then(Value::as_str)
                .map(Path::new),
        )?;
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
        let layer = state
            .get("image")
            .and_then(Value::as_str)
            .ok_or(ImageWorkspaceError::InvalidState)?;
        let base = state
            .get("baseImage")
            .and_then(Value::as_str)
            .map(PathBuf::from);
        self.backend
            .mount_workspace(Path::new(layer), &mount, base.as_deref())?;
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
        let mount = state
            .get("mount")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.store.join("mnt").join(agent_id));
        let unmount = self.backend.unmount_workspace(&mount, true)?;
        state["state"] = json!("archived");
        state["mounted"] = json!(false);
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
        let state = read_json(&state_path)?;
        if state.is_null() {
            return Ok(json!({"freedBytes":0,"state":"removed"}));
        }
        let _lock = FileLock::exclusive(&agent_dir.join(".workspace.lock"))?;
        let mount = state
            .get("mount")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.store.join("mnt").join(agent_id));
        let unmount = self.backend.unmount_workspace(&mount, true)?;
        self.backend.remove_layer(&agent_dir)?;
        let _cleanup = fs::remove_dir_all(&mount);
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
        let mut existing = self.store.as_path();
        while !existing.exists() {
            existing = existing.parent().ok_or(ImageWorkspaceError::InvalidState)?;
        }
        let stats = rustix::fs::statvfs(existing)
            .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
        let free = stats.f_bavail.saturating_mul(stats.f_frsize);
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
    let temp = path.with_extension(format!("{}.tmp", now_ns()));
    let mut file = File::create(&temp)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temp, path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
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

#[cfg(target_os = "macos")]
#[path = "image_workspace_macos.rs"]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::MacAsifBackend;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeBackend {
        store: PathBuf,
        fail_once: Mutex<Option<&'static str>>,
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
            fs::create_dir_all(mount.join("repo"))?;
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
        fn unmount_workspace(&self, _: &Path, _: bool) -> Result<Value, ImageWorkspaceError> {
            self.fail("unmount")?;
            Ok(json!({"method":"fake","terminatedPids":0}))
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

    fn wait_for_build(manager: &WorkspaceLifecycle<FakeBackend>, root: &Path) -> Value {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let status = manager.base_status(root).expect("base state");
            if status["state"] != "building" {
                return status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "base build completion"
            );
            thread::sleep(Duration::from_millis(5));
        }
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
        };
        let manager = WorkspaceLifecycle::with_backend(&store, backend);

        assert_eq!(
            manager.start_base_build(&project).expect("build start")["state"],
            "building"
        );
        assert_eq!(wait_for_build(&manager, &project)["state"], "failed");
        assert_eq!(
            read_json(&manager.base_state(&repo_key(&project))).expect("failed row")["error"],
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
        let base = read_json(&manager.base_state(&repo_key(&project))).expect("base row");
        let version = base["version"].as_str().expect("base version");
        let copied_file = store
            .join("bases")
            .join(repo_key(&project))
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
        let pending = manager.list_workspaces().expect("pending state");
        assert_eq!(pending[0]["state"], "creating");
        fs::write(project.join("file.txt"), "changed under same repo identity")
            .expect("mutated source");

        let ready = manager
            .create_workspace(&project, "agent-1")
            .expect("workspace replay");
        assert_eq!(
            manager.list_workspaces().expect("ready state")[0]["state"],
            "ready"
        );
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
}
