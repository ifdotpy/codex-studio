//! Durable Linux VM project and line preparation shared by action and turn-start paths.

use crate::{
    linux_vm_guest::GuestRpc,
    native_actions::{self, NativeError},
    tar_gzip::TarGzipWriter,
    workspace_operations::ProviderTicket,
};
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, RuntimeStore};

const VM_UPLOAD_CHUNK_BYTES: usize = 256 * 1024;

pub struct VmProjectArchive {
    pub file: tempfile::NamedTempFile,
    pub bytes: u64,
    pub sha256: String,
}

struct VmProjectUploadError {
    proven_not_sent: bool,
    detail: String,
}

impl From<crate::linux_vm_guest::GuestError> for VmProjectUploadError {
    fn from(error: crate::linux_vm_guest::GuestError) -> Self {
        Self {
            proven_not_sent: false,
            detail: error.to_string(),
        }
    }
}

pub fn vm_project_archive(source: &Path) -> std::io::Result<VmProjectArchive> {
    let source = fs::canonicalize(source)?;
    if !source.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "VM project source is not a directory",
        ));
    }
    let exclusions = vm_archive_exclusions(&source)?;
    let identities = std::cell::RefCell::new(HashMap::new());
    let mut archive = tempfile::NamedTempFile::new()?;
    {
        let mut gzip = TarGzipWriter::new(archive.as_file_mut());
        let metadata = fs::symlink_metadata(&source)?;
        gzip.append_directory(".", vm_tar_mode(&metadata))?;
        identities
            .borrow_mut()
            .insert(source.clone(), vm_archive_identity(&metadata));
        add_vm_tar_directory(&mut gzip, &source, &source, &exclusions, &identities)?;
        let _archive_file = gzip.finish()?;
    }
    for (path, before) in identities.into_inner() {
        let after = fs::symlink_metadata(&path)?;
        if vm_archive_identity(&after) != before {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "VM project source changed while creating the archive",
            ));
        }
    }
    archive.as_file_mut().sync_all()?;
    let bytes = archive.as_file().metadata()?.len();
    let mut digest = Sha256::new();
    let mut reader = File::open(archive.path())?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(VmProjectArchive {
        file: archive,
        bytes,
        sha256: data_encoding::HEXLOWER.encode(&digest.finalize()),
    })
}

fn add_vm_tar_directory<W: Write>(
    output: &mut TarGzipWriter<W>,
    root: &Path,
    directory: &Path,
    exclusions: &VmArchiveExclusions,
    identities: &std::cell::RefCell<HashMap<std::path::PathBuf, VmArchiveIdentity>>,
) -> std::io::Result<()> {
    let mut children = fs::read_dir(directory)?.collect::<std::io::Result<Vec<_>>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        let path = entry.path();
        let relative = path.strip_prefix(root).map_err(std::io::Error::other)?;
        let name = relative.to_string_lossy().replace('\\', "/");
        if vm_archive_skip(root, relative, &path, exclusions) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        identities
            .borrow_mut()
            .insert(path.clone(), vm_archive_identity(&metadata));
        let kind = metadata.file_type();
        if kind.is_symlink() {
            let target = fs::read_link(&path)?.to_string_lossy().into_owned();
            output.append_symlink(&name, vm_tar_mode(&metadata), &target)?;
        } else if kind.is_dir() {
            output.append_directory(&name, vm_tar_mode(&metadata))?;
            add_vm_tar_directory(output, root, &path, exclusions, identities)?;
        } else if kind.is_file() {
            let mut file = File::open(&path)?;
            output.append_file(&name, vm_tar_mode(&metadata), metadata.len(), &mut file)?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "VM project source contains a special file",
            ));
        }
    }
    Ok(())
}

type VmArchiveIdentity = (u64, u64, u64, i128, i128);

fn vm_archive_identity(metadata: &fs::Metadata) -> VmArchiveIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (
            metadata.dev(),
            metadata.ino(),
            metadata.size(),
            i128::from(metadata.mtime()) * 1_000_000_000 + i128::from(metadata.mtime_nsec()),
            i128::from(metadata.ctime()) * 1_000_000_000 + i128::from(metadata.ctime_nsec()),
        )
    }
    #[cfg(not(unix))]
    {
        (0, 0, metadata.len(), 0, 0)
    }
}

struct VmArchiveExclusions {
    ignored: HashSet<String>,
    worktrees: HashSet<String>,
}

fn vm_archive_exclusions(root: &Path) -> std::io::Result<VmArchiveExclusions> {
    let mut repositories = HashSet::new();
    let mut worktrees = HashSet::new();
    let mut ignored = HashSet::new();
    let root_modules = root.join(".git/modules");
    validate_vm_git_modules(&root_modules)?;
    let mut module_roots = vec![root_modules];
    let configured_store = std::env::var_os("CODEX_WORKSPACE_STORE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_default()
                .join(".local/state/codex-agents/workspaces")
        });
    if let Ok(relative) = configured_store.strip_prefix(root) {
        ignored.insert(relative.to_string_lossy().replace('\\', "/"));
    }
    // A root .gitignore still controls non-repository sources, as it does in
    // Python's _code_only_skips. Load it before traversal can inspect a stale
    // .git pointer inside one of those ignored trees.
    // Python only uses a temporary repository to interpret .gitignore when
    // the source itself is not a repository (_code_only_skips, lines 140-160).
    // In a real repo this would classify force-added ignored paths as skipped.
    let root_probe = Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap_or_default(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output()?;
    let root_is_repository = root_probe.status.success()
        && Path::new(String::from_utf8_lossy(&root_probe.stdout).trim()) == root;
    if !root_is_repository && root.join(".gitignore").is_file() {
        let temporary = tempfile::tempdir()?;
        let init = Command::new("git")
            .args([
                "init",
                "-q",
                "--bare",
                temporary.path().to_str().unwrap_or_default(),
            ])
            .output()?;
        if init.status.success() {
            let output = Command::new("git")
                .args([
                    "--git-dir",
                    temporary.path().to_str().unwrap_or_default(),
                    "--work-tree",
                    root.to_str().unwrap_or_default(),
                    "ls-files",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                    "--directory",
                    "-z",
                ])
                .output()?;
            for item in output
                .stdout
                .split(|byte| *byte == 0)
                .filter(|item| item.ends_with(b"/"))
            {
                let relative = String::from_utf8_lossy(&item[..item.len() - 1]).replace('\\', "/");
                if !root.join(&relative).join(".git").exists() {
                    ignored.insert(relative);
                }
            }
        }
    }
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        if directory != root {
            let relative = directory.strip_prefix(root).unwrap_or(Path::new("."));
            let exclusions = VmArchiveExclusions {
                ignored: ignored.clone(),
                worktrees: worktrees.clone(),
            };
            if vm_archive_skip(root, relative, &directory, &exclusions) {
                continue;
            }
        }
        let marker = directory.join(".git");
        if marker.is_dir() && !fs::symlink_metadata(&marker)?.file_type().is_symlink() {
            let probe = Command::new("git")
                .args([
                    "-C",
                    directory.to_str().unwrap_or_default(),
                    "rev-parse",
                    "--show-toplevel",
                ])
                .output()?;
            if probe.status.success()
                && Path::new(String::from_utf8_lossy(&probe.stdout).trim()) == directory
            {
                let relative = directory
                    .strip_prefix(root)
                    .unwrap_or(Path::new("."))
                    .to_string_lossy()
                    .replace('\\', "/");
                let repo = if relative.is_empty() {
                    ".".to_owned()
                } else {
                    relative
                };
                repositories.insert(repo.clone());
                let modules = directory.join(".git/modules");
                validate_vm_git_modules(&modules)?;
                module_roots.push(modules);
                collect_vm_ignored_directories(root, &directory, &repo, &mut ignored)?;
            }
        } else if fs::symlink_metadata(&marker)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            if directory == root {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "VM project root has linked Git metadata",
                ));
            }
            worktrees.insert(
                directory
                    .strip_prefix(root)
                    .unwrap_or(Path::new("."))
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        } else if marker.is_file() {
            if directory == root {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "VM project root is a linked Git worktree",
                ));
            }
            let target = read_gitdir_pointer(&directory, &marker)?;
            if target.starts_with(root.join(".git/worktrees")) {
                worktrees.insert(
                    directory
                        .strip_prefix(root)
                        .unwrap_or(Path::new("."))
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
                continue;
            }
            // Python recognizes submodule stores under the containing repo's
            // .git/modules, including nested repositories (codex_linux_workspace_sync.py:51-74).
            let internal_submodule = module_roots
                .iter()
                .any(|modules| target.starts_with(modules));
            if !internal_submodule {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "VM project contains linked Git metadata without its object store",
                ));
            }
            let relative = directory
                .strip_prefix(root)
                .unwrap_or(Path::new("."))
                .to_string_lossy()
                .replace('\\', "/");
            let repo = if relative.is_empty() {
                ".".to_owned()
            } else {
                relative
            };
            repositories.insert(repo.clone());
            collect_vm_ignored_directories(root, &directory, &repo, &mut ignored)?;
        }
        let mut children = fs::read_dir(&directory)?.collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children.into_iter().rev() {
            let metadata = fs::symlink_metadata(child.path())?;
            if metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && child.file_name() != ".git"
            {
                pending.push(child.path());
            }
        }
    }

    let mut repo_list = repositories.iter().cloned().collect::<Vec<_>>();
    repo_list.sort();
    for repo in &repo_list {
        let directory = if repo == "." {
            root.to_path_buf()
        } else {
            root.join(repo)
        };
        let output = Command::new("git")
            .args([
                "-C",
                directory.to_str().unwrap_or_default(),
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "--directory",
                "-z",
            ])
            .output()?;
        if !output.status.success() {
            continue;
        }
        for item in output
            .stdout
            .split(|byte| *byte == 0)
            .filter(|item| item.ends_with(b"/"))
        {
            let child = String::from_utf8_lossy(&item[..item.len() - 1]).replace('\\', "/");
            let full = if repo == "." {
                child
            } else {
                format!("{repo}/{child}")
            };
            if !repositories.iter().any(|candidate| candidate == &full) {
                ignored.insert(full);
            }
        }
    }
    let exclusions = VmArchiveExclusions { ignored, worktrees };
    for repository in &repositories {
        let relative = Path::new(repository);
        let directory = root.join(relative);
        if vm_archive_excluded_by_ancestor(root, relative, &exclusions) {
            let marker = directory.join(".git");
            if fs::symlink_metadata(&marker)
                .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
            {
                let git_dir = read_gitdir_pointer(&directory, &marker)?;
                if vm_internal_git_modules(root, &git_dir)
                    && let Ok(git_relative) = git_dir.strip_prefix(root)
                    && !vm_archive_excluded_by_ancestor(root, git_relative, &exclusions)
                {
                    validate_vm_git_object_store(&git_dir)?;
                }
            }
            continue;
        }
        validate_vm_repository_objects(root, &directory)?;
    }
    Ok(exclusions)
}

