//! One Mac folder sync round (docs/vm-mac-sync.md, "A sync round"), the Mac side of the
//! last Python release's `codex_vm_mac_sync.py`.
//!
//! The manifest records, for each synced path, the content the Mac and the VM agreed on
//! and the layr state that content came from. A round sends the Mac's changes with those
//! bases, lets the guest fold them into the `mac` line and merge it into main, and then
//! writes main's changes into the Mac folder where the Mac file still has its agreed
//! content. Every displaced inode remains available for recovery after a concurrent
//! save. No conflict markers reach the Mac.
//!
//! Differences from Python (registered in docs/vm-mac-sync.md):
//! - An uncertain guest result keeps the exact pending operation until a proven reply.
//!   Legacy recovery records stop sync until the old operation can be reconciled.
//! - A Mac volume finds a file under another case or Unicode form. The outbound step
//!   resolves names to the spelling on disk, moves the file for a rename in main, and
//!   never deletes or overwrites a file under another spelling.
//! - Any scan error fails the round; a file that cannot be read is never a deletion.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{Cursor, Read, Write};
use std::os::unix::fs::{DirEntryExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_store::path_identity::CanonicalPath;

use super::{
    ABSENT, MAX_SYNC_FILE_BYTES, MacSyncError, contained_path, fetch_entry, hex_sha, lock_round,
    now_seconds, project_directory, project_record, read_json, save_json, upload_archive,
};
use crate::{bounded_child, linux_vm_guest::GuestRpc, tar_gzip::TarGzipWriter};

/// Folders and files that never sync, wherever they are.
const ALWAYS_SKIPPED: [&str; 8] = [
    ".git",
    ".worktrees",
    "node_modules",
    ".venv",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".DS_Store",
];
/// A round that deletes more than this share of the agreed files, or more than this
/// count, waits for the user's confirmation (docs/vm-mac-sync.md, "Safety").
const DELETE_SHARE: f64 = 0.25;
const DELETE_COUNT: usize = 500;
/// Fewer deletions than this never wait.
const DELETE_FLOOR: usize = 3;
/// The guest compares at most this many bases with main.
const MAX_BASES: usize = 16;
const APPLY_TIMEOUT: Duration = Duration::from_secs(1800);
const HASHES_TIMEOUT: Duration = Duration::from_secs(600);
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const GIT_TIMEOUT: Duration = Duration::from_secs(120);
const GIT_OUTPUT_LIMIT: usize = 64 * 1024 * 1024;
/// Where the guest finds a completed upload of a round.
const UPLOAD_ROOT: &str = "/var/lib/codex-studio/projects/mac-sync-";
/// The marker of a temporary file of [`write_mac`]: `.<name>.studio-sync-<8 hex>`.
const TEMPORARY_MARK: &str = ".studio-sync-";
/// Private directories that retain every displaced Mac inode until explicit review.
const PRESERVED_PREFIX: &str = ".studio-sync-preserved-";
/// Format of the durable record beside each retained inode.
const PRESERVATION_VERSION: u64 = 1;
/// Maximum bytes in a Git worktree's metadata-directory pointer file.
const MAX_GIT_DIRECTORY_RECORD_BYTES: u64 = 65_536;
/// Maximum preview attempts to read a stable main state and its hashes.
const PREVIEW_READS: usize = 5;
/// The number of paths per list in a plan.
const PLAN_SAMPLE: usize = 20;

/// One manifest entry (`manifest.json`, the same shape as the Python rounds): kind
/// (`file` or `link`), size, modification time in nanoseconds, permission bits, SHA-256
/// of the content, and the layr state of the agreed content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub k: String,
    #[serde(default)]
    pub s: i64,
    #[serde(default)]
    pub m: i64,
    #[serde(default)]
    pub x: u32,
    pub h: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b: Option<String>,
}

impl Entry {
    /// Content identity and executable permission bits, the values that sync compares.
    fn sync_key(&self) -> (&str, &str, u32) {
        (
            &self.k,
            &self.h,
            if self.k == "file" { self.x & 0o111 } else { 0 },
        )
    }

    fn without_base(&self) -> Self {
        Self {
            b: None,
            ..self.clone()
        }
    }
    fn with_base(&self, base: &str) -> Self {
        Self {
            b: Some(base.to_owned()),
            ..self.clone()
        }
    }
}

pub type Manifest = BTreeMap<String, Entry>;

/// Which Mac paths sync: not Git metadata, caches, dependency folders, or folders Git
/// ignores. Files Git ignores, such as `.env`, do sync.
pub struct Rules {
    ignored: BTreeSet<String>,
    limit: u64,
}

impl Rules {
    /// The rules of a Mac folder. Any Git error, deadline, or output overflow stops
    /// the round before ignored folders can reach the guest.
    pub fn new(root: &Path) -> Result<Self, MacSyncError> {
        let mut ignored = BTreeSet::new();
        if root.join(".git").exists() {
            match bounded_child::run_without_git_environment(bounded_child::Request {
                program: std::ffi::OsStr::new("git"),
                git_directory: Some(root.as_os_str()),
                args: &[
                    "ls-files",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                    "--directory",
                    "-z",
                ],
                environment: &[],
                input: None,
                stdout_limit: GIT_OUTPUT_LIMIT,
                stderr_limit: 4800,
                timeout: GIT_TIMEOUT,
            }) {
                Ok(output) if output.stdout.truncated => {
                    return Err(MacSyncError::Io(std::io::Error::other(
                        "git ls-files listed more ignored paths than the limit",
                    )));
                }
                Ok(output) => {
                    ignored = output
                        .stdout
                        .bytes
                        .split(|byte| *byte == 0)
                        .filter(|item| item.ends_with(b"/"))
                        .map(|item| String::from_utf8_lossy(&item[..item.len() - 1]).into_owned())
                        .collect();
                }
                Err(error) => {
                    return Err(MacSyncError::Io(std::io::Error::other(format!(
                        "git ls-files in the Mac folder: {error:?}"
                    ))));
                }
            }
        }
        Ok(Self {
            ignored,
            limit: MAX_SYNC_FILE_BYTES,
        })
    }

    /// Files larger than `bytes` do not sync.
    pub fn with_file_limit(mut self, bytes: u64) -> Self {
        self.limit = bytes;
        self
    }

    pub fn skipped(&self, rel: &str) -> bool {
        let parts = rel.split('/').collect::<Vec<_>>();
        if parts
            .iter()
            .any(|part| ALWAYS_SKIPPED.contains(part) || is_preserved(part) || is_temporary(part))
        {
            return true;
        }
        (1..=parts.len()).any(|count| self.ignored.contains(&parts[..count].join("/")))
    }
}

