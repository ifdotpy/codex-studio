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
    backend: B,
}

impl<B: WorkspaceBackend> WorkspaceLifecycle<B> {
    /// Testable lifecycle constructor; callers provide an isolated store and backend.
    pub fn with_backend(store: impl Into<PathBuf>, backend: B) -> Self {
        Self {
            store: store.into(),
            backend,
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

    /// Synchronous equivalent of Python's asynchronous `start_base_build`.
    pub fn start_base_build(&self, root: &Path) -> Result<Value, ImageWorkspaceError> {
        self.start_base_build_with_retry(root, false)
    }

    fn start_base_build_with_retry(
        &self,
        root: &Path,
        retry_failed: bool,
    ) -> Result<Value, ImageWorkspaceError> {
        let root = root_path(root)?;
        self.backend.supported(&root)?;
        let key = repo_key(&root);
        let _lock = FileLock::exclusive(&self.base_dir(&key).join(".build.lock"))?;
        let state_path = self.base_state(&key);
        let current = read_json(&state_path)?;
        if current.get("state").and_then(Value::as_str) == Some("ready")
            && current
                .get("image")
                .and_then(Value::as_str)
                .is_some_and(|p| Path::new(p).exists())
        {
            return Ok(status_view(&current));
        }
        if current.get("state").and_then(Value::as_str) == Some("failed") && !retry_failed {
            let failed_at = current
                .get("failedAt")
                .and_then(Value::as_f64)
                .unwrap_or_default();
            if now_seconds() - failed_at < 300.0 {
                return Ok(status_view(&current));
            }
        }
        let version = format!("v-{}", now_ns());
        self.require_space(DEFAULT_BASE_MIN_FREE, BASE_MIN_FREE_ENV, "image base", None)?;
        let mut pending = json!({
            "schema":STATE_SCHEMA,"state":"building","repoRoot":root,
            "repoKey":key,"changeDetector":"git-v1","version":version,
            "error":null,"startedAt":now_seconds()
        });
        write_json(&state_path, &pending)?;
        let mut staging = None;
        let result: Result<Value, ImageWorkspaceError> = (|| {
            let mut stage = self.backend.open_base_staging(&root, &key, &version)?;
            stage.token = self.backend.current_event_id(&root)?;
            staging = Some(stage.clone());
            self.require_space(
                DEFAULT_BASE_MIN_FREE,
                BASE_MIN_FREE_ENV,
                "image base",
                Some(&stage),
            )?;
            let excludes = self.workspace_excludes(&root);
            let check = || {
                self.require_space(
                    DEFAULT_BASE_MIN_FREE,
                    BASE_MIN_FREE_ENV,
                    "image base",
                    Some(&stage),
                )
            };
            self.backend
                .copy_base_tree(&root, &stage.root, &excludes, &check)?;
            check()?;
            let repositories = git_capture(&root)?;
            let sealed = self.backend.seal_base(&stage)?;
            let mut ready = json!({
                "schema":STATE_SCHEMA,"state":"ready","repoRoot":root,"repoKey":key,
                "changeDetector":"git-v1","version":version,"error":null,
                "image":sealed.get("image").cloned().unwrap_or(Value::Null),
                "versionPath":sealed.get("versionPath").cloned().unwrap_or(json!(stage.version_path)),
                "token":stage.token,"excludes":excludes,"repositories":repositories,
                "createdAt":now_seconds()
            });
            let version_json = stage.version_path.join("version.json");
            write_json(&version_json, &ready)?;
            write_json(&state_path, &ready)?;
            if let Err(error) = self.prune_base_versions(&key) {
                ready["pruneError"] = json!(error.to_string());
                write_json(&state_path, &ready)?;
            }
            Ok(status_view(&ready))
        })();
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                if let Some(stage) = staging {
                    let _cleanup = self.backend.remove_base_version(&stage.version_path);
                }
                pending["state"] = json!("failed");
                pending["error"] = json!(error.to_string());
                pending["failedAt"] = json!(now_seconds());
                write_json(&state_path, &pending)?;
                Err(error)
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
        state["path"] = json!(workspace_root);
        state["mount"] = json!(mount);
        state["state"] = json!("ready");
        state["mounted"] = json!(true);
        state["repositories"] = json!(git_capture(&root)?);
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
        production().start_base_build_with_retry(root, retry_failed)
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
            repos.push(json!({"path":directory.strip_prefix(root).unwrap_or(&directory).to_string_lossy(),"head":head.map(|s|s.trim().to_owned()),"dirtyPaths":dirty_paths(&dirty),"indexFingerprint":index}));
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
    let identity = serde_json::to_vec(&json!([
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        mtime_ns,
        ctime_ns
    ]))
    .ok()?;
    Some(data_encoding::HEXLOWER.encode(&Sha256::digest(identity)))
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
        let absent = std::env::temp_dir().join(format!("image-workspace-absent-{}", now_ns()));
        let error = create_workspace(&absent, "agent-1").expect_err("ASIF is unsupported");
        assert_eq!(error.to_string(), PLATFORM_REFUSAL);
        assert!(!absent.exists());
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

        assert!(
            matches!(manager.start_base_build(&project), Err(ImageWorkspaceError::Backend(message)) if message == "failpoint:copy")
        );
        assert_eq!(
            manager.base_status(&project).expect("failed status")["state"],
            "failed"
        );
        assert_eq!(
            manager.start_base_build(&project).expect("cooldown status")["state"],
            "failed"
        );
        assert_eq!(
            manager
                .start_base_build_with_retry(&project, true)
                .expect("base retry")["state"],
            "ready"
        );
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