fn collect_vm_ignored_directories(
    root: &Path,
    directory: &Path,
    repository: &str,
    ignored: &mut HashSet<String>,
) -> std::io::Result<()> {
    let output = Command::new("git")
        .args([
            "-C",
            directory.to_str().unwrap_or_default(),
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "--directory",
            "-z",
        ])
        .output()?;
    if !output.status.success() {
        return Ok(());
    }
    for item in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|item| item.ends_with(b"/"))
    {
        let child = String::from_utf8_lossy(&item[..item.len() - 1]).replace('\\', "/");
        let full = if repository == "." {
            child
        } else {
            format!("{repository}/{child}")
        };
        if !root.join(&full).join(".git").exists() {
            ignored.insert(full);
        }
    }
    Ok(())
}

fn vm_archive_excluded_by_ancestor(
    root: &Path,
    relative: &Path,
    exclusions: &VmArchiveExclusions,
) -> bool {
    let mut current_relative = std::path::PathBuf::new();
    let mut current_path = root.to_path_buf();
    for component in relative.components() {
        if component == std::path::Component::CurDir {
            continue;
        }
        current_relative.push(component.as_os_str());
        current_path.push(component.as_os_str());
        if vm_archive_skip(root, &current_relative, &current_path, exclusions) {
            return true;
        }
    }
    false
}

fn read_gitdir_pointer(directory: &Path, marker: &Path) -> std::io::Result<std::path::PathBuf> {
    let contents = fs::read_to_string(marker)?;
    let line = contents
        .strip_suffix("\r\n")
        .or_else(|| contents.strip_suffix('\n'))
        .or_else(|| contents.strip_suffix('\r'))
        .unwrap_or(&contents);
    if line.contains(['\n', '\r']) || line.trim() != line {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "VM source has malformed Git pointer metadata",
        ));
    }
    let target = line
        .strip_prefix("gitdir: ")
        .filter(|target| !target.is_empty() && target.trim() == *target)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "VM source has malformed Git pointer metadata",
            )
        })?;
    fs::canonicalize(directory.join(target))
}

fn validate_vm_repository_objects(root: &Path, directory: &Path) -> std::io::Result<()> {
    let marker = directory.join(".git");
    let marker_metadata = fs::symlink_metadata(&marker)?;
    let git_dir = if marker_metadata.is_dir() && !marker_metadata.file_type().is_symlink() {
        fs::canonicalize(&marker)?
    } else if marker_metadata.is_file() && !marker_metadata.file_type().is_symlink() {
        let target = read_gitdir_pointer(directory, &marker)?;
        if !vm_internal_git_modules(root, &target) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "VM source repository has external Git metadata",
            ));
        }
        target
    } else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "VM source repository has external Git metadata",
        ));
    };
    validate_vm_git_object_store(&git_dir)
}

fn vm_internal_git_modules(root: &Path, target: &Path) -> bool {
    let Ok(relative) = target.strip_prefix(root) else {
        return false;
    };
    let parts = relative
        .components()
        .filter_map(|part| part.as_os_str().to_str())
        .collect::<Vec<_>>();
    parts
        .windows(3)
        .any(|parts| parts[0] == ".git" && parts[1] == "modules")
}

fn validate_vm_git_object_store(git_dir: &Path) -> std::io::Result<()> {
    let objects = git_dir.join("objects");
    let object_metadata = fs::symlink_metadata(&objects)?;
    if !object_metadata.is_dir() || object_metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "VM source repository has an external Git object store",
        ));
    }
    let canonical_objects = fs::canonicalize(&objects)?;
    if !canonical_objects.starts_with(git_dir) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "VM source repository has an external Git object store",
        ));
    }
    let alternates = objects.join("info/alternates");
    if fs::symlink_metadata(&alternates).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "VM source repository uses external Git object alternates",
        ));
    }
    let mut pending = vec![canonical_objects];
    while let Some(path) = pending.pop() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "VM source Git object store contains a symlink",
                ));
            }
            if metadata.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

fn validate_vm_git_modules(modules: &Path) -> std::io::Result<()> {
    if !modules.is_dir() {
        return Ok(());
    }
    let mut pending = vec![modules.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let metadata = fs::symlink_metadata(&path)?;
            if entry.file_name() == "objects" {
                let git_dir = path.parent().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "VM Git module object store has no metadata directory",
                    )
                })?;
                validate_vm_git_object_store(git_dir)?;
                continue;
            }
            if metadata.file_type().is_symlink() {
                continue;
            }
            if !metadata.is_dir() {
                continue;
            }
            pending.push(path);
        }
    }
    Ok(())
}