/// Whether a name is a temporary file of [`write_mac`].
fn is_temporary(name: &str) -> bool {
    name.starts_with('.')
        && name.rsplit_once(TEMPORARY_MARK).is_some_and(|(_, tail)| {
            tail.len() == 8 && tail.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
}

fn is_preserved(name: &str) -> bool {
    name.strip_prefix(PRESERVED_PREFIX).is_some_and(|suffix| {
        suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    })
}

fn preserved_conflict(rel: &str, directory: &Path) -> MacSyncError {
    MacSyncError::PreservedConflict {
        path: rel.to_owned(),
        recovery: directory.to_path_buf(),
    }
}

/// Inspect a retained inode without changing or removing it. A later editor save to
/// an open descriptor changes this inode, even after the sync syscall returned.
fn inspect_preserved(directory: &Path, rel: &str) -> Result<(), MacSyncError> {
    let record = read_json(&directory.join("record.json"), Value::Null)?;
    if record["version"] == PRESERVATION_VERSION
        && matches!(record["phase"].as_str(), Some("not-applied" | "reviewed"))
    {
        return Ok(());
    }
    let original = record["path"].as_str().unwrap_or(rel);
    if record["version"] != PRESERVATION_VERSION
        || record["phase"] != "applied"
        || record["conflict"] == true
    {
        return Err(preserved_conflict(original, directory));
    }
    if record["displaced"] == true {
        let path = directory.join("original");
        let meta = fs::symlink_metadata(&path).map_err(io)?;
        if !(meta.is_file() || meta.file_type().is_symlink())
            || describe(&path, &meta, true).map_err(io)?.h
                != record["originalHash"].as_str().unwrap_or_default()
        {
            return Err(preserved_conflict(original, directory));
        }
    }
    Ok(())
}

/// Every round checks retained inodes, even when no source event names them. Git
/// worktree metadata can be outside the watched root. Non-Git originals can be
/// beside any formerly synced path, including a path now removed from the manifest.
/// Only recovery content is read; ordinary files need no content scan here.
fn check_retained(root: &Path) -> Result<(), MacSyncError> {
    if let Some(git) = git_directory(root)? {
        for item in fs::read_dir(git).map_err(io)? {
            let item = item.map_err(io)?;
            if is_preserved(&item.file_name().to_string_lossy()) {
                inspect_preserved(&item.path(), "Git metadata")?;
            }
        }
    }
    let mut folders = vec![root.to_path_buf()];
    while let Some(folder) = folders.pop() {
        for item in fs::read_dir(&folder).map_err(io)? {
            let item = item.map_err(io)?;
            let name = item.file_name();
            let name = name.to_string_lossy();
            let path = item.path();
            if is_preserved(&name) {
                inspect_preserved(
                    &path,
                    &path.strip_prefix(root).unwrap_or(&path).to_string_lossy(),
                )?;
            } else if is_temporary(&name) {
                return Err(preserved_conflict(
                    &path.strip_prefix(root).unwrap_or(&path).to_string_lossy(),
                    &path,
                ));
            } else if !ALWAYS_SKIPPED.contains(&name.as_ref())
                && item.file_type().map_err(io)?.is_dir()
            {
                folders.push(path);
            }
        }
    }
    Ok(())
}

fn io(error: std::io::Error) -> MacSyncError {
    MacSyncError::Io(error)
}

fn not_found(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::NotFound
}

fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(meta.mtime_nsec())
}

/// Identity and change times of a Mac path before the guest request.
#[derive(PartialEq, Eq)]
struct FileStamp(u64, u64, i64, i64, i64, i64, u64, u32);

impl FileStamp {
    fn read(path: &Path) -> std::io::Result<Option<Self>> {
        match fs::symlink_metadata(path) {
            Ok(meta) => Ok(Some(Self::from_metadata(&meta))),
            Err(error) if not_found(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn from_metadata(meta: &fs::Metadata) -> Self {
        Self(
            meta.dev(),
            meta.ino(),
            meta.mtime(),
            meta.mtime_nsec(),
            meta.ctime(),
            meta.ctime_nsec(),
            meta.len(),
            meta.mode(),
        )
    }

    /// Our rename changes ctime. Identity, mtime, size, and mode must still match.
    fn same_after_move(&self, other: &Self) -> bool {
        (self.0, self.1, self.2, self.3, self.6, self.7)
            == (other.0, other.1, other.2, other.3, other.6, other.7)
    }
}

fn open_no_follow(path: &Path) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(File::from(descriptor))
}

fn file_hash(path: &Path) -> std::io::Result<String> {
    let mut file = open_no_follow(path)?;
    hash_file(&mut file)
}

fn hash_file(file: &mut File) -> std::io::Result<String> {
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(data_encoding::HEXLOWER.encode(&digest.finalize()))
}

fn link_target(path: &Path) -> std::io::Result<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    Ok(fs::read_link(path)?.as_os_str().as_bytes().to_vec())
}

/// The entry of a file or link; `digest` false keeps the hash empty for the caller to fill.
fn describe(path: &Path, meta: &fs::Metadata, digest: bool) -> std::io::Result<Entry> {
    if meta.file_type().is_symlink() {
        let target = link_target(path)?;
        return Ok(Entry {
            k: "link".into(),
            s: target.len() as i64,
            m: mtime_ns(meta),
            x: 0,
            h: hex_sha(&target),
            b: None,
        });
    }
    Ok(Entry {
        k: "file".into(),
        s: meta.len() as i64,
        m: mtime_ns(meta),
        x: meta.permissions().mode() & 0o777,
        h: if digest {
            file_hash(path)?
        } else {
            String::new()
        },
        b: None,
    })
}

/// The names in the Mac folders, with their file IDs, read once per round. A default
/// Mac volume finds a file under another case or Unicode form of its name; these
/// listings tell the spelling on disk.
#[derive(Default)]
struct Listings(HashMap<PathBuf, Vec<(OsString, u64)>>);

impl Listings {
    fn names(&mut self, folder: &Path) -> std::io::Result<&[(OsString, u64)]> {
        if !self.0.contains_key(folder) {
            let mut names = Vec::new();
            for item in fs::read_dir(folder)? {
                let item = item?;
                names.push((item.file_name(), item.ino()));
            }
            self.0.insert(folder.to_path_buf(), names);
        }
        Ok(self.0.get(folder).map(Vec::as_slice).unwrap_or_default())
    }

    fn forget(&mut self, path: &Path) {
        if let Some(parent) = path.parent() {
            self.0.remove(parent);
        }
    }

    /// The spelling on disk of the file that `rel` reaches, `None` when nothing does.
    fn actual(&mut self, root: &Path, rel: &str) -> std::io::Result<Option<String>> {
        let mut folder = root.to_path_buf();
        let mut parts = Vec::new();
        for component in rel.split('/') {
            let meta = match fs::symlink_metadata(folder.join(component)) {
                Ok(meta) => meta,
                Err(error) if not_found(&error) || error.raw_os_error() == Some(libc::ENOTDIR) => {
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            let names = self.names(&folder)?;
            let name = names
                .iter()
                .find(|(name, _)| name.as_os_str() == component)
                .or_else(|| names.iter().find(|(_, id)| *id == meta.ino()))
                .map(|(name, _)| name.clone());
            let Some(name) = name else {
                return Ok(None);
            };
            parts.push(name.to_string_lossy().into_owned());
            folder.push(name);
        }
        Ok(Some(parts.join("/")))
    }
}

/// How a scan treats the Mac folder.
struct Scan<'a> {
    root: &'a Path,
    rules: &'a Rules,
    manifest: &'a Manifest,
    entries: Manifest,
    notices: Vec<String>,
}

impl Scan<'_> {
    /// One path: its new entry, a notice, or nothing when it vanished. Any other error
    /// fails the round: a path that cannot be read is never a deletion.
    fn one(&mut self, path: &Path, rel: &str) -> Result<(), MacSyncError> {
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(error) if not_found(&error) => return Ok(()),
            Err(error) => return Err(io(error)),
        };
        let kind = meta.file_type();
        if !(kind.is_file() || kind.is_symlink()) {
            self.notices
                .push(format!("{rel}: a special file does not sync"));
            return Ok(());
        }
        let known = self.manifest.get(rel);
        if kind.is_file() && meta.len() > self.rules.limit {
            self.notices
                .push(format!("{rel}: files over 256 MiB do not sync"));
            if let Some(known) = known {
                // Too large to send now, but not deleted.
                self.entries.insert(rel.to_owned(), known.without_base());
            }
            return Ok(());
        }
        let same = known.is_some_and(|known| {
            known.k == if kind.is_symlink() { "link" } else { "file" }
                && known.s == meta.len() as i64
                && known.m == mtime_ns(&meta)
        });
        match describe(path, &meta, !same) {
            Ok(mut entry) => {
                if same && let Some(known) = known {
                    entry.h = known.h.clone();
                }
                self.entries.insert(rel.to_owned(), entry);
                Ok(())
            }
            Err(error) if not_found(&error) => Ok(()),
            Err(error) => Err(io(error)),
        }
    }

    fn folder(&mut self, start: &Path) -> Result<(), MacSyncError> {
        let mut folders = vec![start.to_path_buf()];
        while let Some(folder) = folders.pop() {
            let prefix = match folder.strip_prefix(self.root) {
                Ok(relative) if relative.as_os_str().is_empty() => String::new(),
                Ok(relative) => format!("{}/", relative.to_string_lossy()),
                Err(_) => continue,
            };
            let listing = match fs::read_dir(&folder) {
                Ok(listing) => listing,
                Err(error) if not_found(&error) && folder != self.root => continue,
                Err(error) => return Err(io(error)),
            };
            let mut names = Vec::new();
            for item in listing {
                names.push(item.map_err(io)?.file_name().to_string_lossy().into_owned());
            }
            names.sort();
            let mut below = Vec::new();
            for name in names {
                let rel = format!("{prefix}{name}");
                let path = folder.join(&name);
                if is_temporary(&name) {
                    // Older versions also used this name for the sole original of a
                    // rename. Its contents are uncertain and must never be discarded.
                    return Err(preserved_conflict(&rel, &path));
                }
                if is_preserved(&name) {
                    inspect_preserved(&path, &rel)?;
                    self.notices.push(format!(
                        "{rel}: retained Mac data stays here until explicit review"
                    ));
                    continue;
                }
                if self.rules.skipped(&rel) {
                    continue;
                }
                match fs::symlink_metadata(&path) {
                    Ok(meta) if meta.is_dir() => {
                        below.push(path);
                        continue;
                    }
                    Ok(_) => {}
                    Err(error) if not_found(&error) => continue,
                    Err(error) => return Err(io(error)),
                }
                self.one(&path, &rel)?;
            }
            folders.extend(below.into_iter().rev());
        }
        Ok(())
    }
}

/// The Mac's synced paths (below `start` when given). A file whose size and time match
/// the manifest keeps its recorded hash; others are hashed. Returns (entries, notices).
pub fn scan(
    root: &Path,
    rules: &Rules,
    manifest: &Manifest,
    start: Option<&Path>,
) -> Result<(Manifest, Vec<String>), MacSyncError> {
    scan_folder(root, rules, manifest, start)
}

fn scan_folder(
    root: &Path,
    rules: &Rules,
    manifest: &Manifest,
    start: Option<&Path>,
) -> Result<(Manifest, Vec<String>), MacSyncError> {
    let mut scan = Scan {
        root,
        rules,
        manifest,
        entries: Manifest::new(),
        notices: Vec::new(),
    };
    if let Some(git) = git_directory(root)? {
        for item in fs::read_dir(&git).map_err(io)? {
            let item = item.map_err(io)?;
            if is_preserved(&item.file_name().to_string_lossy()) {
                inspect_preserved(&item.path(), "Git metadata")?;
                scan.notices
                    .push(format!("retained Mac originals: {}", item.path().display()));
            }
        }
    }
    scan.folder(start.unwrap_or(root))?;
    Ok((scan.entries, scan.notices))
}

/// The Mac's state with only `paths` (and the folders among them) read again: every
/// other path keeps its manifest entry. File events name what changed. A named path
/// counts under its own spelling only; the spelling on disk is read as well, so a
/// rename in case or Unicode form is a deletion and a new file.
pub fn scan_paths(
    root: &Path,
    rules: &Rules,
    manifest: &Manifest,
    paths: &BTreeSet<String>,
) -> Result<(Manifest, Vec<String>), MacSyncError> {
    let mut listings = Listings::default();
    let mut named = BTreeSet::new();
    for rel in paths {
        if rel.is_empty() || rel.starts_with("../") || rel.split('/').any(|part| part == "..") {
            continue;
        }
        let mut prefix = PathBuf::new();
        let mut retained = false;
        for component in rel.split('/') {
            prefix.push(component);
            if is_temporary(component) {
                return Err(preserved_conflict(rel, &root.join(prefix)));
            }
            if is_preserved(component) {
                inspect_preserved(&contained_path(root, &prefix.to_string_lossy())?, rel)?;
                retained = true;
                break;
            }
        }
        if retained {
            continue;
        }
        named.insert(rel.clone());
        if let Some(actual) = listings.actual(root, rel).map_err(io)? {
            named.insert(actual);
        }
    }
    let mut covered: Vec<&String> = Vec::new();
    for rel in &named {
        if !covered
            .iter()
            .any(|other| rel.starts_with(&format!("{other}/")))
        {
            covered.push(rel);
        }
    }
    let under = |rel: &str| {
        covered
            .iter()
            .any(|item| rel == item.as_str() || rel.starts_with(&format!("{item}/")))
    };
    let entries = manifest
        .iter()
        .filter(|(rel, _)| !under(rel))
        .map(|(rel, known)| (rel.clone(), known.without_base()))
        .collect::<Manifest>();
    let mut scan = Scan {
        root,
        rules,
        manifest,
        entries,
        notices: Vec::new(),
    };
    for rel in covered {
        if rules.skipped(rel) || listings.actual(root, rel).map_err(io)?.as_deref() != Some(rel) {
            // Not on disk under this spelling.
            continue;
        }
        let path = root.join(rel);
        let mut parent = path.parent();
        let mut linked = false;
        while let Some(folder) = parent.filter(|folder| *folder != root) {
            linked |= fs::symlink_metadata(folder).is_ok_and(|meta| meta.file_type().is_symlink());
            parent = folder.parent();
        }
        if linked {
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => scan.folder(&path)?,
            Ok(_) => scan.one(&path, rel)?,
            Err(error) if not_found(&error) => {}
            Err(error) => return Err(io(error)),
        }
    }
    Ok((scan.entries, scan.notices))
}

/// The result of a guarded write.
enum Written {
    /// The new manifest entry, `None` after a delete.
    Done(Option<Entry>, Option<PreservedOutcome>),
    /// The Mac file no longer had the expected content: nothing changed.
    Changed,
}

struct PreservedOutcome {
    directory: PathBuf,
    changed: bool,
}

/// Holds both directory descriptors across the final guard and the atomic operation.
/// No displaced inode is unlinked, including after an error or a later editor save.
struct PreservedWrite {
    parent: File,
    directory: File,
    name: OsString,
    location: PathBuf,
    record: Value,
}

fn open_directory(path: &Path) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(Into::into)
}

