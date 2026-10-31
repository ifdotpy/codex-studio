//! Source transfer into a Mac slot: manifests, clones from the project folder
//! on this Mac, mode changes and file entries.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use serde_json::{Value, json};

use super::protocol::{self, Failure, HostExecError, INVALID, Manifest, Outcome, require};
use super::{HostService, Op, State, SyncBatchItem, SyncModeItem, SyncOperation, clone_ffi, slots};

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
/// Largest hash cache of a project folder before it starts again.
const PROJECT_HASHES_LIMIT: usize = 100_000;
/// Largest cache folder list of one request.
const MAX_CACHES: usize = 256;
/// Largest `sync.mode` item count.
const MAX_MODE_ITEMS: usize = 4096;
/// Largest `sync.batch` item count.
const MAX_BATCH_ITEMS: usize = 1024;
/// Longest clone failure text in a `prime` or `sync.clone` answer, in characters.
const CLONE_ERROR_CHARS: usize = 300;

#[derive(serde::Deserialize)]
struct ProjectSource {
    source: String,
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

fn clone_error(error: &std::io::Error) -> String {
    format!(
        "clone failed: {}",
        error
            .to_string()
            .chars()
            .take(CLONE_ERROR_CHARS)
            .collect::<String>()
    )
}

/// Python `clone_tree`: clone a folder file by file with APFS clonefile(2),
/// never with a byte copy. Links stay links; sockets and pipes stay out; a file
/// that disappears during the walk stays out. Returns the bytes as
/// [`HostService::size`] counts them, although the clones share their blocks.
pub(crate) fn clone_tree(origin: &Path, target: &Path) -> std::io::Result<u64> {
    let mut total = 0_u64;
    super::private_dir(target)?;
    let mut stack = vec![PathBuf::new()];
    while let Some(base) = stack.pop() {
        let Ok(listing) = fs::read_dir(origin.join(&base)) else {
            continue;
        };
        for entry in listing.flatten() {
            let name = base.join(entry.file_name());
            let (source, copy) = (origin.join(&name), target.join(&name));
            let meta = match fs::symlink_metadata(&source) {
                Ok(meta) => meta,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if meta.file_type().is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(&source)?, &copy)?;
            } else if meta.is_dir() {
                fs::create_dir(&copy)?;
                stack.push(name);
            } else if meta.is_file() {
                match clone_ffi::clone_file(&source, &copy) {
                    Ok(()) => total = total.saturating_add(meta.blocks().saturating_mul(512)),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(total)
}

impl HostService {
    /// The entries of the slot's source folder. The slot's hash cache keeps the
    /// hash of each unchanged file and of each file cloned since the last scan.
    pub(crate) fn scan_slot(
        slot: &Path,
        skip: &[String],
        cloned: Option<protocol::HashCache>,
    ) -> Outcome<Manifest> {
        let cache = slot.join("hash-cache.json");
        let mut hashes = protocol::load_hashes(&cache);
        if let Some(cloned) = cloned {
            hashes.extend(cloned);
        }
        let entries = protocol::scan(&slot.join("source"), skip, Some(&mut hashes))?;
        protocol::atomic(&cache, &hashes)?;
        Ok(entries)
    }

    /// [`Self::scan_slot`] off the request loop: hashing a large slot takes minutes.
    pub(crate) async fn scan_slot_blocking(
        self: &Arc<Self>,
        slot: &Path,
        skip: Vec<String>,
    ) -> Outcome<Manifest> {
        let cloned = self.inner().cloned.remove(slot);
        let slot = slot.to_path_buf();
        studio_runtime_store::blocking_pool::spawn_blocking(
            "runtime-services/host-exec-scan",
            move || Self::scan_slot(&slot, &skip, cloned),
        )
        .await
        .map_err(|error| Failure::Unexpected(error.to_string()))?
    }

    pub(crate) fn cache_paths(items: &[String]) -> Result<Vec<String>, HostExecError> {
        require(items.len() <= MAX_CACHES, "Invalid cache folders", INVALID)?;
        items
            .iter()
            .map(|item| protocol::relative(Some(&protocol::text_value(item))))
            .collect()
    }

    /// The project's folder on this Mac (`layr-projects/<project>/project.json` "source").
    pub(crate) fn project_folder(&self, project_id: &str) -> Option<PathBuf> {
        if !slots::project_folder_name(project_id) {
            return None;
        }
        let path = self
            .state
            .join("layr-projects")
            .join(project_id)
            .join("project.json");
        let studio_runtime_schema::json_object::JsonObject(project): studio_runtime_schema::json_object::JsonObject<ProjectSource> = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
        let source = project.source;
        if !source.starts_with('/') {
            return None;
        }
        let path = PathBuf::from(source);
        (path.is_dir() && !protocol::is_symlink(&path)).then_some(path)
    }

    /// Give the slot's git layer the objects of the project's repository on this
    /// Mac. The guest sends the line's `.git` without its objects; git reads the
    /// project objects through alternates and writes new objects into the slot.
    pub(crate) fn link_git_objects(&self, op: &Op, source: &Path) -> Outcome<()> {
        let git = source.join(".git");
        let Some(project) = self.project_folder(op.project_id.as_str()) else {
            return Ok(());
        };
        let objects = project.join(".git").join("objects");
        if !objects.is_dir()
            || protocol::is_symlink(&objects)
            || !git.is_dir()
            || protocol::is_symlink(&git)
        {
            return Ok(());
        }
        let info = git.join("objects").join("info");
        super::private_dirs(&info)?;
        let temporary = info.join(".alternates.tmp");
        fs::write(&temporary, format!("{}\n", objects.display()))?;
        fs::rename(&temporary, info.join("alternates"))?;
        Ok(())
    }

    /// Whether the slot's cache folder holds only Mac-only paths and the folders above them.
    fn bare(target: &Path, cache: &str, mac_only: &[String]) -> bool {
        let mut stack = vec![PathBuf::new()];
        while let Some(here) = stack.pop() {
            let prefix = if here.as_os_str().is_empty() {
                cache.to_owned()
            } else {
                format!("{cache}/{}", here.display())
            };
            let Ok(listing) = fs::read_dir(target.join(&here)) else {
                continue;
            };
            for entry in listing.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let rel = format!("{prefix}/{name}");
                if protocol::under(&rel, mac_only) {
                    continue;
                }
                let path = entry.path();
                if path.is_dir()
                    && !protocol::is_symlink(&path)
                    && mac_only
                        .iter()
                        .any(|item| item.starts_with(&format!("{rel}/")))
                {
                    stack.push(here.join(&name));
                    continue;
                }
                return false;
            }
        }
        true
    }

    /// Clone a cache folder that the slot lacks from the project's Mac folder, so
    /// the transfer sends only the files that differ. Each reason not to clone
    /// leaves the normal transfer.
    async fn prime(self: &Arc<Self>, op: &Op, slot: &Path, cache: &str) -> Outcome<Value> {
        let mac_only = Self::mac_only(op);
        let target = protocol::inside(
            &slot.join("source"),
            Some(&protocol::text_value(cache)),
            false,
        )?;
        let skipped = |reason: &str| Ok(json!({"path": cache, "primed": false, "reason": reason}));
        if protocol::under(cache, &mac_only) {
            return skipped("Host-only path");
        }
        if protocol::is_symlink(&target)
            || (target.exists() && !Self::bare(&target, cache, &mac_only))
        {
            return skipped("the slot has the folder");
        }
        let Some(folder) = self.project_folder(op.project_id.as_str()) else {
            return skipped("no project folder on this host");
        };
        let Ok(origin) = protocol::inside(&folder, Some(&protocol::text_value(cache)), true) else {
            return skipped("the host path crosses a symbolic link");
        };
        if !origin.is_dir() {
            return skipped("the project folder on this host has no such folder");
        }
        let temporary = slot.join("prime");
        let cloned = {
            let (temporary, origin) = (temporary.clone(), origin.clone());
            studio_runtime_store::blocking_pool::spawn_blocking(
                "runtime-services/host-exec-prime",
                move || {
                    super::remove_tree_quietly(&temporary);
                    let result = clone_tree(&origin, &temporary);
                    if result.is_err() {
                        super::remove_tree_quietly(&temporary);
                    }
                    result
                },
            )
            .await
            .map_err(|error| Failure::Unexpected(error.to_string()))?
        };
        let cloned = match cloned {
            Ok(cloned) => cloned,
            Err(error) => {
                return Ok(json!({"path": cache, "primed": false, "reason": clone_error(&error)}));
            }
        };
        // A clone counts at full size, as the walk of `size` counts it.
        let project = slot.parent().unwrap_or(slot);
        if let Err(error) = self.reserve(project, self.budget_of(op), cloned, slot) {
            super::remove_tree_quietly(&temporary);
            return Err(error);
        }
        if target.exists() {
            // The slot keeps its own Mac-only output in place of the copy from the project folder.
            let prefix = format!("{cache}/");
            for item in &mac_only {
                let Some(inner) = item.strip_prefix(&prefix) else {
                    continue;
                };
                let own = target.join(inner);
                if !(own.exists() || protocol::is_symlink(&own)) {
                    continue;
                }
                let Ok(copy) =
                    protocol::inside(&temporary, Some(&protocol::text_value(inner)), false)
                else {
                    continue;
                };
                super::remove_entry(&copy)?;
                if let Some(parent) = copy.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::rename(&own, &copy)?;
            }
            fs::remove_dir_all(&target)?;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::rename(&temporary, &target)?;
        Ok(json!({"path": cache, "primed": true, "bytes": cloned}))
    }

    pub(crate) async fn prime_request(
        self: &Arc<Self>,
        identity: &str,
        op: &Op,
        slot: &Path,
        caches: &[String],
    ) -> Outcome<Value> {
        require(
            op.state == State::Leased,
            "The slot is no longer open for source transfer",
            INVALID,
        )?;
        let caches = Self::cache_paths(caches)?;
        let lock = self.lock_for(&format!("run\0{identity}"));
        let _held = lock.lock().await;
        let mut primed = Vec::new();
        for cache in &caches {
            primed.push(self.prime(op, slot, cache).await?);
        }
        self.refuse_orphan(identity)?;
        Ok(json!({"primed": primed}))
    }

    pub(crate) async fn held_manifest(
        self: &Arc<Self>,
        identity: &str,
        slot: &Path,
        mac_only: Vec<String>,
        after: &str,
    ) -> Outcome<Value> {
        let path = self.opdir(identity).join("held-manifest.json");
        let name = format!("{identity}\0held");
        if !after.is_empty() {
            return self.listing(&name, &path, after, None);
        }
        let held = self.scan_slot_blocking(slot, mac_only).await?;
        protocol::atomic(&path, &held)?;
        self.listing(&name, &path, "", Some(&held))
    }

    /// Clone one file from the same path in the project folder on this Mac when
    /// that file has the size and SHA-256 of `entry`, so the guest does not send
    /// its content. Each reason not to clone leaves the transfer.
    fn clone_source(
        &self,
        op: &Op,
        slot: &Path,
        name: &str,
        entry: &protocol::ManifestEntry,
    ) -> Outcome<Value> {
        let protocol::ManifestEntry::File {
            bytes,
            mode,
            sha256,
        } = entry
        else {
            return Err(HostExecError::new(INVALID, "Invalid source entry").into());
        };
        require(*mode <= 0o777, "Invalid source file mode", INVALID)?;
        let name = protocol::relative(Some(&protocol::text_value(name)))?;
        require(
            !protocol::under(&name, &Self::mac_only(op)),
            "The path stays on the host (macOnly)",
            INVALID,
        )?;
        let path = protocol::inside(
            &slot.join("source"),
            Some(&protocol::text_value(&name)),
            false,
        )?;
        let skipped = |reason: &str| Ok(json!({"cloned": false, "reason": reason}));
        let Some(folder) = self.project_folder(op.project_id.as_str()) else {
            return skipped("no project folder on this host");
        };
        let Ok(origin) = protocol::inside(&folder, Some(&protocol::text_value(&name)), true) else {
            return skipped("the host path crosses a symbolic link");
        };
        let Ok(info) = fs::symlink_metadata(&origin) else {
            return skipped("the project folder has no such file");
        };
        if !info.is_file() || info.size() != *bytes {
            return skipped("the project file differs");
        }
        let cache = self
            .state
            .join("layr-projects")
            .join(op.project_id.as_str())
            .join("host-exec-hashes.json");
        let mut hashes = protocol::load_hashes(&cache);
        if hashes.len() > PROJECT_HASHES_LIMIT {
            hashes.clear();
        }
        let Ok((checksum, known)) =
            protocol::file_hash(&origin, &info, &name, Some(&mut hashes), protocol::now_ns())
        else {
            return skipped("the project file cannot be read");
        };
        protocol::atomic(&cache, &hashes)?;
        if sha256 != &checksum {
            return skipped("the project file differs");
        }
        self.reserve(
            slot.parent().unwrap_or(slot),
            self.budget_of(op),
            info.blocks().saturating_mul(512),
            slot,
        )?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        super::remove_entry(&path)?;
        if let Err(error) = clone_ffi::clone_file(&origin, &path) {
            return Ok(json!({"cloned": false, "reason": clone_error(&error)}));
        }
        set_mode(&path, *mode)?;
        let after = fs::symlink_metadata(&origin)?;
        if known && protocol::stamp(&after) == protocol::stamp(&info) {
            // The origin did not change since its known hash: the run check need not read the clone.
            let mine = fs::symlink_metadata(&path)?;
            let row = protocol::HashRow::new(protocol::stamp(&mine), checksum);
            self.inner()
                .cloned
                .entry(slot.to_path_buf())
                .or_default()
                .insert(name, row);
        }
        Ok(json!({"cloned": true, "bytes": bytes}))
    }

    pub(crate) async fn sync_clone(
        self: &Arc<Self>,
        op: &Op,
        slot: &Path,
        name: &str,
        entry: &protocol::ManifestEntry,
    ) -> Outcome<Value> {
        require(
            op.state == State::Leased,
            "The slot is no longer open for source transfer",
            INVALID,
        )?;
        let (service, op, slot) = (Arc::clone(self), op.clone(), slot.to_path_buf());
        let (name, entry) = (name.to_owned(), entry.clone());
        studio_runtime_store::blocking_pool::spawn_blocking(
            "runtime-services/host-exec-clone",
            move || service.clone_source(&op, &slot, &name, &entry),
        )
        .await
        .map_err(|error| Failure::Unexpected(error.to_string()))?
    }

    /// Change only the mode of slot files whose content the guest's entry already
    /// has. The hash cache keeps each known hash with the new change time.
    fn set_modes(slot: &Path, mac_only: &[String], items: &[SyncModeItem]) -> Outcome<()> {
        let cache = slot.join("hash-cache.json");
        let mut hashes = protocol::load_hashes(&cache);
        for item in items {
            let protocol::ManifestEntry::File {
                bytes,
                mode,
                sha256,
            } = &item.entry
            else {
                return Err(HostExecError::new(INVALID, "Invalid mode change").into());
            };
            require(*mode <= 0o777, "Invalid mode change", INVALID)?;
            let name = protocol::relative(Some(&protocol::text_value(&item.path)))?;
            require(
                !protocol::under(&name, mac_only),
                "The path stays on the host (macOnly)",
                INVALID,
            )?;
            let path = protocol::inside(
                &slot.join("source"),
                Some(&protocol::text_value(&name)),
                true,
            )?;
            let info = match fs::symlink_metadata(&path) {
                Ok(info) => info,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(HostExecError::new(
                        "sync_conflict",
                        format!("The slot file differs: {name}"),
                    )
                    .into());
                }
                Err(error) => return Err(error.into()),
            };
            require(
                info.is_file() && info.size() == *bytes,
                "The slot file differs",
                "sync_conflict",
            )?;
            let stamp = protocol::stamp(&info);
            let known = hashes
                .get(&name)
                .filter(|cached| cached.stamp() == stamp)
                .map(|cached| cached.4.clone());
            let checksum = match &known {
                Some(checksum) => checksum.clone(),
                None => protocol::digest_file(&path)?.0,
            };
            require(
                *sha256 == checksum,
                "The slot file differs",
                "sync_conflict",
            )?;
            set_mode(&path, *mode)?;
            let after = fs::symlink_metadata(&path)?;
            let after_stamp = protocol::stamp(&after);
            if known.is_some()
                && (after_stamp.0, after_stamp.1, after_stamp.2) == (stamp.0, stamp.1, stamp.2)
            {
                hashes.insert(name, protocol::HashRow::new(after_stamp, checksum));
            } else {
                hashes.remove(&name);
            }
        }
        protocol::atomic(&cache, &hashes)?;
        Ok(())
    }

    pub(crate) async fn sync_mode(
        &self,
        op: &Op,
        slot: &Path,
        mac_only: Vec<String>,
        items: &[SyncModeItem],
    ) -> Outcome<Value> {
        require(
            op.state == State::Leased,
            "The slot is no longer open for source transfer",
            INVALID,
        )?;
        require(
            !items.is_empty() && items.len() <= MAX_MODE_ITEMS,
            "Invalid mode changes",
            INVALID,
        )?;
        let items = items.to_vec();
        let count = items.len();
        let slot = slot.to_path_buf();
        studio_runtime_store::blocking_pool::spawn_blocking(
            "runtime-services/host-exec-modes",
            move || Self::set_modes(&slot, &mac_only, &items),
        )
        .await
        .map_err(|error| Failure::Unexpected(error.to_string()))??;
        Ok(json!({"entries": count}))
    }

    /// Write one source entry: a folder, a link or a file. A file with `data` is
    /// complete; a file without it starts empty and receives its content in chunks.
    fn place(
        &self,
        op: &Op,
        slot: &Path,
        name: &str,
        entry: &protocol::ManifestEntry,
        data: Option<&[u8]>,
    ) -> Outcome<()> {
        let name_value = protocol::text_value(name);
        let path = protocol::inside(&slot.join("source"), Some(&name_value), false)?;
        let name = protocol::relative(Some(&name_value))?;
        require(
            !protocol::under(&name, &Self::mac_only(op)),
            "The path stays on the host (macOnly)",
            INVALID,
        )?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        match fs::symlink_metadata(&path) {
            Ok(meta) if !meta.is_dir() => fs::remove_file(&path)?,
            Ok(_) if !matches!(entry, protocol::ManifestEntry::Directory) => {
                fs::remove_dir_all(&path)?
            }
            _ => {}
        }
        let (length, mode, checksum) = match entry {
            protocol::ManifestEntry::Directory => {
                if !path.is_dir() {
                    fs::create_dir(&path)?;
                }
                return Ok(());
            }
            protocol::ManifestEntry::Link { target } => {
                require(!target.contains('\0'), "Invalid link target", INVALID)?;
                std::os::unix::fs::symlink(target, &path)?;
                return Ok(());
            }
            protocol::ManifestEntry::File {
                bytes,
                mode,
                sha256,
            } => (*bytes, *mode, sha256),
        };
        let budget = self.budget_of(op);
        require(length <= budget, "Invalid file size", INVALID)?;
        self.reserve(slot.parent().unwrap_or(slot), budget, length, slot)?;
        let Some(data) = data else {
            fs::File::create(&path)?;
            let entries = self.opdir(op.operation_id.as_str()).join("entries");
            if !entries.is_dir() {
                fs::create_dir(&entries)?;
            }
            protocol::atomic(
                &entries.join(format!("{}.json", protocol::key(&name))),
                entry,
            )?;
            return Ok(());
        };
        use sha2::Digest;
        let actual_checksum = protocol::hex(sha2::Sha256::digest(data).as_slice());
        require(
            data.len() as u64 == length && *checksum == actual_checksum,
            "The source checksum differs",
            INVALID,
        )?;
        require(mode <= 0o777, "Invalid source file mode", INVALID)?;
        // No fsync: the next command hashes the slot again and sends a file that a crash broke.
        fs::write(&path, data)?;
        set_mode(&path, mode)?;
        Ok(())
    }

    pub(crate) fn sync_batch(
        &self,
        op: &Op,
        slot: &Path,
        items: &[SyncBatchItem],
    ) -> Outcome<Value> {
        require(
            op.state == State::Leased,
            "The slot is no longer open for source transfer",
            INVALID,
        )?;
        require(
            !items.is_empty() && items.len() <= MAX_BATCH_ITEMS,
            "Invalid source batch",
            INVALID,
        )?;
        for item in items {
            let content = match item.data.as_deref() {
                Some(data) => Some(
                    BASE64
                        .decode(data)
                        .map_err(|error| Failure::Unexpected(error.to_string()))?,
                ),
                None => None,
            };
            self.place(op, slot, &item.path, &item.entry, content.as_deref())?;
        }
        Ok(json!({"entries": items.len()}))
    }

    pub(crate) fn sync_operation(
        &self,
        identity: &str,
        op: &Op,
        slot: &Path,
        mac_only: &[String],
        operation: &SyncOperation,
    ) -> Outcome<Value> {
        require(
            op.state == State::Leased,
            "The slot is no longer open for source transfer",
            INVALID,
        )?;
        let name = match operation {
            SyncOperation::Delete(params) | SyncOperation::Commit(params) => &params.path,
            SyncOperation::Entry(params) => &params.path,
            SyncOperation::Chunk(params) => &params.path,
        };
        let name_value = protocol::text_value(name);
        let path = protocol::inside(&slot.join("source"), Some(&name_value), false)?;
        let name = protocol::relative(Some(&name_value))?;
        require(
            !protocol::under(&name, mac_only),
            "The path stays on the host (macOnly)",
            INVALID,
        )?;
        let entry_file = self
            .opdir(identity)
            .join("entries")
            .join(format!("{}.json", protocol::key(&name)));
        match operation {
            SyncOperation::Delete(_) => match fs::symlink_metadata(&path) {
                Ok(meta) if !meta.is_dir() => fs::remove_file(&path)?,
                Ok(_)
                    if !mac_only
                        .iter()
                        .any(|item| item.starts_with(&format!("{name}/"))) =>
                {
                    // A folder above a Mac-only path stays: the guest deletes its other entries one by one.
                    fs::remove_dir_all(&path)?;
                }
                _ => {}
            },
            SyncOperation::Entry(params) => {
                self.place(op, slot, &params.path, &params.entry, None)?
            }
            SyncOperation::Chunk(params) => {
                require(
                    path.is_file() && !protocol::is_symlink(&path),
                    "The source file is not open",
                    INVALID,
                )?;
                let data = BASE64
                    .decode(&params.data)
                    .map_err(|error| Failure::Unexpected(error.to_string()))?;
                require(
                    data.len() <= protocol::CHUNK,
                    "The source chunk exceeds its limit",
                    INVALID,
                )?;
                let size = fs::metadata(&path)?.len();
                require(
                    params.offset == size,
                    "The source chunk offset differs",
                    INVALID,
                )?;
                let entry: protocol::ManifestEntry =
                    serde_json::from_str(&fs::read_to_string(&entry_file)?)?;
                let protocol::ManifestEntry::File {
                    bytes: declared, ..
                } = entry
                else {
                    return Err(HostExecError::new(INVALID, "Invalid source entry").into());
                };
                require(
                    size + data.len() as u64 <= declared,
                    "The source exceeds its declared size",
                    INVALID,
                )?;
                let mut file = fs::OpenOptions::new().append(true).open(&path)?;
                file.write_all(&data)?;
                file.flush()?;
                file.sync_all()?;
            }
            SyncOperation::Commit(_) => {
                require(
                    !protocol::is_symlink(&path),
                    "The source file is a symbolic link",
                    INVALID,
                )?;
                let entry: protocol::ManifestEntry =
                    serde_json::from_str(&fs::read_to_string(&entry_file)?)?;
                let protocol::ManifestEntry::File {
                    bytes,
                    mode,
                    sha256,
                } = entry
                else {
                    return Err(HostExecError::new(INVALID, "Invalid source entry").into());
                };
                let (checksum, meta) = protocol::digest_file(&path)?;
                require(
                    bytes == meta.len() && sha256 == checksum,
                    "The source checksum differs",
                    INVALID,
                )?;
                require(mode <= 0o777, "Invalid source file mode", INVALID)?;
                set_mode(&path, mode)?;
            }
        }
        Ok(json!({"path": name}))
    }
}