fn vm_archive_skip(
    _root: &Path,
    relative: &Path,
    path: &Path,
    exclusions: &VmArchiveExclusions,
) -> bool {
    let parts = relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy());
    let parts = parts.collect::<Vec<_>>();
    let normalized = relative.to_string_lossy().replace('\\', "/");
    if parts.contains(&".worktrees".into())
        || exclusions.worktrees.contains(&normalized)
        || normalized == "data/downloads"
        || exclusions
            .ignored
            .iter()
            .any(|item| normalized == *item || normalized.starts_with(&format!("{item}/")))
    {
        return true;
    }
    if parts.contains(&".git".into()) {
        return false;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    if matches!(
        name.as_ref(),
        "node_modules"
            | ".venv"
            | "venv"
            | "__pycache__"
            | ".pytest_cache"
            | ".mypy_cache"
            | ".ruff_cache"
            | ".turbo"
            | ".next"
    ) {
        return true;
    }
    name == "target"
        && path
            .parent()
            .is_some_and(|parent| parent.join("Cargo.toml").is_file())
}

pub fn vm_guest_request_id(prefix: &str, identity: &str) -> String {
    format!(
        "{prefix}:{}",
        studio_runtime_store::sha256_hex(identity.as_bytes())
    )
}

fn vm_tar_mode(metadata: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        if metadata.permissions().readonly() {
            0o444
        } else {
            0o666
        }
    }
}

pub struct Preparation<'a> {
    store: &'a RuntimeStore,
    guest: Option<Arc<dyn GuestRpc>>,
    expected_lead: bool,
}

async fn guest_call(
    guest: Arc<dyn GuestRpc>,
    method: String,
    params: Value,
    request_id: Option<String>,
    timeout: std::time::Duration,
) -> Result<Value, crate::linux_vm_guest::GuestError> {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/vm-guest-rpc",
        move || guest.call(&method, &params, request_id.as_deref(), timeout),
    )
    .await
    .unwrap_or_else(|_| {
        Err(crate::linux_vm_guest::GuestError::Transport {
            message: "guest task outcome unknown".into(),
            uncertain: true,
        })
    })
}