/// Resolve Git metadata without running a command per file. Retained .env bytes must
/// never become untracked files that `git add -A` can publish.
fn git_directory(root: &Path) -> Result<Option<PathBuf>, MacSyncError> {
    let dot = root.join(".git");
    let meta = match fs::symlink_metadata(&dot) {
        Ok(meta) => meta,
        Err(error) if not_found(&error) => return Ok(None),
        Err(error) => return Err(io(error)),
    };
    let path = if meta.is_file() {
        let mut text = String::new();
        open_no_follow(&dot)
            .map_err(io)?
            .take(MAX_GIT_DIRECTORY_RECORD_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(io)?;
        if text.len() as u64 > MAX_GIT_DIRECTORY_RECORD_BYTES {
            return Err(MacSyncError::Invalid);
        }
        let value = text
            .strip_prefix("gitdir: ")
            .ok_or(MacSyncError::Invalid)?
            .trim_end_matches(['\r', '\n']);
        if value.is_empty() || value.contains(['\r', '\n']) {
            return Err(MacSyncError::Invalid);
        }
        root.join(value)
    } else {
        dot
    };
    let directory = CanonicalPath::existing(&path).map_err(io)?.to_path_buf();
    if !directory.is_dir() {
        return Err(MacSyncError::Invalid);
    }
    Ok(Some(directory))
}

impl PreservedWrite {
    fn new(
        root: &Path,
        rel: &str,
        kind: &str,
        expected: Option<&str>,
    ) -> Result<Self, MacSyncError> {
        use rustix::fs::{Mode, OFlags};
        let path = Path::new(rel);
        let mut parent = open_directory(root).map_err(io)?;
        let relative_parent = path.parent().ok_or(MacSyncError::Invalid)?;
        for part in relative_parent.components() {
            let part = part.as_os_str();
            match rustix::fs::openat(
                &parent,
                part,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => parent = File::from(fd),
                Err(rustix::io::Errno::NOENT) => {
                    match rustix::fs::mkdirat(&parent, part, Mode::from_raw_mode(0o755)) {
                        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
                        Err(error) => return Err(io(error.into())),
                    }
                    parent = File::from(
                        rustix::fs::openat(
                            &parent,
                            part,
                            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::empty(),
                        )
                        .map_err(|error| io(error.into()))?,
                    );
                }
                Err(error) => return Err(io(error.into())),
            }
        }
        let (base, location) = match git_directory(root)? {
            Some(git) => (open_directory(&git).map_err(io)?, git),
            None => (parent.try_clone().map_err(io)?, root.join(relative_parent)),
        };
        if base.metadata().map_err(io)?.dev() != parent.metadata().map_err(io)?.dev() {
            return Err(io(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "Mac sync requires recovery storage on the target filesystem",
            )));
        }
        let directory_name = format!("{PRESERVED_PREFIX}{}", uuid::Uuid::new_v4().simple());
        rustix::fs::mkdirat(&base, &directory_name, Mode::from_raw_mode(0o700))
            .map_err(|error| io(error.into()))?;
        let directory = File::from(
            rustix::fs::openat(
                &base,
                &directory_name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|error| io(error.into()))?,
        );
        // This also protects a non-Git folder if the user later starts Git here.
        // Create the exclusion before any original or VM content enters the directory.
        let mut ignore = File::from(
            rustix::fs::openat(
                &directory,
                ".gitignore",
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|error| io(error.into()))?,
        );
        ignore.write_all(b"*\n").map_err(io)?;
        ignore.sync_all().map_err(io)?;
        directory.sync_all().map_err(io)?;
        base.sync_all().map_err(io)?;
        let value = Self {
            parent,
            directory,
            name: path.file_name().ok_or(MacSyncError::Invalid)?.to_owned(),
            location: location.join(directory_name),
            record: json!({"version":PRESERVATION_VERSION,"phase":"prepared","path":rel,"requestedKind":kind,
                "expectedHash":expected,"at":now_seconds()}),
        };
        value.save_record()?;
        Ok(value)
    }

    fn save_record(&self) -> Result<(), MacSyncError> {
        use rustix::fs::{Mode, OFlags};
        let mut file = File::from(
            rustix::fs::openat(
                &self.directory,
                "record.next",
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )
            .map_err(|error| io(error.into()))?,
        );
        file.write_all(&serde_json::to_vec(&self.record).map_err(MacSyncError::Json)?)
            .map_err(io)?;
        file.sync_all().map_err(io)?;
        rustix::fs::renameat(
            &self.directory,
            "record.next",
            &self.directory,
            "record.json",
        )
        .map_err(|error| io(error.into()))?;
        self.directory.sync_all().map_err(io)
    }

    fn stage(&self, kind: &str, data: &[u8], mode: u32) -> Result<Entry, MacSyncError> {
        use rustix::fs::{Mode, OFlags};
        let (mtime, mode) = if kind == "link" {
            rustix::fs::symlinkat(
                std::str::from_utf8(data).map_err(|_| MacSyncError::Invalid)?,
                &self.directory,
                "original",
            )
            .map_err(|error| io(error.into()))?;
            (0, 0)
        } else {
            let mut file = File::from(
                rustix::fs::openat(
                    &self.directory,
                    "original",
                    OFlags::WRONLY
                        | OFlags::CREATE
                        | OFlags::EXCL
                        | OFlags::NOFOLLOW
                        | OFlags::CLOEXEC,
                    Mode::from_raw_mode(0o600),
                )
                .map_err(|error| io(error.into()))?,
            );
            file.write_all(data).map_err(io)?;
            let mode = if mode == 0 { 0o644 } else { mode & 0o777 };
            file.set_permissions(fs::Permissions::from_mode(mode))
                .map_err(io)?;
            file.sync_all().map_err(io)?;
            (mtime_ns(&file.metadata().map_err(io)?), mode)
        };
        self.directory.sync_all().map_err(io)?;
        Ok(Entry {
            k: kind.to_owned(),
            s: data.len() as i64,
            m: mtime,
            x: mode,
            h: hex_sha(data),
            b: None,
        })
    }

    fn finish(
        &mut self,
        displaced: bool,
        expected: Option<(&str, &Option<FileStamp>)>,
    ) -> Result<bool, MacSyncError> {
        use rustix::fs::{AtFlags, FileType, Mode, OFlags};
        let mut changed = false;
        if displaced {
            let stat = rustix::fs::statat(&self.directory, "original", AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|error| io(error.into()))?;
            if let Some((_, Some(before))) = expected {
                changed |= before.0 != stat.st_dev as u64 || before.1 != stat.st_ino as u64;
            }
            let (hash, stamp) = match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => {
                    let mut file = File::from(
                        rustix::fs::openat(
                            &self.directory,
                            "original",
                            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                            Mode::empty(),
                        )
                        .map_err(|error| io(error.into()))?,
                    );
                    file.sync_all().map_err(io)?;
                    let stamp = FileStamp::from_metadata(&file.metadata().map_err(io)?);
                    (hash_file(&mut file).map_err(io)?, Some(stamp))
                }
                FileType::Symlink => (
                    hex_sha(
                        rustix::fs::readlinkat(&self.directory, "original", Vec::new())
                            .map_err(|error| io(error.into()))?
                            .as_bytes(),
                    ),
                    None,
                ),
                _ => {
                    changed = true;
                    (String::new(), None)
                }
            };
            if let Some((expected_hash, before)) = expected {
                changed |= hash != expected_hash;
                if let (Some(before), Some(after)) = (before, &stamp) {
                    changed |= !before.same_after_move(after);
                }
            }
            self.record["originalHash"] = json!(hash);
        }
        self.directory.sync_all().map_err(io)?;
        self.parent.sync_all().map_err(io)?;
        self.record["phase"] = json!("applied");
        self.record["displaced"] = json!(displaced);
        self.record["conflict"] = json!(changed);
        self.save_record()?;
        Ok(changed)
    }

    fn not_applied(&mut self) -> Result<(), MacSyncError> {
        self.record["phase"] = json!("not-applied");
        self.save_record()
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn atomic_move(
    from_dir: &File,
    from: &std::ffi::OsStr,
    to_dir: &File,
    to: &std::ffi::OsStr,
    exchange: bool,
) -> std::io::Result<()> {
    use rustix::fs::{RenameFlags, renameat_with};
    renameat_with(
        from_dir,
        from,
        to_dir,
        to,
        if exchange {
            RenameFlags::EXCHANGE
        } else {
            RenameFlags::NOREPLACE
        },
    )
    .map_err(Into::into)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn atomic_move(
    _from_dir: &File,
    _from: &std::ffi::OsStr,
    _to_dir: &File,
    _to: &std::ffi::OsStr,
    _exchange: bool,
) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "Mac sync requires atomic exchange and no-replace rename",
    ))
}

/// The hash of the Mac file under exactly this spelling, `ABSENT` when it has none.
fn current_hash(listings: &mut Listings, root: &Path, rel: &str) -> std::io::Result<String> {
    if listings.actual(root, rel)?.as_deref() != Some(rel) {
        return Ok(ABSENT.to_owned());
    }
    let path = root.join(rel);
    match fs::symlink_metadata(&path) {
        Ok(meta) => describe(&path, &meta, true).map(|entry| entry.h),
        Err(error) if not_found(&error) => Ok(ABSENT.to_owned()),
        Err(error) => Err(error),
    }
}

/// Replace one Mac path atomically; returns its new manifest entry (`None` after a
/// delete). Writes never follow a link and never leave the folder.
pub fn write_mac(
    root: &Path,
    rel: &str,
    kind: &str,
    data: &[u8],
    mode: u32,
) -> Result<Option<Entry>, MacSyncError> {
    match write_guarded(root, rel, kind, data, mode, None, &mut Listings::default())? {
        Written::Done(entry, _) => Ok(entry),
        Written::Changed => Err(MacSyncError::Invalid),
    }
}

/// [`write_mac`] that first checks, right before the rename or the delete, that the Mac
/// file under exactly this spelling still has the `expected` hash (`ABSENT` for none).
fn write_guarded(
    root: &Path,
    rel: &str,
    kind: &str,
    data: &[u8],
    mode: u32,
    expected: Option<(&str, &Option<FileStamp>)>,
    listings: &mut Listings,
) -> Result<Written, MacSyncError> {
    write_guarded_with_hook(root, rel, kind, data, mode, expected, listings, &mut || {})
}

#[allow(clippy::too_many_arguments)]
fn write_guarded_with_hook(
    root: &Path,
    rel: &str,
    kind: &str,
    data: &[u8],
    mode: u32,
    expected: Option<(&str, &Option<FileStamp>)>,
    listings: &mut Listings,
    before_apply: &mut dyn FnMut(),
) -> Result<Written, MacSyncError> {
    let path = contained_path(root, rel)?;
    if !matches!(kind, "file" | "link" | ABSENT) {
        return Err(MacSyncError::Invalid);
    }
    let unchanged = |listings: &mut Listings| -> Result<bool, MacSyncError> {
        listings.forget(&path);
        Ok(match expected {
            None => true,
            Some((expected, stamp)) => {
                current_hash(listings, root, rel).map_err(io)? == expected
                    && FileStamp::read(&path).map_err(io)? == *stamp
            }
        })
    };
    if !unchanged(listings)? {
        return Ok(Written::Changed);
    }
    if listings
        .actual(root, rel)
        .map_err(io)?
        .is_some_and(|actual| actual != rel)
    {
        return Ok(Written::Changed);
    }
    let present = FileStamp::read(&path).map_err(io)?.is_some();
    if kind == ABSENT && !present {
        return Ok(Written::Done(None, None));
    }
    let mut retained = PreservedWrite::new(root, rel, kind, expected.map(|(hash, _)| hash))?;
    let entry = if kind == ABSENT {
        None
    } else {
        match retained.stage(kind, data, mode) {
            Ok(entry) => Some(entry),
            Err(error) => {
                retained.not_applied()?;
                return Err(error);
            }
        }
    };
    // These checks reduce conflicts. Only the atomic operation preserves the inode
    // that actually occupies the destination after an uncoordinated editor save.
    match unchanged(listings) {
        Ok(true) => {}
        Ok(false) => {
            retained.not_applied()?;
            return Ok(Written::Changed);
        }
        Err(error) => {
            retained.not_applied()?;
            return Err(error);
        }
    }
    before_apply();
    let original = std::ffi::OsStr::new("original");
    let result = if kind == ABSENT {
        atomic_move(
            &retained.parent,
            &retained.name,
            &retained.directory,
            original,
            false,
        )
    } else {
        atomic_move(
            &retained.directory,
            original,
            &retained.parent,
            &retained.name,
            present,
        )
    };
    if let Err(error) = result {
        if not_found(&error) || error.kind() == std::io::ErrorKind::AlreadyExists {
            retained.not_applied()?;
            return Ok(Written::Changed);
        }
        // Do not remove anything after an uncertain filesystem error.
        return Err(io(error));
    }
    let displaced = present;
    let changed = retained.finish(displaced, expected)?;
    listings.forget(&path);
    Ok(Written::Done(
        entry,
        displaced.then_some(PreservedOutcome {
            directory: retained.location,
            changed,
        }),
    ))
}

fn string_set(value: &Value) -> BTreeSet<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

/// One change of a round: the path, its Mac entry (`None` for a deletion), and the base
/// of its agreed content.
type Change = (String, Option<Entry>, Option<String>);

/// What the outbound step did: agreed without a write, written, skipped, the main
/// deletions that wait for a confirmation, and notices.
struct Outbound {
    agreed: BTreeSet<String>,
    written: BTreeSet<String>,
    skipped: BTreeSet<String>,
    removals: usize,
    notices: Vec<String>,
    preserved: Vec<Value>,
}

/// The sync of one VM project with its Mac folder.
pub struct ProjectSync<'a> {
    guest: &'a dyn GuestRpc,
    project_id: String,
    root: PathBuf,
    directory: PathBuf,
    import_state: Option<String>,
    file_limit: u64,
}

impl<'a> ProjectSync<'a> {
    /// The sync of `layr-projects/<project_id>` in the VM state directory.
    pub fn open(
        guest: &'a dyn GuestRpc,
        state_dir: &Path,
        project_id: &str,
    ) -> Result<Self, MacSyncError> {
        let project = project_directory(state_dir, project_id)?;
        let record = project_record(&project)?;
        let source = record["source"].as_str().ok_or(MacSyncError::NoProject)?;
        let root = CanonicalPath::existing(Path::new(source))
            .map_err(io)?
            .to_path_buf();
        Ok(Self {
            guest,
            project_id: project_id.to_owned(),
            root,
            directory: project.join("sync"),
            import_state: record["importStateId"].as_str().map(str::to_owned),
            file_limit: MAX_SYNC_FILE_BYTES,
        })
    }

    /// Files larger than `bytes` do not sync.
    pub fn with_file_limit(mut self, bytes: u64) -> Self {
        self.file_limit = bytes;
        self
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// The Mac folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder of the sync records (`sync/`).
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.join(name)
    }

    fn rules(&self) -> Result<Rules, MacSyncError> {
        Ok(Rules::new(&self.root)?.with_file_limit(self.file_limit))
    }

    /// Merge `values` into `status.json` with the current time; returns the new status.
    pub fn record_status(&self, values: Value) -> Result<Value, MacSyncError> {
        fs::create_dir_all(&self.directory).map_err(io)?;
        let mut current = read_json(&self.path("status.json"), json!({}))?;
        if !current.is_object() {
            current = json!({});
        }
        if let Value::Object(values) = values {
            for (key, value) in values {
                current[key] = value;
            }
        }
        current["at"] = json!(now_seconds());
        save_json(&self.path("status.json"), &current)?;
        Ok(current)
    }

    /// One round. `paths` limits the Mac scan to what file events named, and an empty set
    /// reads nothing on the Mac (main changed); `None` scans the whole folder.
    ///
    /// An uncertain reply keeps the exact operation and payload. A later round repeats
    /// the same request. A definite guest refusal ends the operation.
    pub fn round(&self, paths: Option<&BTreeSet<String>>) -> Result<Value, MacSyncError> {
        let _lock = lock_round(&self.directory)?;
        let result = self.round_locked(paths);
        if let Err(error) = &result
            && let MacSyncError::PreservedConflict { path, recovery } = error
        {
            self.record_status(json!({"state":"error","reason":super::error_reason(error),
                "preservedConflicts":[{"path":path,"recovery":recovery}]}))?;
        }
        result
    }

    fn load_manifest(&self) -> Result<Option<Manifest>, MacSyncError> {
        let value = read_json(&self.path("manifest.json"), Value::Null)?;
        if value.is_null() {
            return Ok(None);
        }
        serde_json::from_value(value)
            .map(Some)
            .map_err(MacSyncError::Json)
    }

    fn save_manifest(&self, manifest: &Manifest) -> Result<(), MacSyncError> {
        save_json(
            &self.path("manifest.json"),
            &serde_json::to_value(manifest).map_err(MacSyncError::Json)?,
        )
    }

    fn load_object(&self, name: &str) -> Result<Map<String, Value>, MacSyncError> {
        Ok(match read_json(&self.path(name), json!({}))? {
            Value::Object(map) => map,
            _ => Map::new(),
        })
    }

    fn remove(&self, name: &str) -> Result<(), MacSyncError> {
        match fs::remove_file(self.path(name)) {
            Ok(()) => Ok(()),
            Err(error) if not_found(&error) => Ok(()),
            Err(error) => Err(io(error)),
        }
    }

    fn head(&self) -> Result<String, MacSyncError> {
        let head = self
            .guest
            .call(
                "sync.mac.head",
                &json!({"projectId": self.project_id}),
                None,
                HEAD_TIMEOUT,
            )
            .map_err(MacSyncError::Guest)?;
        head["mainStateId"]
            .as_str()
            .map(str::to_owned)
            .ok_or(MacSyncError::Invalid)
    }

    /// Every synced path of a layr state that `keep` accepts, with its guest description.
    /// The guest reads it under its project lock, after any apply that runs.
    fn state_hashes(
        &self,
        state: &str,
        keep: impl Fn(&str) -> bool,
        read_only: bool,
    ) -> Result<BTreeMap<String, Value>, MacSyncError> {
        let mut known = BTreeMap::new();
        let mut after = String::new();
        loop {
            let page = self
                .guest
                .call(
                    if read_only {
                        "sync.mac.preview"
                    } else {
                        "sync.mac.hashes"
                    },
                    &json!({"projectId": self.project_id, "stateId": state, "after": after}),
                    None,
                    HASHES_TIMEOUT,
                )
                .map_err(MacSyncError::Guest)?;
            if read_only && page["readOnly"] != true {
                return Err(MacSyncError::Invalid);
            }
            for (rel, item) in page["items"].as_object().into_iter().flatten() {
                if keep(rel) {
                    known.insert(rel.clone(), item.clone());
                }
            }
            match page["next"].as_str() {
                Some(next) if !next.is_empty() && next > after.as_str() => after = next.to_owned(),
                _ => break,
            }
        }
        Ok(known)
    }