impl Preparation<'_> {
    async fn prepare_vm_workspace(
        &self,
        request: &native_actions::Request,
        ticket: &crate::workspace_operations::ProviderTicket,
    ) -> native_actions::Result<()> {
        let request = request.clone();
        let ticket = ticket.clone();
        let (agent, parent) = {
            let connection = self.store.read()?;
            let agent = AgentRepository::get(&connection, &AgentId::from(request.agent.as_str()))?
                .ok_or(NativeError::Changed)?
                .value();
            if agent.get("executionMode").and_then(Value::as_str) != Some("vm") {
                return Ok(());
            }
            let parent = if agent.get("isLead").and_then(Value::as_bool) == Some(true) {
                None
            } else {
                let parent_id = agent
                    .get("parentId")
                    .and_then(Value::as_str)
                    .ok_or(NativeError::Invalid("vm-workspace/parent"))?;
                Some(
                    AgentRepository::get(&connection, &AgentId::from(parent_id))?
                        .ok_or(NativeError::Changed)?
                        .value(),
                )
            };
            (agent, parent)
        };
        let guest = self
            .guest
            .as_ref()
            .ok_or(NativeError::Unavailable("workspace-state/vm-workspace"))?;
        if agent.get("layrReady").and_then(Value::as_bool) == Some(true) {
            let guest_for_verify = Arc::clone(guest);
            let agent_for_verify = agent.clone();
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/vm-verify-connection",
                move || Self::verify_vm_connection(guest_for_verify.as_ref(), &agent_for_verify),
            )
            .await
            .map_err(|_| NativeError::Unknown)??;
            return Ok(());
        }
        let is_lead = agent.get("isLead").and_then(Value::as_bool) == Some(true);
        if is_lead != self.expected_lead {
            return Err(NativeError::Changed);
        }
        let mut project_import = None;
        let (method, request_id, params) = if is_lead {
            let source = agent
                .get("hostProjectPath")
                .and_then(Value::as_str)
                .or_else(|| agent.get("cwd").and_then(Value::as_str))
                .ok_or(NativeError::Invalid("vm-workspace/project"))?;
            let source_identity = agent
                .get("projectId")
                .and_then(Value::as_str)
                .unwrap_or(source);
            let project_id = agent
                .get("layrProjectId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| studio_runtime_store::sha256_hex(source_identity.as_bytes()));
            let import_request_id = format!("layr-project:{project_id}");
            let import_operation = studio_runtime_store::sha256_hex(import_request_id.as_bytes());
            let upload_root =
                format!("/var/lib/codex-studio/projects/layr-import-{import_operation}");
            project_import = Some(serde_json::json!({
                "projectId":project_id,"source":source,"requestId":import_request_id,
                "operation":import_operation,"uploadId":import_operation,
                "root":upload_root,"phase":"planned"
            }));
            (
                "line.bind",
                vm_guest_request_id("layr-bind", &request.agent),
                serde_json::json!({"projectId":project_id,"agentId":request.agent,"line":"main"}),
            )
        } else {
            let parent = parent.as_ref().ok_or(NativeError::Changed)?;
            if parent.get("layrReady").and_then(Value::as_bool) != Some(true) {
                return Err(NativeError::Unavailable("workspace-state/vm-workspace"));
            }
            let project_id = parent
                .get("layrProjectId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or(NativeError::Changed)?;
            let mut params = serde_json::json!({
                "projectId":project_id,"agentId":request.agent,"parentAgentId":parent["id"],
                "readOnly":agent.get("role").and_then(Value::as_str)==Some("reviewer"),
                "subpath":agent.get("layrSubpath").and_then(Value::as_str).unwrap_or("."),
            });
            if let Some(state_id) = agent.get("layrBaseState").filter(|value| !value.is_null()) {
                params["stateId"] = state_id.clone();
            }
            (
                "line.branch",
                vm_guest_request_id("layr-branch", &request.agent),
                params,
            )
        };

        // A durable uncertain attempt must be refused before probing the guest
        // or rebuilding its archive. Normal turn startup derives this request id
        // from the attempt when actionRequestId is absent.
        {
            let connection = self.store.read()?;
            let current =
                AgentRepository::get(&connection, &AgentId::from(request.agent.as_str()))?
                    .ok_or(NativeError::Changed)?
                    .value();
            if current["startAttempt"]["id"] != ticket.operation
                || current.get("epoch").and_then(Value::as_u64) != Some(ticket.epoch)
                || current
                    .get("deletedAt")
                    .is_some_and(|value| !value.is_null())
            {
                return Err(NativeError::Changed);
            }
            if let Some(action_request_id) = current["startAttempt"]
                .get("actionRequestId")
                .and_then(Value::as_str)
                && action_request_id != request.request_id
            {
                return Err(NativeError::Changed);
            }
            if let Some(existing) = current["startAttempt"].get("vmWorkspacePreparation") {
                if existing.get("requestId").and_then(Value::as_str) != Some(request_id.as_str())
                    || existing.get("method").and_then(Value::as_str) != Some(method)
                    || existing.get("params") != Some(&params)
                {
                    return Err(NativeError::Changed);
                }
                if existing.get("phase").and_then(Value::as_str) != Some("not_sent") {
                    return Err(NativeError::Unknown);
                }
            }
        }

        // Match Python's project.ensure-first import flow: a completed guest project
        // is authoritative for this stable project identity, so a replay must not
        // construct a new archive or collide with the prior upload.begin receipt.
        let mut reusable_project = None;
        let mut archive = None;
        if let Some(import) = project_import.as_ref() {
            let project_id = import["projectId"]
                .as_str()
                .ok_or(NativeError::Invalid("vm-workspace/project"))?;
            match guest_call(
                Arc::clone(guest),
                "project.ensure".into(),
                serde_json::json!({"projectId":project_id}),
                None,
                std::time::Duration::from_secs(30),
            )
            .await
            {
                Ok(project)
                    if project.get("projectId").and_then(Value::as_str) == Some(project_id) =>
                {
                    reusable_project = Some(project)
                }
                Ok(_) => return Err(NativeError::Changed),
                Err(error) if error.code() == Some("not_found") => {
                    let source = PathBuf::from(
                        import["source"]
                            .as_str()
                            .ok_or(NativeError::Invalid("vm-workspace/project"))?,
                    );
                    archive = Some(
                        studio_runtime_process::blocking_pool::spawn_blocking(
                            "runtime-services/vm-project-archive",
                            move || vm_project_archive(&source),
                        )
                        .await
                        .map_err(|_| NativeError::Unavailable("workspace-state/vm-workspace"))?
                        .map_err(|_| NativeError::Unavailable("workspace-state/vm-workspace"))?,
                    );
                }
                Err(error) => {
                    return Err(if error.uncertain() {
                        NativeError::Unknown
                    } else {
                        NativeError::Unavailable("workspace-state/vm-workspace")
                    });
                }
            }
        }

        let phase_agent = request.agent.clone();
        let phase_epoch = ticket.epoch;
        let phase_operation = ticket.operation.clone();
        let phase_action_request = request.request_id.clone();
        let phase_request_id = request_id.clone();
        let phase_method = method.to_owned();
        let phase_params = params.clone();
        let phase_import = project_import.clone();
        let phase_archive = archive
            .as_ref()
            .map(|archive| (archive.bytes, archive.sha256.clone()));
        let had_reusable_project = reusable_project.is_some();
        let phase = self.store.transaction_with_error(move |tx| {
            let mut current = AgentRepository::get(tx, &AgentId::from(phase_agent.as_str()))?
                .ok_or(NativeError::Changed)?
                .value();
            if current["startAttempt"]["id"] != phase_operation
                || current.get("epoch").and_then(Value::as_u64) != Some(phase_epoch)
                || current
                    .get("deletedAt")
                    .is_some_and(|value| !value.is_null())
            {
                return Err(NativeError::Changed);
            }
            if let Some(action_request_id) = current["startAttempt"]
                .get("actionRequestId")
                .and_then(Value::as_str)
                && action_request_id != phase_action_request
            {
                return Err(NativeError::Changed);
            }
            let existing = current["startAttempt"]
                .get("vmWorkspacePreparation")
                .cloned();
            if let Some(existing) = existing {
                if existing.get("requestId").and_then(Value::as_str) != Some(phase_request_id.as_str())
                    || existing.get("method").and_then(Value::as_str) != Some(phase_method.as_str())
                    || existing.get("params") != Some(&phase_params)
                {
                    return Err(NativeError::Changed);
                }
                if existing.get("phase").and_then(Value::as_str) != Some("not_sent") {
                    return Ok(existing
                        .get("phase")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned());
                }
                match (&phase_import, &phase_archive) {
                    (Some(import), Some((archive_bytes, archive_sha))) => {
                        let prior_import =
                            existing.get("projectImport").ok_or(NativeError::Changed)?;
                        if prior_import.get("requestId") != import.get("requestId")
                            || prior_import.get("archiveBytes").and_then(Value::as_u64)
                                != Some(*archive_bytes)
                            || prior_import.get("archiveSha256").and_then(Value::as_str)
                                != Some(archive_sha.as_str())
                        {
                            return Err(NativeError::Changed);
                        }
                    }
                    (Some(_), None) if had_reusable_project => {}
                    (None, None) => {}
                    _ => return Err(NativeError::Changed),
                }
                current["startAttempt"]["vmWorkspacePreparation"]["phase"] =
                    serde_json::json!("submitted");
                current["startAttempt"]["vmWorkspacePreparation"]["error"] = Value::Null;
                if current["startAttempt"]["vmWorkspacePreparation"]
                    .get("projectImport")
                    .is_some()
                {
                    current["startAttempt"]["vmWorkspacePreparation"]["projectImport"]["phase"] =
                        serde_json::json!("planned");
                }
                AgentRepository::put(tx, &AgentRecord::from_value(current)?)?;
                return Ok("new".to_owned());
            }
            current["startAttempt"]["vmWorkspacePreparation"] = serde_json::json!({
                "requestId":phase_request_id,"method":phase_method,"params":phase_params,"phase":"submitted"
            });
            if let Some(import) = phase_import.as_ref() {
                current["startAttempt"]["vmWorkspacePreparation"]["projectImport"] = import.clone();
            }
            AgentRepository::put(tx, &AgentRecord::from_value(current)?)?;
            Ok("new".to_owned())
        }).await?;
        if phase != "new" {
            return Err(NativeError::Unknown);
        }
        let mut bind_params = params;
        if let Some(import) = project_import.as_ref() {
            let imported =
                if let Some(existing) = reusable_project {
                    existing
                } else {
                    let archive_ref = archive.as_ref().ok_or(NativeError::Changed)?;
                    let import_record = import.clone();
                    let operation = ticket.operation.clone();
                    let agent_id = request.agent.clone();
                    let archive_bytes = archive_ref.bytes;
                    let archive_sha = archive_ref.sha256.clone();
                    self.store.transaction_with_error(move |tx| {
                    let mut current =
                        AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                            .ok_or(NativeError::Changed)?
                            .value();
                    if current["startAttempt"]["id"] != operation {
                        return Err(NativeError::Changed);
                    }
                    let saved =
                        &mut current["startAttempt"]["vmWorkspacePreparation"]["projectImport"];
                    if saved["requestId"] != import_record["requestId"] {
                        return Err(NativeError::Changed);
                    }
                    saved["phase"] = serde_json::json!("uploading");
                    saved["archiveBytes"] = serde_json::json!(archive_bytes);
                    saved["archiveSha256"] = serde_json::json!(archive_sha);
                    AgentRepository::put(tx, &AgentRecord::from_value(current)?)?;
                    Ok(())
                }).await?;
                    let guest_for_upload = Arc::clone(guest);
                    let import = import.clone();
                    let archive = archive.take().ok_or(NativeError::Changed)?;
                    let imported_result = studio_runtime_process::blocking_pool::spawn_blocking(
                        "runtime-services/vm-project-upload",
                        move || {
                            Self::upload_vm_project(guest_for_upload.as_ref(), &import, &archive)
                        },
                    )
                    .await;
                    let imported_result = match imported_result {
                        Ok(result) => result,
                        Err(_) => Err(VmProjectUploadError {
                            proven_not_sent: false,
                            detail: "guest task outcome unknown".into(),
                        }),
                    };
                    match imported_result {
                        Ok(imported) => imported,
                        Err(error) => {
                            let not_sent = error.proven_not_sent;
                            let detail = error.detail;
                            let agent_id = request.agent.clone();
                            let operation = ticket.operation.clone();
                            self.store.transaction_with_error(move |tx| {
                            let mut current = AgentRepository::get(
                                tx,
                                &AgentId::from(agent_id.as_str()),
                            )?
                            .ok_or(NativeError::Changed)?
                            .value();
                            if current["startAttempt"]["id"] != operation {
                                return Err(NativeError::Changed);
                            }
                            current["startAttempt"]["vmWorkspacePreparation"]["phase"] =
                                serde_json::json!(if not_sent { "not_sent" } else { "unknown" });
                            current["startAttempt"]["vmWorkspacePreparation"]["projectImport"][
                                "phase"
                            ] = serde_json::json!(if not_sent { "not_sent" } else { "unknown" });
                            current["startAttempt"]["vmWorkspacePreparation"]["error"] =
                                serde_json::json!(detail);
                            AgentRepository::put(tx, &AgentRecord::from_value(current)?)?;
                            Ok(())
                        }).await?;
                            return Err(if not_sent {
                                NativeError::Unavailable("workspace-state/vm-workspace")
                            } else {
                                NativeError::Unknown
                            });
                        }
                    }
                };
            let state_dir = self.store.state_dir().to_path_buf();
            let project_id = import["projectId"]
                .as_str()
                .ok_or(NativeError::Invalid("vm-workspace/project"))?
                .to_owned();
            let source = import["source"]
                .as_str()
                .ok_or(NativeError::Invalid("vm-workspace/project"))?
                .to_owned();
            let import_state_id = imported
                .get("stateId")
                .and_then(Value::as_str)
                .map(str::to_owned);
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/vm-project-registration",
                move || {
                    write_vm_project_registration(
                        &state_dir,
                        &project_id,
                        &source,
                        import_state_id.as_deref(),
                    )
                },
            )
            .await
            .map_err(|_| NativeError::Unavailable("workspace-state/vm-project-registration"))??;
            let operation = ticket.operation.clone();
            let agent_id = request.agent.clone();
            self.store
                .transaction_with_error(move |tx| {
                    let mut current = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .ok_or(NativeError::Changed)?
                        .value();
                    if current["startAttempt"]["id"] != operation {
                        return Err(NativeError::Changed);
                    }
                    current["startAttempt"]["vmWorkspacePreparation"]["projectImport"]["phase"] =
                        serde_json::json!("imported");
                    AgentRepository::put(tx, &AgentRecord::from_value(current)?)?;
                    Ok(())
                })
                .await?;
            bind_params = serde_json::json!({
                "projectId":imported["projectId"],"agentId":request.agent,"line":"main"
            });
        }
        let result = match guest_call(
            Arc::clone(guest),
            method.to_owned(),
            bind_params,
            Some(request_id.clone()),
            std::time::Duration::from_secs(30),
        )
        .await
        {
            Ok(value) => value,
            Err(error) => {
                let uncertain = error.uncertain();
                let error_text = error.to_string();
                let agent_id = request.agent.clone();
                let operation = ticket.operation.clone();
                self.store
                    .transaction_with_error(move |tx| {
                        let mut current =
                            AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                                .ok_or(NativeError::Changed)?
                                .value();
                        if current["startAttempt"]["id"] != operation {
                            return Err(NativeError::Changed);
                        }
                        current["startAttempt"]["vmWorkspacePreparation"]["phase"] =
                            serde_json::json!(if uncertain { "unknown" } else { "failed" });
                        current["startAttempt"]["vmWorkspacePreparation"]["error"] =
                            serde_json::json!(error_text);
                        AgentRepository::put(tx, &AgentRecord::from_value(current)?)?;
                        Ok(())
                    })
                    .await?;
                return Err(if uncertain {
                    NativeError::Unknown
                } else {
                    NativeError::Unavailable("workspace-state/vm-workspace")
                });
            }
        };
        let fields = ["cwd", "projectId", "line", "owner", "home"];
        if fields.iter().any(|field| {
            result
                .get(field)
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        }) {
            return Err(NativeError::Unavailable("workspace-state/vm-workspace"));
        }
        let connected_project_id = result["projectId"]
            .as_str()
            .expect("validated VM project identifier")
            .to_owned();
        let connected_line = result["line"]
            .as_str()
            .expect("validated VM project line")
            .to_owned();
        let agent_id = request.agent.clone();
        let operation = ticket.operation.clone();
        self.store
            .transaction_with_error(move |tx| {
                let mut current = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                    .ok_or(NativeError::Changed)?
                    .value();
                if current["startAttempt"]["id"] != operation
                    || current
                        .get("deletedAt")
                        .is_some_and(|value| !value.is_null())
                {
                    return Err(NativeError::Changed);
                }
                for (source, target) in [
                    ("cwd", "cwd"),
                    ("projectId", "layrProjectId"),
                    ("line", "layrLine"),
                    ("owner", "layrOwner"),
                    ("home", "layrHome"),
                    ("stateId", "layrStateId"),
                ] {
                    current[target] = result[source].clone();
                }
                // A reviewer line of a current guest has a private writable folder.
                if let Some(scratch) = result.get("scratch").filter(|value| value.is_string()) {
                    current["layrScratch"] = scratch.clone();
                }
                current["environment"] = serde_json::json!("linux");
                current["worktree"] = serde_json::json!(false);
                current["layrReady"] = serde_json::json!(true);
                current["startAttempt"]["vmWorkspacePreparation"]["phase"] =
                    serde_json::json!("completed");
                AgentRepository::put(tx, &AgentRecord::from_value(current)?)?;
                Ok(())
            })
            .await?;
        let mut connected = agent;
        connected["layrProjectId"] = serde_json::json!(connected_project_id);
        connected["layrLine"] = serde_json::json!(connected_line);
        Self::verify_vm_connection(guest.as_ref(), &connected)?;
        Ok(())
    }

    fn verify_vm_connection(guest: &dyn GuestRpc, agent: &Value) -> native_actions::Result<()> {
        let agent_id = agent
            .get("id")
            .and_then(Value::as_str)
            .ok_or(NativeError::Invalid("vm-workspace/agent"))?;
        guest
            .call(
                "health",
                &serde_json::json!({}),
                None,
                std::time::Duration::from_secs(15),
            )
            .map_err(|error| {
                if error.uncertain() {
                    NativeError::Unknown
                } else {
                    NativeError::Unavailable("workspace-state/vm-workspace")
                }
            })?;
        let context = guest
            .call(
                "agent.context",
                &serde_json::json!({"agentId":agent_id}),
                Some(&vm_guest_request_id("layr-context", agent_id)),
                std::time::Duration::from_secs(20),
            )
            .map_err(|error| {
                if error.uncertain() {
                    NativeError::Unknown
                } else {
                    NativeError::Unavailable("workspace-state/vm-workspace")
                }
            })?;
        if context.get("projectId")
            != agent
                .get("layrProjectId")
                .or_else(|| agent.get("projectId"))
            || context.get("line") != agent.get("layrLine").or_else(|| agent.get("line"))
        {
            return Err(NativeError::Changed);
        }
        Ok(())
    }

    fn upload_vm_project(
        guest: &dyn GuestRpc,
        intent: &Value,
        archive: &VmProjectArchive,
    ) -> Result<Value, VmProjectUploadError> {
        use crate::linux_vm_guest::GuestError;

        let field = |name| {
            intent
                .get(name)
                .and_then(Value::as_str)
                .ok_or(GuestError::InvalidRequest)
        };
        let project_id = field("projectId")?;
        let operation = field("operation")?;
        let upload_id = field("uploadId")?;
        let root = field("root")?;
        let begin = serde_json::json!({
            "uploadId":upload_id,"root":root,"totalBytes":archive.bytes,
            "sha256":archive.sha256,"mode":"full","deletePaths":[]
        });
        let short_timeout = Duration::from_secs(30);
        if let Err(error) = guest.call(
            "upload.begin",
            &begin,
            Some(&format!("{operation}:begin")),
            short_timeout,
        ) {
            if matches!(&error, GuestError::SpacePreflight { .. }) {
                return Err(VmProjectUploadError {
                    proven_not_sent: true,
                    detail: error.to_string(),
                });
            }
            if error.code() != Some("id_conflict") {
                return Err(error.into());
            }
            let existing = guest.call(
                "project.ensure",
                &serde_json::json!({"projectId":project_id}),
                None,
                short_timeout,
            )?;
            if existing.get("projectId").and_then(Value::as_str) == Some(project_id) {
                return Ok(existing);
            }
            return Err(GuestError::InvalidRequest.into());
        }
        let mut reader = File::open(archive.file.path()).map_err(|_| GuestError::InvalidRequest)?;
        let mut chunk = vec![0u8; VM_UPLOAD_CHUNK_BYTES];
        let mut sequence = 0usize;
        loop {
            let count = reader
                .read(&mut chunk)
                .map_err(|_| GuestError::InvalidRequest)?;
            if count == 0 {
                break;
            }
            let params = serde_json::json!({
                "uploadId":upload_id,"seq":sequence,
                "data":base64::engine::general_purpose::STANDARD.encode(&chunk[..count])
            });
            guest.call(
                "upload.chunk",
                &params,
                Some(&format!("{operation}:chunk:{sequence}")),
                short_timeout,
            )?;
            sequence += 1;
        }
        guest.call(
            "upload.commit",
            &serde_json::json!({"uploadId":upload_id}),
            Some(&format!("{operation}:upload")),
            Duration::from_secs(1810),
        )?;
        let project = guest.call(
            "project.import",
            &serde_json::json!({
                "projectId":project_id,"source":root,"owner":Value::Null,
                "incremental":false,"expectedStateId":Value::Null
            }),
            Some(&format!("{operation}:import")),
            Duration::from_secs(1810),
        )?;
        if project.get("projectId").and_then(Value::as_str) != Some(project_id) {
            return Err(GuestError::InvalidRequest.into());
        }
        Ok(project)
    }
}