    /// Main's head and the hashes of the paths `keep` accepts. The hashes wait for any
    /// running apply; when main moved meanwhile, both are read again.
    fn settled_main(
        &self,
        keep: impl Fn(&str) -> bool,
    ) -> Result<(String, BTreeMap<String, Value>), MacSyncError> {
        let mut head = self.head()?;
        for _ in 0..PREVIEW_READS {
            let main = self.state_hashes(&head, &keep, true)?;
            let after = self.head()?;
            if after == head {
                return Ok((head, main));
            }
            head = after;
        }
        Err(MacSyncError::Io(std::io::Error::other(
            "main kept moving while the sync read it",
        )))
    }

    /// The first manifest. With the import state, its content is what the Mac and the VM
    /// agreed on: later Mac edits then go in, and agent work comes out. Without it, only
    /// files equal to main are agreed; every other difference waits for a choice.
    fn bootstrap(&self, rules: &Rules) -> Result<Manifest, MacSyncError> {
        let ensured = self
            .guest
            .call(
                "project.ensure",
                &json!({"projectId": self.project_id}),
                None,
                HEAD_TIMEOUT,
            )
            .map_err(MacSyncError::Guest)?;
        let main_state = ensured["stateId"]
            .as_str()
            .ok_or(MacSyncError::Invalid)?
            .to_owned();
        let base = self.import_state.clone().unwrap_or(main_state);
        let recorded = self
            .state_hashes(&base, |rel| !rules.skipped(rel), false)?
            .into_iter()
            .filter_map(|(rel, item)| {
                let kind = item["kind"].as_str()?;
                matches!(kind, "file" | "link").then(|| {
                    let entry = Entry {
                        k: kind.to_owned(),
                        s: item["size"].as_i64().unwrap_or(0),
                        m: 0,
                        x: item["mode"].as_u64().unwrap_or(0) as u32,
                        h: item["sha256"].as_str().unwrap_or_default().to_owned(),
                        b: Some(base.clone()),
                    };
                    (rel, entry)
                })
            })
            .collect::<Manifest>();
        if self.import_state.is_some() {
            return Ok(recorded);
        }
        let (current, _) = scan(&self.root, rules, &Manifest::new(), None)?;
        let manifest = recorded
            .iter()
            .filter(|(rel, entry)| {
                current
                    .get(*rel)
                    .is_some_and(|found| found.k == entry.k && found.h == entry.h)
            })
            .map(|(rel, entry)| (rel.clone(), entry.clone()))
            .collect::<Manifest>();
        let waiting = current
            .keys()
            .chain(recorded.keys())
            .filter(|rel| !manifest.contains_key(*rel))
            .cloned()
            .collect::<BTreeSet<_>>();
        if !waiting.is_empty() {
            self.record_status(json!({"choices": waiting}))?;
        }
        Ok(manifest)
    }

    /// A legacy recovery record lacks the original payload. Hashes cannot establish
    /// that the old operation has stopped, so it cannot authorize a new operation.
    fn check_legacy_recovery(&self) -> Result<(), MacSyncError> {
        let record = read_json(&self.path("recover.json"), Value::Null)?;
        if !record.is_null() {
            return Err(MacSyncError::Unproven {
                operation: record["operation"].as_str().unwrap_or("unknown").to_owned(),
                paths: string_set(&record["paths"]).len(),
            });
        }
        Ok(())
    }

    /// What the next round would move, without a write on either side: the Mac folder
    /// and the sync records stay as they are, and the guest gets only reads
    /// (`sync.mac.head`, `sync.mac.preview`). Each list has a count and its first
    /// [`PLAN_SAMPLE`] paths. The round's guards then decide what waits.
    pub fn plan(&self) -> Result<Value, MacSyncError> {
        // Open an existing lock for reads. A preview never creates a sync record.
        let lock = match File::open(self.path("round.lock")) {
            Ok(file) => Some(
                nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockSharedNonblock).map_err(
                    |(_, error)| {
                        if error == nix::errno::Errno::EWOULDBLOCK
                            || error == nix::errno::Errno::EAGAIN
                        {
                            MacSyncError::Busy
                        } else {
                            io(std::io::Error::from_raw_os_error(error as i32))
                        }
                    },
                )?,
            ),
            Err(error) if not_found(&error) => None,
            Err(error) => return Err(io(error)),
        };
        let rules = self.rules()?;
        let manifest = self.load_manifest()?;
        let pending = read_json(&self.path("pending.json"), Value::Null)?;
        let status = read_json(&self.path("status.json"), json!({}))?;
        let held = self.load_object("held.json")?;
        let waiting = string_set(&status["choices"]);
        let pending_view = (!pending.is_null()).then(|| {
            json!({
                "operation": pending["operation"],
                "entries": pending["params"]["entries"].as_array().map_or(0, Vec::len),
            })
        });
        let first_round = manifest.is_none();
        let (head, main) = self.settled_main(|rel| !rules.skipped(rel))?;
        let manifest = match manifest {
            Some(manifest) => manifest,
            None => {
                let (base, recorded) = match &self.import_state {
                    Some(base) if *base != head => (
                        base.clone(),
                        self.state_hashes(base, |rel| !rules.skipped(rel), true)?,
                    ),
                    _ => (head.clone(), main.clone()),
                };
                recorded
                    .iter()
                    .filter_map(|(rel, item)| {
                        let kind = item["kind"].as_str()?;
                        matches!(kind, "file" | "link").then(|| {
                            (
                                rel.clone(),
                                Entry {
                                    k: kind.to_owned(),
                                    s: item["size"].as_i64().unwrap_or(0),
                                    m: 0,
                                    x: item["mode"].as_u64().unwrap_or(0) as u32,
                                    h: item["sha256"].as_str().unwrap_or_default().to_owned(),
                                    b: Some(base.clone()),
                                },
                            )
                        })
                    })
                    .collect()
            }
        };
        let (current, notices) = scan_folder(&self.root, &rules, &manifest, None)?;
        let main_key = |rel: &str| {
            main.get(rel)
                .filter(|item| matches!(item["kind"].as_str(), Some("file" | "link")))
                .and_then(|item| {
                    Some((
                        item["kind"].as_str()?,
                        item["sha256"].as_str()?,
                        if item["kind"] == "file" {
                            item["mode"].as_u64().unwrap_or(0) as u32 & 0o111
                        } else {
                            0
                        },
                    ))
                })
        };
        let mut lists: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        let paths = manifest
            .keys()
            .chain(current.keys())
            .chain(main.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        for rel in paths {
            if rules.skipped(&rel) || waiting.contains(&rel) {
                continue;
            }
            let agreed = manifest.get(&rel).map(Entry::sync_key);
            let mac = current.get(&rel).map(Entry::sync_key);
            let vm = main_key(&rel);
            let mac_hash = current.get(&rel).map_or(ABSENT, |entry| entry.h.as_str());
            let held_hash = held.get(&rel).and_then(|value| value["h"].as_str());
            let list = if mac == vm {
                "agreeWithoutTransfer"
            } else if first_round && self.import_state.is_none() {
                "choices"
            } else if held_hash == Some(mac_hash) {
                "held"
            } else if mac != agreed && vm == agreed {
                if mac.is_none() {
                    "deletionsToMain"
                } else {
                    "uploads"
                }
            } else if mac == agreed && vm != agreed {
                if vm.is_none() {
                    "macDeletions"
                } else {
                    "macWrites"
                }
            } else {
                "conflicts"
            };
            if list == "agreeWithoutTransfer" && mac == agreed {
                continue;
            }
            lists.entry(list).or_default().push(rel);
        }
        let mut plan = json!({
            "projectId": self.project_id,
            "pending": pending_view,
            "preservedConflicts": status["preservedConflicts"],
            "recovery": read_json(&self.path("recover.json"), Value::Null)?,
            "firstRound": first_round,
            "importStateId": self.import_state,
            "mainStateId": head,
            "agreedPaths": manifest.len(),
            "notices": notices,
        });
        for name in [
            "uploads",
            "deletionsToMain",
            "macWrites",
            "macDeletions",
            "conflicts",
            "held",
            "choices",
            "agreeWithoutTransfer",
        ] {
            let items = lists.remove(name).unwrap_or_default();
            plan[name] = json!({
                "count": items.len(),
                "paths": items.iter().take(PLAN_SAMPLE).collect::<Vec<_>>(),
            });
        }
        let mac_deletions = plan["deletionsToMain"]["count"].as_u64().unwrap_or(0) as usize;
        let main_deletions = plan["macDeletions"]["count"].as_u64().unwrap_or(0) as usize;
        plan["guards"] = json!({
            "macDeletionsWait": too_many(mac_deletions, 0, manifest.len()),
            "mainDeletionsWait": too_many(main_deletions, 0, manifest.len()),
        });
        if lock.is_none() && self.path("round.lock").exists() {
            return Err(MacSyncError::Busy);
        }
        Ok(plan)
    }