fn write_vm_project_registration(
    state_dir: &Path,
    project_id: &str,
    source: &str,
    import_state_id: Option<&str>,
) -> Result<(), NativeError> {
    if project_id.is_empty()
        || Path::new(project_id).components().count() != 1
        || !matches!(
            Path::new(project_id).components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(NativeError::Invalid("vm-workspace/project"));
    }
    let directory = state_dir.join("layr-projects").join(project_id);
    std::fs::create_dir_all(&directory)
        .map_err(|_| NativeError::Unavailable("workspace-state/vm-project-registration"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| NativeError::Unavailable("workspace-state/vm-project-registration"))?;
    }
    let path = directory.join("project.json");
    let value = serde_json::json!({
        "projectId": project_id,
        "source": source,
        "importStateId": import_state_id,
    });
    let bytes = serde_json::to_vec(&value)
        .map_err(|_| NativeError::Unavailable("workspace-state/vm-project-registration"))?;
    crate::atomic_file::replace(&path, &bytes, Some(0o600), true)
        .map_err(|_| NativeError::Unavailable("workspace-state/vm-project-registration"))
}

fn ticket(operation: &str, agent: &str, action_request_id: &str, epoch: u64) -> ProviderTicket {
    ProviderTicket {
        operation: operation.to_owned(),
        agent: agent.to_owned(),
        epoch,
        account: "default".into(),
        thread: None,
        connection: String::new(),
        generation: 0,
        request: action_request_id.to_owned(),
    }
}

fn request(agent: &str, action_request_id: &str) -> native_actions::Request {
    native_actions::Request {
        request_id: action_request_id.to_owned(),
        agent: agent.to_owned(),
        action: native_actions::Action::Review,
        context: Value::Null,
    }
}

pub async fn prepare_lead(
    store: &RuntimeStore,
    guest: Arc<dyn GuestRpc>,
    agent_id: &str,
    start_attempt_id: &str,
    action_request_id: &str,
    epoch: u64,
) -> native_actions::Result<()> {
    Preparation {
        store,
        guest: Some(guest),
        expected_lead: true,
    }
    .prepare_vm_workspace(
        &request(agent_id, action_request_id),
        &ticket(start_attempt_id, agent_id, action_request_id, epoch),
    )
    .await
}

pub async fn prepare_child(
    store: &RuntimeStore,
    guest: Arc<dyn GuestRpc>,
    agent_id: &str,
    start_attempt_id: &str,
    action_request_id: &str,
    epoch: u64,
) -> native_actions::Result<()> {
    Preparation {
        store,
        guest: Some(guest),
        expected_lead: false,
    }
    .prepare_vm_workspace(
        &request(agent_id, action_request_id),
        &ticket(start_attempt_id, agent_id, action_request_id, epoch),
    )
    .await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::linux_vm_guest::GuestError;
    use flate2::read::GzDecoder;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn imported_vm_project_is_registered_for_host_mac_sync() {
        let state = tempfile::tempdir().unwrap();
        write_vm_project_registration(state.path(), "project-1", "/host/project", Some("state-1"))
            .expect("imported project registration writes");
        let value: Value = serde_json::from_slice(
            &fs::read(state.path().join("layr-projects/project-1/project.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "projectId":"project-1", "source":"/host/project", "importStateId":"state-1"
            })
        );
    }

    fn archive_paths(path: &Path) -> Vec<String> {
        tar::Archive::new(GzDecoder::new(
            File::open(path).expect("open project archive"),
        ))
        .entries()
        .expect("archive entries")
        .map(|entry| {
            entry
                .expect("valid archive entry")
                .path()
                .expect("valid archive path")
                .to_string_lossy()
                .into_owned()
        })
        .collect()
    }

    #[cfg(unix)]
    #[test]
    fn vm_project_archive_preserves_long_unicode_names_symlinks_and_permissions() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let root = tempfile::tempdir().expect("fixture operation succeeds");
        let directory_name = format!("目录-{}", "component".repeat(18));
        let directory = root.path().join(&directory_name);
        fs::create_dir(&directory).expect("create project directory");
        let file_name = "café.txt";
        let file_path = directory.join(file_name);
        fs::write(&file_path, b"payload").expect("write project file");
        fs::set_permissions(&file_path, fs::Permissions::from_mode(0o751))
            .expect("set project file permissions");
        let link_name = format!("{}.link", "link".repeat(30));
        symlink(file_name, directory.join(&link_name)).expect("create project symlink");

        let archive = vm_project_archive(root.path()).expect("archive project");
        let mut tar_archive = tar::Archive::new(GzDecoder::new(
            File::open(archive.file.path()).expect("open project archive"),
        ));
        let mut entries = tar_archive.entries().expect("archive entries");
        let mut found_file = false;
        let mut found_link = false;
        for entry in &mut entries {
            let entry = entry.expect("valid archive entry");
            let path = entry
                .path()
                .expect("valid archive path")
                .to_string_lossy()
                .into_owned();
            if path == format!("{directory_name}/{file_name}") {
                found_file = true;
                assert_eq!(entry.header().mode().expect("file mode"), 0o751);
            } else if path == format!("{directory_name}/{link_name}") {
                found_link = true;
                assert!(entry.header().entry_type().is_symlink());
                assert_eq!(
                    entry
                        .link_name()
                        .expect("symlink target field")
                        .expect("symlink target")
                        .to_string_lossy(),
                    file_name
                );
            }
        }
        assert!(found_file);
        assert!(found_link);
    }

    #[test]
    fn actual_repository_keeps_force_added_files_under_ignored_directories() {
        let root = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .args(["init", "--quiet", root.path().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(init.status.success());
        fs::write(root.path().join(".gitignore"), "ignored/\n").unwrap();
        fs::create_dir_all(root.path().join("ignored")).unwrap();
        fs::write(
            root.path().join("ignored/forced.txt"),
            "keep this tracked file\n",
        )
        .unwrap();
        let add = Command::new("git")
            .args([
                "-C",
                root.path().to_str().unwrap(),
                "add",
                "-f",
                "ignored/forced.txt",
            ])
            .output()
            .unwrap();
        assert!(add.status.success());

        let archive = vm_project_archive(root.path()).unwrap();
        assert!(
            archive_paths(archive.file.path())
                .iter()
                .any(|path| { path == "ignored/forced.txt" })
        );
    }

    #[test]
    fn submodule_metadata_inside_a_nested_repository_is_archived() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        let init = Command::new("git")
            .args(["init", "--quiet", nested.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(init.status.success());
        let submodule = nested.join("vendor/dep");
        let git_dir = nested.join(".git/modules/vendor/dep");
        fs::create_dir_all(&submodule).unwrap();
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            submodule.join(".git"),
            "gitdir: ../../.git/modules/vendor/dep\n",
        )
        .unwrap();
        fs::write(submodule.join("source.txt"), "nested submodule code\n").unwrap();

        let archive = vm_project_archive(root.path()).unwrap();
        let paths = archive_paths(archive.file.path());
        for expected in [
            "nested/vendor/dep/.git",
            "nested/vendor/dep/source.txt",
            "nested/.git/modules/vendor/dep/HEAD",
        ] {
            assert!(
                paths.iter().any(|path| path == expected),
                "missing {expected}"
            );
        }
    }

    struct UploadPreflightGuest {
        preflight_once: AtomicBool,
        preflight_method: &'static str,
        calls: Mutex<Vec<(String, Option<String>)>>,
        project_id: Mutex<Option<String>>,
        begin_conflict: bool,
        existing_project: bool,
    }

    impl UploadPreflightGuest {
        fn new(preflight_method: &'static str) -> Self {
            Self {
                preflight_once: AtomicBool::new(true),
                preflight_method,
                calls: Mutex::new(Vec::new()),
                project_id: Mutex::new(None),
                begin_conflict: false,
                existing_project: false,
            }
        }

        fn with_begin_conflict(mut self) -> Self {
            self.begin_conflict = true;
            self
        }

        fn with_existing_project(mut self) -> Self {
            self.existing_project = true;
            self
        }
    }

    impl GuestRpc for UploadPreflightGuest {
        fn stream(
            &self,
            method: &str,
            params: &Value,
            request_id: Option<&str>,
            _timeout: Duration,
            _on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
        ) -> Result<Value, GuestError> {
            self.calls
                .lock()
                .unwrap()
                .push((method.to_owned(), request_id.map(str::to_owned)));
            match method {
                "upload.begin" if self.begin_conflict => Err(GuestError::Rejected {
                    code: Some("id_conflict".into()),
                    detail: None,
                    uncertain: false,
                }),
                "project.ensure" if self.existing_project => Ok(serde_json::json!({
                    "projectId":params["projectId"],"stateId":"existing-state"
                })),
                "project.ensure" => Err(GuestError::Rejected {
                    code: Some("not_found".into()),
                    detail: None,
                    uncertain: false,
                }),
                method
                    if method == self.preflight_method
                        && self.preflight_once.swap(false, Ordering::SeqCst) =>
                {
                    Err(GuestError::SpacePreflight {
                        message: "insufficient guest space".into(),
                    })
                }
                "project.import" => {
                    *self.project_id.lock().unwrap() =
                        params["projectId"].as_str().map(str::to_owned);
                    Ok(serde_json::json!({"projectId":params["projectId"]}))
                }
                "line.bind" => Ok(serde_json::json!({
                    "cwd":"/vm/project", "projectId":params["projectId"],
                    "line":"main", "owner":"vm-lead", "home":"/vm",
                    "stateId":"state-1"
                })),
                "agent.context" => Ok(serde_json::json!({
                    "projectId":self.project_id.lock().unwrap().clone(), "line":"main"
                })),
                _ => Ok(Value::Null),
            }
        }
    }

    #[test]
    fn upload_identity_conflict_reuses_the_existing_guest_project() {
        let archive_file = tempfile::NamedTempFile::new().expect("archive fixture");
        let archive = VmProjectArchive {
            file: archive_file,
            bytes: 0,
            sha256: "fixture-hash".into(),
        };
        let guest = UploadPreflightGuest::new("")
            .with_begin_conflict()
            .with_existing_project();
        let intent = serde_json::json!({
            "projectId":"stable-project", "operation":"stable-operation",
            "uploadId":"stable-operation", "root":"/projects/stable"
        });
        let result = Preparation::upload_vm_project(&guest, &intent, &archive)
            .ok()
            .expect("reuse existing project after upload identity conflict");
        assert_eq!(result["projectId"], "stable-project");
        let calls = guest.calls.lock().expect("guest calls");
        assert_eq!(
            calls.iter().map(|call| call.0.as_str()).collect::<Vec<_>>(),
            ["upload.begin", "project.ensure"]
        );
    }

    #[tokio::test]
    async fn upload_space_preflight_is_retryable_with_the_same_request_identity() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("project");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("main.py"), "print('hello')\n").unwrap();
        let stale_dependency = source.join("data/downloads/stale-dependency");
        fs::create_dir_all(&stale_dependency).unwrap();
        fs::write(
            stale_dependency.join(".git"),
            "gitdir: ../../missing/object-store\n",
        )
        .unwrap();
        let state = directory.path().join("state");
        fs::create_dir(&state).unwrap();
        let store = RuntimeStore::open(Some(&state)).await.unwrap();
        let agent = AgentRecord::from_value(serde_json::json!({
            "id":"vm-lead", "rootId":"vm-lead", "epoch":1, "status":"starting",
            "provider":"codex", "executionMode":"vm", "isLead":true, "autoWake":true,
            "cwd":source, "hostProjectPath":source, "projectId":"project-1",
            "startAttempt":{"id":"attempt-1"}
        }))
        .unwrap();
        let stored_agent = agent.clone();
        store
            .transaction(move |tx| AgentRepository::put(tx, &stored_agent))
            .await
            .unwrap();
        let guest = Arc::new(UploadPreflightGuest::new("upload.begin"));

        assert!(matches!(
            prepare_lead(
                &store,
                guest.clone(),
                "vm-lead",
                "attempt-1",
                "turn:attempt-1",
                1
            )
            .await,
            Err(NativeError::Unavailable(_))
        ));
        let connection = store.read().unwrap();
        let saved = AgentRepository::get(&connection, &agent.id)
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(
            saved["startAttempt"]["vmWorkspacePreparation"]["phase"],
            "not_sent"
        );
        drop(connection);

        prepare_lead(
            &store,
            guest.clone(),
            "vm-lead",
            "attempt-1",
            "turn:attempt-1",
            1,
        )
        .await
        .expect("retry exact not-sent upload");
        let calls = guest.calls.lock().unwrap();
        let begin_ids = calls
            .iter()
            .filter(|(method, _)| method == "upload.begin")
            .map(|(_, request_id)| request_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(begin_ids.len(), 2);
        assert_eq!(begin_ids[0], begin_ids[1]);
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| method == "upload.chunk")
                .count(),
            1
        );
        let connection = store.read().unwrap();
        let saved = AgentRepository::get(&connection, &agent.id)
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(
            saved["startAttempt"]["vmWorkspacePreparation"]["phase"],
            "completed"
        );
    }

    #[tokio::test]
    async fn upload_commit_preflight_is_unknown_after_begin_and_chunk_effects() {
        let directory = tempfile::tempdir().expect("fixture operation succeeds");
        let source = directory.path().join("project");
        fs::create_dir(&source).expect("create project directory");
        fs::write(source.join("main.py"), "print('hello')\n").expect("write project file");
        let state = directory.path().join("state");
        fs::create_dir(&state).expect("create state directory");
        let store = RuntimeStore::open(Some(&state))
            .await
            .expect("open test store");
        let agent = AgentRecord::from_value(serde_json::json!({
            "id":"vm-lead", "rootId":"vm-lead", "epoch":1, "status":"starting",
            "provider":"codex", "executionMode":"vm", "isLead":true, "autoWake":true,
            "cwd":source, "hostProjectPath":source, "projectId":"project-1",
            "startAttempt":{"id":"attempt-1"}
        }))
        .expect("valid test agent");
        let stored_agent = agent.clone();
        store
            .transaction(move |tx| AgentRepository::put(tx, &stored_agent))
            .await
            .expect("insert test agent");
        let guest = Arc::new(UploadPreflightGuest::new("upload.commit"));

        assert!(matches!(
            prepare_lead(
                &store,
                guest.clone(),
                "vm-lead",
                "attempt-1",
                "turn:attempt-1",
                1
            )
            .await,
            Err(NativeError::Unknown)
        ));
        {
            let connection = store.read().expect("open test store read");
            let saved = AgentRepository::get(&connection, &agent.id)
                .expect("read test agent")
                .expect("test agent exists")
                .value();
            assert_eq!(
                saved["startAttempt"]["vmWorkspacePreparation"]["phase"],
                "unknown"
            );
            assert_eq!(
                saved["startAttempt"]["vmWorkspacePreparation"]["projectImport"]["phase"],
                "unknown"
            );
        }

        let first_calls = guest.calls.lock().expect("read guest calls").clone();
        assert!(
            first_calls
                .iter()
                .any(|(method, _)| method == "upload.begin")
        );
        assert!(
            first_calls
                .iter()
                .any(|(method, _)| method == "upload.chunk")
        );
        assert!(
            first_calls
                .iter()
                .any(|(method, _)| method == "upload.commit")
        );

        assert!(matches!(
            prepare_lead(
                &store,
                guest.clone(),
                "vm-lead",
                "attempt-1",
                "turn:attempt-1",
                1
            )
            .await,
            Err(NativeError::Unknown)
        ));
        assert_eq!(*guest.calls.lock().expect("read guest calls"), first_calls);
    }

    #[test]
    fn gitdir_pointer_trims_one_line_ending_and_rejects_multiple_lines() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("submodule");
        let git_dir = root.path().join(".git/modules/submodule");
        fs::create_dir_all(&directory).unwrap();
        fs::create_dir_all(git_dir.join("objects")).unwrap();
        let marker = directory.join(".git");
        fs::write(&marker, "gitdir: ../.git/modules/submodule\r\n").unwrap();
        assert_eq!(
            read_gitdir_pointer(&directory, &marker).unwrap(),
            fs::canonicalize(&git_dir).unwrap()
        );
        fs::write(&marker, "gitdir: ../.git/modules/submodule\ngarbage\n").unwrap();
        assert_eq!(
            read_gitdir_pointer(&directory, &marker).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        fs::write(&marker, "gitdir:  ../.git/modules/submodule\n").unwrap();
        assert_eq!(
            read_gitdir_pointer(&directory, &marker).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[cfg(unix)]
    #[test]
    fn git_object_external_links_and_alternates_are_refused() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        fs::create_dir_all(repo.join(".git/objects/info")).unwrap();
        let external = tempfile::tempdir().unwrap();
        symlink(external.path(), repo.join(".git/objects/pack-link")).unwrap();
        assert!(validate_vm_repository_objects(root.path(), &repo).is_err());
        fs::remove_file(repo.join(".git/objects/pack-link")).unwrap();

        fs::write(
            repo.join(".git/objects/info/alternates"),
            external.path().to_string_lossy().as_bytes(),
        )
        .unwrap();
        assert!(validate_vm_repository_objects(root.path(), &repo).is_err());
        fs::remove_file(repo.join(".git/objects/info/alternates")).unwrap();

        fs::remove_dir_all(repo.join(".git/objects")).unwrap();
        symlink(external.path(), repo.join(".git/objects")).unwrap();
        assert!(validate_vm_repository_objects(root.path(), &repo).is_err());
    }

    #[test]
    fn excluded_nested_git_repository_does_not_block_archive_for_external_alternates() {
        let root = tempfile::tempdir().unwrap();
        let excluded_repo = root.path().join("node_modules/shared-clone");
        fs::create_dir_all(&excluded_repo).unwrap();
        let init = Command::new("git")
            .args(["init", "--quiet", excluded_repo.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(init.status.success());
        fs::write(
            excluded_repo.join(".git/objects/info/alternates"),
            "/external/object/store\n",
        )
        .unwrap();

        let exclusions = vm_archive_exclusions(root.path())
            .expect("excluded repositories are not validated or archived");
        assert!(vm_archive_skip(
            root.path(),
            Path::new("node_modules"),
            &root.path().join("node_modules"),
            &exclusions,
        ));
    }

    #[test]
    fn excluded_download_tree_skips_a_stale_git_pointer() {
        let root = tempfile::tempdir().unwrap();
        let dependency = root.path().join("data/downloads/stale-dependency");
        fs::create_dir_all(&dependency).unwrap();
        fs::write(
            dependency.join(".git"),
            "gitdir: ../../missing/object-store\n",
        )
        .unwrap();

        let archive = vm_project_archive(root.path())
            .expect("the excluded download pointer is never resolved");

        assert!(archive.bytes > 0);
    }

    #[test]
    fn regenerable_names_inside_git_metadata_are_preserved() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let root = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .args(["init", "--quiet", root.path().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(init.status.success());
        fs::write(
            root.path().join(".git/refs/heads/venv"),
            "ref: refs/heads/main\n",
        )
        .unwrap();
        let module = root.path().join(".git/modules/node_modules");
        fs::create_dir_all(module.join("objects")).unwrap();
        fs::write(
            module.join("config"),
            "[core]\nrepositoryformatversion = 0\n",
        )
        .unwrap();

        let archive = vm_project_archive(root.path()).expect("archive Git metadata");
        let mut decoded = Vec::new();
        GzDecoder::new(File::open(archive.file.path()).unwrap())
            .read_to_end(&mut decoded)
            .unwrap();
        let mut paths = Vec::new();
        let mut offset = 0;
        while offset + 512 <= decoded.len() {
            let header = &decoded[offset..offset + 512];
            if header.iter().all(|byte| *byte == 0) {
                break;
            }
            let field = |bytes: &[u8]| {
                let end = bytes
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(bytes.len());
                String::from_utf8_lossy(&bytes[..end]).into_owned()
            };
            let name = field(&header[..100]);
            let prefix = field(&header[345..500]);
            paths.push(if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            });
            let size = usize::from_str_radix(field(&header[124..136]).trim(), 8).unwrap();
            offset += 512 + size.div_ceil(512) * 512;
        }
        for entry in [".git/refs/heads/venv", ".git/modules/node_modules/config"] {
            assert!(
                paths.iter().any(|path| path == entry),
                "archive should retain {entry}"
            );
        }
    }

    #[test]
    fn excluded_submodule_worktree_still_validates_archived_internal_metadata() {
        let root = tempfile::tempdir().unwrap();
        let init = Command::new("git")
            .args(["init", "--quiet", root.path().to_str().unwrap()])
            .output()
            .unwrap();
        assert!(init.status.success());
        let submodule = root.path().join("data/downloads/dep");
        let git_dir = root.path().join(".git/modules/data/downloads/dep");
        fs::create_dir_all(&submodule).unwrap();
        fs::create_dir_all(git_dir.join("objects/info")).unwrap();
        fs::write(
            submodule.join(".git"),
            "gitdir: ../../../.git/modules/data/downloads/dep\n",
        )
        .unwrap();
        fs::write(
            git_dir.join("objects/info/alternates"),
            "/external/object/store\n",
        )
        .unwrap();

        assert!(vm_archive_exclusions(root.path()).is_err());
    }
}