    fn round_locked(&self, paths: Option<&BTreeSet<String>>) -> Result<Value, MacSyncError> {
        self.check_legacy_recovery()?;
        let previous = read_json(&self.path("status.json"), json!({}))?;
        if let Some(conflict) = previous["preservedConflicts"]
            .as_array()
            .and_then(|items| items.first())
        {
            return Err(MacSyncError::PreservedConflict {
                path: conflict["path"].as_str().unwrap_or("unknown").to_owned(),
                recovery: PathBuf::from(conflict["recovery"].as_str().unwrap_or("unknown")),
            });
        }
        check_retained(&self.root)?;
        let rules = self.rules()?;
        let mut manifest = match self.load_manifest()? {
            Some(manifest) => manifest,
            None => {
                let manifest = self.bootstrap(&rules)?;
                self.save_manifest(&manifest)?;
                manifest
            }
        };
        let waiting = string_set(&previous["choices"]);
        let previous_rescan = string_set(&previous["rescan"]);
        // Mac content in a conflict, or handed to the lead: it is not sent again until the
        // Mac file changes or the conflict is resolved.
        let mut held = self.load_object("held.json")?;
        // Read paths again when a refused merge or a choice left them unsent.
        let paths = paths.map(|paths| {
            paths
                .iter()
                .chain(&previous_rescan)
                .cloned()
                .collect::<BTreeSet<_>>()
        });
        let held_hash = |rel: &str| {
            held.get(rel)
                .and_then(|value| value["h"].as_str())
                .map(str::to_owned)
        };
        let (current, mut notices) = match &paths {
            Some(paths) if self.path("manifest.json").exists() => {
                scan_paths(&self.root, &rules, &manifest, paths)?
            }
            _ => scan(&self.root, &rules, &manifest, None)?,
        };
        let mut changes: Vec<Change> = Vec::new();
        for (rel, entry) in &current {
            if waiting.contains(rel) || held_hash(rel).as_deref() == Some(entry.h.as_str()) {
                continue;
            }
            let known = manifest.get(rel);
            let changed = match known {
                None => true,
                Some(known) => {
                    known.k != entry.k
                        || known.h != entry.h
                        || (entry.k == "file" && (known.x ^ entry.x) & 0o111 != 0)
                }
            };
            if changed {
                changes.push((
                    rel.clone(),
                    Some(entry.clone()),
                    known.and_then(|known| known.b.clone()),
                ));
            }
        }
        let deleted = manifest
            .iter()
            .filter(|(rel, _)| {
                !current.contains_key(*rel)
                    && !rules.skipped(rel)
                    && !waiting.contains(*rel)
                    && held_hash(rel).as_deref() != Some(ABSENT)
            })
            .map(|(rel, known)| (rel.clone(), None, known.b.clone()))
            .collect::<Vec<Change>>();
        let allowed = read_json(&self.path("allow-deletes.json"), json!({}))?["count"]
            .as_u64()
            .unwrap_or(0) as usize;
        if too_many(deleted.len(), allowed, manifest.len()) {
            return self.record_status(json!({
                "state": "paused",
                "notices": notices,
                "deletes": deleted.len(),
                "reason": format!(
                    "{} files were deleted on the Mac; confirm before they sync",
                    deleted.len()
                ),
            }));
        }
        changes.extend(deleted);
        let observed = current
            .keys()
            .map(|rel| {
                let path = contained_path(&self.root, rel)?;
                Ok((rel.clone(), FileStamp::read(&path).map_err(io)?))
            })
            .collect::<Result<BTreeMap<_, _>, MacSyncError>>()?;
        let pending = read_json(&self.path("pending.json"), Value::Null)?;
        let replayed = !pending.is_null();
        let (operation, params, sent) = if replayed {
            let sent = pending["hashes"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(rel, hash)| Some((rel.clone(), hash.as_str()?.to_owned())))
                .collect::<BTreeMap<_, _>>();
            (
                pending["operation"]
                    .as_str()
                    .ok_or(MacSyncError::Invalid)?
                    .to_owned(),
                pending["params"].clone(),
                sent,
            )
        } else {
            let operation = format!("mac-{}", uuid::Uuid::new_v4().simple());
            // A path handed to the lead comes back when main changes it after the handoff.
            let handoffs = held
                .values()
                .filter(|value| value["why"] == "agent")
                .filter_map(|value| value["base"].as_str().map(str::to_owned))
                .collect::<Vec<_>>();
            let (params, sent) = self.upload(&operation, &changes, &manifest, &handoffs)?;
            save_json(
                &self.path("pending.json"),
                &json!({"operation": operation, "params": params, "hashes": sent}),
            )?;
            (operation, params, sent)
        };
        let result =
            match self
                .guest
                .call("sync.mac.apply", &params, Some(&operation), APPLY_TIMEOUT)
            {
                Ok(result) => result,
                Err(error) => {
                    if !error.uncertain() && error.code() != Some("outcome_unknown") {
                        // The guest refused the operation: repeating it cannot help.
                        self.remove("pending.json")?;
                    }
                    return Err(MacSyncError::Guest(error));
                }
            };
        let merge = result["merge"].as_str().unwrap_or_default();
        let merged = matches!(merge, "merged" | "none");
        let result_conflicts = string_set(&result["conflicts"]);
        let conflicts = result_conflicts
            .union(&string_set(&result["mergeConflicts"]))
            .cloned()
            .collect::<BTreeSet<_>>();
        let main_state = result["mainStateId"]
            .as_str()
            .ok_or(MacSyncError::Invalid)?
            .to_owned();
        let outbound_bases = result["outbound"]
            .as_object()
            .map(|outbound| outbound.keys().cloned().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let allowed_removals = read_json(&self.path("allow-deletes.json"), json!({}))?["count"]
            .as_u64()
            .unwrap_or(0) as usize;
        let outbound = self.outbound(
            &rules,
            &mut manifest,
            &result,
            &sent,
            &conflicts,
            &held,
            allowed_removals,
            &observed,
        )?;
        notices.extend(outbound.notices.iter().cloned());
        let bases = params["entries"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                Some((
                    item["path"].as_str()?.to_owned(),
                    item["base"].as_str().map(str::to_owned),
                ))
            })
            .collect::<BTreeMap<_, _>>();
        for rel in string_set(&result["applied"]) {
            if outbound.agreed.contains(&rel)
                || outbound.written.contains(&rel)
                || outbound.skipped.contains(&rel)
                || !merged
            {
                continue;
            }
            if let Some(Some(base)) = bases.get(&rel)
                && !outbound_bases.contains(base)
            {
                // The guest did not compare this base with main: agree in a later round.
                continue;
            }
            // main took the Mac content unchanged for this path.
            match sent.get(&rel).map(String::as_str) {
                Some(ABSENT) => {
                    manifest.remove(&rel);
                }
                Some(hash) => {
                    let item = params["entries"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|item| item["path"].as_str() == Some(rel.as_str()));
                    let mut agreed = current
                        .get(&rel)
                        .filter(|found| found.h == hash)
                        .cloned()
                        .unwrap_or_else(|| Entry {
                            k: item
                                .and_then(|item| item["kind"].as_str())
                                .unwrap_or("file")
                                .to_owned(),
                            s: -1,
                            m: 0,
                            x: manifest.get(&rel).map_or(0, |entry| entry.x),
                            h: hash.to_owned(),
                            b: None,
                        });
                    agreed.b = Some(main_state.clone());
                    manifest.insert(rel.clone(), agreed);
                }
                None => {}
            }
        }
        for rel in &result_conflicts {
            if let Some(hash) = sent.get(rel) {
                held.insert(rel.clone(), json!({"h": hash, "why": "conflict"}));
            }
        }
        if merged {
            // A path whose base the guest compared with main, and which main left alone or
            // the Mac took, now agrees with main.
            for (rel, known) in manifest.iter_mut() {
                if !outbound.skipped.contains(rel)
                    && !conflicts.contains(rel)
                    && known
                        .b
                        .as_ref()
                        .is_some_and(|base| outbound_bases.contains(base))
                {
                    known.b = Some(main_state.clone());
                }
            }
        }
        let scanned = |rel: &str| {
            paths.as_ref().is_none_or(|paths| {
                paths
                    .iter()
                    .any(|item| rel == item || rel.starts_with(&format!("{item}/")))
            })
        };
        // A held path ends when main settles it, or when the Mac file changes again.
        held.retain(|rel, value| {
            let hash = value["h"].as_str().unwrap_or_default();
            !outbound.written.contains(rel)
                && manifest.get(rel).map(|entry| entry.h.as_str()) != Some(hash)
                && (!scanned(rel)
                    || current.get(rel).map_or(ABSENT, |entry| entry.h.as_str()) == hash
                    || result_conflicts.contains(rel))
        });
        self.save_manifest(&manifest)?;
        save_json(&self.path("held.json"), &Value::Object(held.clone()))?;
        self.remove("pending.json")?;
        self.remove("allow-deletes.json")?;
        let in_conflict = held
            .iter()
            .filter(|(_, value)| value["why"] == "conflict")
            .map(|(rel, _)| rel.clone())
            .collect::<Vec<_>>();
        let with_agent = held
            .iter()
            .filter(|(_, value)| value["why"] == "agent")
            .map(|(rel, _)| rel.clone())
            .collect::<Vec<_>>();
        let mut state = if !in_conflict.is_empty() || !waiting.is_empty() {
            "conflict"
        } else if matches!(merge, "refused" | "conflict") || !with_agent.is_empty() {
            "waiting"
        } else {
            "synced"
        };
        let sent_paths = bases.keys().cloned().collect::<BTreeSet<_>>();
        // A replayed round did not read the paths waiting for a rescan; a new round did.
        let leftover = if replayed {
            previous_rescan
                .difference(&sent_paths)
                .cloned()
                .collect::<BTreeSet<_>>()
        } else {
            BTreeSet::new()
        };
        let rescan = if merged {
            leftover
        } else {
            leftover.union(&sent_paths).cloned().collect()
        };
        let mut reason = Value::Null;
        if outbound.removals > 0 {
            state = "paused";
            reason = json!(format!(
                "{} files were deleted in the VM; confirm before they leave the Mac",
                outbound.removals
            ));
        }
        let preserved_conflicts = outbound
            .preserved
            .iter()
            .filter(|item| item["conflict"] == true)
            .cloned()
            .collect::<Vec<_>>();
        if let Some(conflict) = preserved_conflicts.first() {
            state = "error";
            reason = json!(format!(
                "Mac sync waits for review of {}; retained data: {}",
                conflict["path"].as_str().unwrap_or("unknown"),
                conflict["recovery"].as_str().unwrap_or("unknown")
            ));
        } else if let Some(preserved) = outbound.preserved.first() {
            reason = json!(format!(
                "Displaced Mac files remain available for recovery at {}",
                preserved["recovery"].as_str().unwrap_or("unknown")
            ));
        }
        self.record_status(json!({
            "state": state,
            "mainStateId": main_state,
            "sent": bases.len(),
            "received": outbound.written.len(),
            "conflicts": in_conflict,
            "withAgent": with_agent,
            "rescan": rescan,
            "mergeError": result["mergeError"],
            "notices": notices,
            "reason": reason,
            "preserved": outbound.preserved,
            "preservedConflicts": preserved_conflicts,
            "deletes": outbound.removals,
        }))
    }

    /// Send the changed Mac files to the guest; returns the apply parameters and the hash
    /// of what was sent per path.
    fn upload(
        &self,
        operation: &str,
        changes: &[Change],
        manifest: &Manifest,
        extra: &[String],
    ) -> Result<(Value, BTreeMap<String, String>), MacSyncError> {
        // The guest compares at most 16 bases with main: the bases of the sent paths and
        // of the handoffs first, then the most common ones.
        let mut counts: Vec<(String, usize)> = Vec::new();
        for known in manifest.values() {
            if let Some(base) = &known.b {
                match counts.iter_mut().find(|(seen, _)| seen == base) {
                    Some((_, count)) => *count += 1,
                    None => counts.push((base.clone(), 1)),
                }
            }
        }
        counts.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
        let mut since: Vec<String> = Vec::new();
        for base in changes
            .iter()
            .filter_map(|(_, _, base)| base.clone())
            .chain(extra.iter().cloned())
            .chain(counts.into_iter().map(|(base, _)| base))
        {
            if !since.contains(&base) {
                since.push(base);
            }
        }
        since.truncate(MAX_BASES);
        let mut params = json!({
            "projectId": self.project_id, "operationId": operation, "since": since, "entries": [],
        });
        let mut hashes = BTreeMap::new();
        for (rel, entry, _) in changes {
            if entry.is_none() {
                hashes.insert(rel.clone(), ABSENT.to_owned());
            }
        }
        if changes.is_empty() {
            return Ok((params, hashes));
        }
        let scratch = tempfile::Builder::new()
            .prefix("studio-mac-sync-")
            .tempdir()
            .map_err(io)?;
        let archive = scratch.path().join("changes.tar.gz");
        let mut entries = Vec::new();
        {
            let raw = File::create(&archive).map_err(io)?;
            let mut output = TarGzipWriter::new(raw);
            output.append_directory(".", 0o755).map_err(io)?;
            for (rel, entry, base) in changes {
                let mut item = json!({
                    "path": rel,
                    "kind": entry.as_ref().map_or(ABSENT, |entry| entry.k.as_str()),
                    "base": base,
                });
                if let (Some(entry), Some(known)) = (entry, manifest.get(rel))
                    && entry.k == "file"
                    && (known.x ^ entry.x) & 0o111 != 0
                {
                    item["modeChanged"] = json!(true);
                }
                entries.push(item);
                let Some(entry) = entry else {
                    continue;
                };
                let path = contained_path(&self.root, rel)?;
                if entry.k == "link" {
                    let target = link_target(&path).map_err(io)?;
                    let text = std::str::from_utf8(&target).map_err(|_| MacSyncError::Invalid)?;
                    output.append_symlink(rel, 0o777, text).map_err(io)?;
                    hashes.insert(rel.clone(), hex_sha(&target));
                    continue;
                }
                let mut data = Vec::new();
                open_no_follow(&path)
                    .and_then(|file| file.take(self.file_limit + 1).read_to_end(&mut data))
                    .map_err(io)?;
                if data.len() as u64 > self.file_limit {
                    return Err(MacSyncError::Invalid);
                }
                let mode = if entry.x == 0 { 0o644 } else { entry.x };
                output
                    .append_file(rel, mode, data.len() as u64, &mut Cursor::new(&data))
                    .map_err(io)?;
                // The hash of what was sent: a later Mac edit is never mistaken for it.
                hashes.insert(rel.clone(), hex_sha(&data));
            }
            output
                .finish()
                .and_then(|mut raw| raw.flush())
                .map_err(io)?;
        }
        upload_archive(self.guest, &archive, operation)?;
        params["entries"] = Value::Array(entries);
        params["upload"] = json!(format!("{UPLOAD_ROOT}{}", &operation[4..28]));
        Ok((params, hashes))
    }

    /// Bring main's changes to the Mac where the Mac file still has its agreed content, or
    /// the content main took from this round. Deletions come after the other changes, so
    /// a rename in case or Unicode form moves the file instead of losing it; a file
    /// written or agreed in this round is never deleted under another spelling.
    #[allow(clippy::too_many_arguments)]
    fn outbound(
        &self,
        rules: &Rules,
        manifest: &mut Manifest,
        result: &Value,
        sent: &BTreeMap<String, String>,
        conflicts: &BTreeSet<String>,
        held: &Map<String, Value>,
        allowed_removals: usize,
        observed: &BTreeMap<String, Option<FileStamp>>,
    ) -> Result<Outbound, MacSyncError> {
        let mut done = Outbound {
            agreed: BTreeSet::new(),
            written: BTreeSet::new(),
            skipped: BTreeSet::new(),
            removals: 0,
            notices: Vec::new(),
            preserved: Vec::new(),
        };
        let state = result["mainStateId"]
            .as_str()
            .ok_or(MacSyncError::Invalid)?
            .to_owned();
        let merged = matches!(result["merge"].as_str(), Some("merged" | "none"));
        let taken = if merged {
            string_set(&result["applied"])
        } else {
            BTreeSet::new()
        };
        let outbound = result["outbound"].as_object().cloned().unwrap_or_default();
        // Main deletions that would remove agreed Mac files: many at once wait for the user.
        let agreed_files: &Manifest = manifest;
        let removals = outbound
            .iter()
            .flat_map(|(base, changes)| {
                changes
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(move |change| {
                        change["kind"] == ABSENT
                            && change["path"].as_str().is_some_and(|rel| {
                                agreed_files.get(rel).and_then(|known| known.b.as_deref())
                                    == Some(base.as_str())
                            })
                    })
                    .filter_map(|change| change["path"].as_str().map(str::to_owned))
            })
            .collect::<BTreeSet<_>>();
        let hold = too_many(removals.len(), allowed_removals, manifest.len());
        let deleted_in_main = outbound
            .values()
            .flat_map(|changes| changes.as_array().into_iter().flatten())
            .filter(|change| change["kind"] == ABSENT)
            .filter_map(|change| change["path"].as_str().map(str::to_owned))
            .collect::<BTreeSet<_>>();
        let ordered = outbound
            .iter()
            .flat_map(|(base, changes)| {
                changes
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(move |change| (base, change))
            })
            .filter(|(_, change)| change["kind"] != ABSENT)
            .chain(
                outbound
                    .iter()
                    .flat_map(|(base, changes)| {
                        changes
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(move |change| (base, change))
                    })
                    .filter(|(_, change)| change["kind"] == ABSENT),
            )
            .collect::<Vec<_>>();
        let mut listings = Listings::default();
        // File IDs of the Mac files that this round wrote or agreed on.
        let mut touched = BTreeSet::new();
        for (base, change) in ordered {
            let Some(rel) = change["path"].as_str() else {
                continue;
            };
            if done.agreed.contains(rel) || done.written.contains(rel) {
                continue;
            }
            let mut known = manifest.get(rel).cloned();
            let handed = held.get(rel);
            // A handoff ends with main's next change of the path, unless the Mac sent a
            // newer version in this round.
            let from_agent = handed.is_some_and(|handed| {
                handed["why"] == "agent" && handed["base"].as_str() == Some(base.as_str())
            }) && !sent.contains_key(rel);
            if rules.skipped(rel)
                || (conflicts.contains(rel) && !from_agent)
                || (known
                    .as_ref()
                    .is_some_and(|known| known.b.as_deref() != Some(base.as_str()))
                    && !from_agent)
            {
                continue;
            }
            if hold && removals.contains(rel) {
                done.skipped.insert(rel.to_owned());
                continue;
            }
            let kind = change["kind"].as_str().unwrap_or(ABSENT);
            let path = contained_path(&self.root, rel)?;
            if let Some(before) = observed.get(rel)
                && *before != FileStamp::read(&path).map_err(io)?
            {
                let target = if kind == ABSENT {
                    ABSENT
                } else {
                    change["sha256"].as_str().unwrap_or_default()
                };
                if taken.contains(rel) && sent.get(rel).map(String::as_str) == Some(target) {
                    if target == ABSENT {
                        manifest.remove(rel);
                    } else {
                        manifest.insert(
                            rel.to_owned(),
                            Entry {
                                k: kind.to_owned(),
                                s: -1,
                                m: 0,
                                x: change["mode"].as_u64().unwrap_or(0) as u32,
                                h: target.to_owned(),
                                b: Some(state.clone()),
                            },
                        );
                    }
                }
                done.skipped.insert(rel.to_owned());
                continue;
            }
            let actual = match listings.actual(&self.root, rel) {
                Ok(actual) => actual,
                Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => None,
                Err(error) => return Err(io(error)),
            };
            if let Some(other) = actual.as_deref().filter(|actual| *actual != rel) {
                // The Mac volume finds this name as another spelling on disk.
                if kind == ABSENT {
                    // The name under this spelling is gone; the other file stays.
                    manifest.remove(rel);
                    done.agreed.insert(rel.to_owned());
                    continue;
                }
                let other_known = manifest.get(other).cloned();
                let unchanged = match &other_known {
                    Some(known) => {
                        current_hash(&mut listings, &self.root, other).map_err(io)? == known.h
                    }
                    None => false,
                };
                if !(deleted_in_main.contains(other) && unchanged) {
                    done.notices.push(format!(
                        "{rel}: the Mac has this name as {other}; the file waits"
                    ));
                    done.skipped.insert(rel.to_owned());
                    continue;
                }
                // Main renamed the file in case or Unicode form: move the Mac file.
                let other_path = contained_path(&self.root, other)?;
                // APFS renames case and Unicode spellings atomically. An intermediate
                // file could be the sole copy after a crash and must not be discarded.
                fs::rename(&other_path, &path).map_err(io)?;
                listings.forget(&other_path);
                listings.forget(&path);
                if let Some(entry) = manifest.remove(other) {
                    manifest.insert(rel.to_owned(), entry);
                }
                done.agreed.insert(other.to_owned());
                known = other_known;
            }
            let meta = match fs::symlink_metadata(&path) {
                Ok(meta) => Some(meta),
                Err(error) if not_found(&error) => None,
                Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => {
                    // A Mac file stands where main has a folder: that file is the conflict.
                    done.skipped.insert(rel.to_owned());
                    continue;
                }
                Err(error) => return Err(io(error)),
            };
            if meta.as_ref().is_some_and(fs::Metadata::is_dir) {
                // A Mac folder stands where main has a file; nothing replaces it.
                done.skipped.insert(rel.to_owned());
                continue;
            }
            if kind == ABSENT
                && let Some(meta) = &meta
                && touched.contains(&(meta.dev(), meta.ino()))
            {
                // Another spelling of a file that this round wrote or agreed on.
                manifest.remove(rel);
                done.agreed.insert(rel.to_owned());
                continue;
            }
            let now = match &meta {
                None => None,
                Some(meta) => Some(describe(&path, meta, true).map_err(io)?),
            };
            let now_hash = now
                .as_ref()
                .map_or(ABSENT, |entry| entry.h.as_str())
                .to_owned();
            let target = if kind == ABSENT {
                ABSENT
            } else {
                change["sha256"].as_str().unwrap_or_default()
            };
            let change_mode = change["mode"].as_u64().map(|mode| mode as u32);
            if now_hash == target {
                match now {
                    None => {
                        manifest.remove(rel);
                    }
                    Some(mut now) => {
                        if let Some(mode) = change_mode
                            && now.k == "file"
                            && (now.x ^ mode) & 0o111 != 0
                            && known
                                .as_ref()
                                .is_none_or(|known| (now.x ^ known.x) & 0o111 == 0)
                        {
                            // Main changed the mode and the Mac did not: the Mac takes it.
                            // The open never follows a link that replaced the file.
                            open_no_follow(&path)
                                .and_then(|file| {
                                    file.set_permissions(fs::Permissions::from_mode(
                                        (now.x & !0o111) | (mode & 0o111),
                                    ))
                                })
                                .map_err(io)?;
                            let meta = fs::symlink_metadata(&path).map_err(io)?;
                            now = describe(&path, &meta, true).map_err(io)?;
                        }
                        manifest.insert(rel.to_owned(), now.with_base(&state));
                        if let Ok(meta) = fs::symlink_metadata(&path) {
                            touched.insert((meta.dev(), meta.ino()));
                        }
                    }
                }
                // The Mac already has main's content: agree without touching the file.
                done.agreed.insert(rel.to_owned());
                continue;
            }
            let mut allowed = BTreeSet::new();
            if from_agent {
                allowed.insert(
                    handed
                        .and_then(|handed| handed["h"].as_str())
                        .unwrap_or_default()
                        .to_owned(),
                );
            } else {
                allowed.insert(
                    known
                        .as_ref()
                        .map_or(ABSENT.to_owned(), |known| known.h.clone()),
                );
                if taken.contains(rel)
                    && let Some(hash) = sent.get(rel)
                {
                    // main took the content this round sent.
                    allowed.insert(hash.clone());
                }
            }
            if !allowed.contains(&now_hash) || sent.get(rel).is_some_and(|hash| *hash != now_hash) {
                done.skipped.insert(rel.to_owned());
                if taken.contains(rel) && sent.get(rel).map(String::as_str) == Some(target) {
                    // The Mac changed again after the send, and main holds what was sent:
                    // that content is the agreed base of the next round.
                    manifest.insert(
                        rel.to_owned(),
                        Entry {
                            k: kind.to_owned(),
                            s: -1,
                            m: 0,
                            x: change_mode.unwrap_or(0),
                            h: target.to_owned(),
                            b: Some(state.clone()),
                        },
                    );
                }
                continue;
            }
            let stamp = FileStamp::read(&path).map_err(io)?;
            let mut data = Vec::new();
            if matches!(kind, "file" | "link") {
                let (_, fetched, _) = fetch_entry(self.guest, &self.project_id, &state, rel)?;
                if hex_sha(&fetched) != target {
                    done.skipped.insert(rel.to_owned());
                    continue;
                }
                data = fetched;
            }
            match write_guarded(
                &self.root,
                rel,
                kind,
                &data,
                change_mode.unwrap_or(0o644),
                Some((&now_hash, &stamp)),
                &mut listings,
            )? {
                Written::Done(entry, preserved) => {
                    if let Some(preserved) = preserved {
                        done.notices.push(format!(
                            "{rel}: the displaced Mac inode remains at {}",
                            preserved.directory.join("original").display()
                        ));
                        done.preserved.push(json!({"path":rel,"recovery":preserved.directory,"conflict":preserved.changed}));
                        if preserved.changed {
                            done.skipped.insert(rel.to_owned());
                            continue;
                        }
                    }
                    match entry {
                        None => {
                            manifest.remove(rel);
                        }
                        Some(entry) => {
                            manifest.insert(rel.to_owned(), entry.with_base(&state));
                            if let Ok(meta) = fs::symlink_metadata(&path) {
                                touched.insert((meta.dev(), meta.ino()));
                            }
                        }
                    }
                    done.written.insert(rel.to_owned());
                }
                Written::Changed => {
                    // The user saved the file meanwhile; the next round sends it.
                    done.skipped.insert(rel.to_owned());
                }
            }
        }
        if hold {
            done.removals = removals.len();
        }
        Ok(done)
    }
}

/// Whether `count` deletions wait for the user's confirmation.
fn too_many(count: usize, allowed: usize, agreed: usize) -> bool {
    count > DELETE_FLOOR
        && count > allowed
        && (count > DELETE_COUNT || count as f64 > agreed as f64 * DELETE_SHARE)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};

    fn temporary() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = CanonicalPath::existing(temp.path()).unwrap().to_path_buf();
        (temp, root)
    }

    fn has_bytes(root: &Path, bytes: &[u8]) -> bool {
        fs::read_dir(root).unwrap().any(|item| {
            let path = item.unwrap().path();
            if path.is_dir() {
                has_bytes(&path, bytes)
            } else {
                fs::read(path).is_ok_and(|content| content == bytes)
            }
        })
    }

    #[test]
    fn replacement_retains_a_save_after_the_final_guard() {
        let (_temp, root) = temporary();
        let path = &root.join("file");
        fs::write(&path, b"base").unwrap();
        let stamp = FileStamp::read(&path).unwrap();
        write_guarded_with_hook(
            &root,
            "file",
            "file",
            b"vm",
            0o644,
            Some((&hex_sha(b"base"), &stamp)),
            &mut Listings::default(),
            &mut || {
                fs::write(&path, b"user save after the guard").unwrap();
            },
        )
        .unwrap();
        assert!(has_bytes(&root, b"user save after the guard"));
    }

    #[test]
    fn deletion_retains_a_file_recreated_after_the_final_guard() {
        let (_temp, root) = temporary();
        let path = &root.join("file");
        fs::write(&path, b"base").unwrap();
        let stamp = FileStamp::read(&path).unwrap();
        write_guarded_with_hook(
            &root,
            "file",
            ABSENT,
            b"",
            0,
            Some((&hex_sha(b"base"), &stamp)),
            &mut Listings::default(),
            &mut || {
                fs::remove_file(&path).unwrap();
                fs::write(&path, b"recreated user file").unwrap();
            },
        )
        .unwrap();
        assert!(has_bytes(&root, b"recreated user file"));
    }

    #[test]
    fn creation_does_not_replace_a_file_created_after_the_final_guard() {
        let (_temp, root) = temporary();
        let path = &root.join("file");
        write_guarded_with_hook(
            &root,
            "file",
            "file",
            b"vm",
            0o644,
            Some((ABSENT, &None)),
            &mut Listings::default(),
            &mut || {
                fs::write(&path, b"new user file").unwrap();
            },
        )
        .unwrap();
        assert_eq!(fs::read(path).unwrap(), b"new user file");
    }

    #[test]
    fn displaced_inode_retains_writes_through_an_open_descriptor_after_return() {
        for kind in ["file", ABSENT] {
            let (_temp, root) = temporary();
            let path = &root.join("file");
            fs::write(&path, b"base").unwrap();
            let mut editor = fs::OpenOptions::new().write(true).open(&path).unwrap();
            let stamp = FileStamp::read(&path).unwrap();
            write_guarded(
                &root,
                "file",
                kind,
                b"vm",
                0o644,
                Some((&hex_sha(b"base"), &stamp)),
                &mut Listings::default(),
            )
            .unwrap();
            editor.set_len(0).unwrap();
            editor.seek(SeekFrom::Start(0)).unwrap();
            editor.write_all(b"late editor save").unwrap();
            editor.sync_all().unwrap();
            drop(editor);
            assert!(matches!(
                scan(&root, &Rules::new(&root).unwrap(), &Manifest::new(), None),
                Err(MacSyncError::PreservedConflict { .. })
            ));
            assert!(has_bytes(&root, b"late editor save"), "{kind}");
        }
    }

    #[test]
    fn a_scan_never_deletes_an_uncertain_legacy_temporary() {
        let (_temp, root) = temporary();
        let path = &root.join(".file.studio-sync-0123abcd");
        fs::write(&path, b"possibly the only original").unwrap();
        assert!(matches!(
            scan(&root, &Rules::new(&root).unwrap(), &Manifest::new(), None),
            Err(MacSyncError::PreservedConflict { .. })
        ));
        assert_eq!(fs::read(path).unwrap(), b"possibly the only original");
    }

    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        bounded_child::run_without_git_environment(bounded_child::Request {
            program: std::ffi::OsStr::new("git"),
            git_directory: Some(root.as_os_str()),
            args,
            environment: &[],
            input: None,
            stdout_limit: 65_536,
            stderr_limit: 4_800,
            timeout: GIT_TIMEOUT,
        })
        .unwrap()
        .stdout
        .bytes
    }

    #[test]
    fn retained_secrets_stay_out_of_git_even_when_git_starts_later() {
        for git_first in [false, true] {
            let (_temp, root) = temporary();
            if git_first {
                git(&root, &["init", "-q"]);
            }
            fs::write(root.join("file"), b"retained private content").unwrap();
            write_mac(&root, "file", "file", b"vm", 0o644).unwrap();
            if !git_first {
                git(&root, &["init", "-q"]);
            }
            let untracked = git(&root, &["ls-files", "--others", "--exclude-standard", "-z"]);
            assert_eq!(untracked, b"file\0", "git_first={git_first}");
        }
    }

    #[test]
    fn an_explicit_review_does_not_remove_the_retained_inode() {
        let (_temp, root) = temporary();
        let path = root.join("file");
        fs::write(&path, b"base").unwrap();
        let stamp = FileStamp::read(&path).unwrap();
        let Written::Done(_, Some(outcome)) = write_guarded_with_hook(
            &root,
            "file",
            "file",
            b"vm",
            0o644,
            Some((&hex_sha(b"base"), &stamp)),
            &mut Listings::default(),
            &mut || fs::write(&path, b"user save").unwrap(),
        )
        .unwrap() else {
            panic!("the displaced inode must remain available");
        };
        assert!(inspect_preserved(&outcome.directory, "file").is_err());
        let receipt = outcome.directory.join("record.json");
        let mut record = read_json(&receipt, Value::Null).unwrap();
        record["phase"] = json!("reviewed");
        save_json(&receipt, &record).unwrap();
        inspect_preserved(&outcome.directory, "file").unwrap();
        assert_eq!(
            fs::read(outcome.directory.join("original")).unwrap(),
            b"user save"
        );
    }
}
