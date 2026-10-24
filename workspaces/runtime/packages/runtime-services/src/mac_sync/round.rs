//! One Mac folder sync round (docs/vm-mac-sync.md, "A sync round"), the Mac side of the
//! last Python release's `codex_vm_mac_sync.py`.
//!
//! The manifest records, for each synced path, the content the Mac and the VM agreed on
//! and the layr state that content came from. A round sends the Mac's changes with those
//! bases, lets the guest fold them into the `mac` line and merge it into main, and then
//! writes main's changes into the Mac folder where the Mac file still has its agreed
//! content. Nothing is written over a Mac edit, and no conflict markers reach the Mac.
//!
//! Registered difference from Python: when the guest answers `outcome_unknown` for a
//! round, Python kept the operation and repeated it forever, and every repeat got the
//! same answer. Here the round records the error, drops the operation, and the next round
//! hashes the paths of that operation again on both sides before it starts a new one
//! ([`ProjectSync::round`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Cursor, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
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
    pub fn new(root: &Path) -> Self {
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
                Ok(output) => {
                    let mut items = output
                        .stdout
                        .bytes
                        .split(|byte| *byte == 0)
                        .collect::<Vec<_>>();
                    if output.stdout.truncated {
                        // The last item may be cut; the complete ones still apply.
                        items.pop();
                    }
                    ignored = items
                        .into_iter()
                        .filter(|item| item.ends_with(b"/"))
                        .map(|item| String::from_utf8_lossy(&item[..item.len() - 1]).into_owned())
                        .collect();
                }
                Err(error) => eprintln!(
                    "mac sync: Git ignore rules of {}: {error:?}",
                    root.display()
                ),
            }
        }
        Self {
            ignored,
            limit: MAX_SYNC_FILE_BYTES,
        }
    }

    /// Files larger than `bytes` do not sync.
    pub fn with_file_limit(mut self, bytes: u64) -> Self {
        self.limit = bytes;
        self
    }

    pub fn skipped(&self, rel: &str) -> bool {
        let parts = rel.split('/').collect::<Vec<_>>();
        if parts.iter().any(|part| ALWAYS_SKIPPED.contains(part)) {
            return true;
        }
        (1..=parts.len()).any(|count| self.ignored.contains(&parts[..count].join("/")))
    }
}

fn mtime_ns(meta: &fs::Metadata) -> i64 {
    meta.mtime()
        .saturating_mul(1_000_000_000)
        .saturating_add(meta.mtime_nsec())
}

fn file_hash(path: &Path) -> std::io::Result<String> {
    let mut file = open_no_follow(path)?;
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

fn open_no_follow(path: &Path) -> std::io::Result<File> {
    use rustix::fs::{Mode, OFlags};
    let descriptor = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(File::from(descriptor))
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

/// One path of a scan: its new entry, or a notice, or nothing when it vanished.
fn scan_one(
    path: &Path,
    rel: &str,
    rules: &Rules,
    manifest: &Manifest,
    entries: &mut Manifest,
    notices: &mut Vec<String>,
) {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => return,
    };
    let kind = meta.file_type();
    if !(kind.is_file() || kind.is_symlink()) {
        notices.push(format!("{rel}: a special file does not sync"));
        return;
    }
    let known = manifest.get(rel);
    if kind.is_file() && meta.len() > rules.limit {
        notices.push(format!("{rel}: files over 256 MiB do not sync"));
        if let Some(known) = known {
            // Too large to send now, but not deleted.
            entries.insert(rel.to_owned(), known.without_base());
        }
        return;
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
            entries.insert(rel.to_owned(), entry);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            // An unreadable file is no deletion: its agreed entry stays.
            notices.push(format!("{rel}: the file could not be read: {error}"));
            if let Some(known) = known {
                entries.insert(rel.to_owned(), known.without_base());
            }
        }
    }
}

/// The Mac's synced paths (below `start` when given). A file whose size and time match
/// the manifest keeps its recorded hash; others are hashed. Returns (entries, notices).
pub fn scan(
    root: &Path,
    rules: &Rules,
    manifest: &Manifest,
    start: Option<&Path>,
) -> (Manifest, Vec<String>) {
    let mut entries = Manifest::new();
    let mut notices = Vec::new();
    let mut folders = vec![start.unwrap_or(root).to_path_buf()];
    while let Some(folder) = folders.pop() {
        let prefix = match folder.strip_prefix(root) {
            Ok(relative) if relative.as_os_str().is_empty() => String::new(),
            Ok(relative) => format!("{}/", relative.to_string_lossy()),
            Err(_) => continue,
        };
        let Ok(listing) = fs::read_dir(&folder) else {
            continue;
        };
        let mut names = listing
            .filter_map(Result::ok)
            .map(|item| item.file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        names.sort();
        let mut below = Vec::new();
        for name in names {
            let rel = format!("{prefix}{name}");
            if rules.skipped(&rel) {
                continue;
            }
            let path = folder.join(&name);
            if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_dir()) {
                below.push(path);
                continue;
            }
            scan_one(&path, &rel, rules, manifest, &mut entries, &mut notices);
        }
        folders.extend(below.into_iter().rev());
    }
    (entries, notices)
}

/// The Mac's state with only `paths` (and the folders among them) read again: every
/// other path keeps its manifest entry. File events name what changed.
pub fn scan_paths(
    root: &Path,
    rules: &Rules,
    manifest: &Manifest,
    paths: &BTreeSet<String>,
) -> (Manifest, Vec<String>) {
    let mut covered: Vec<&String> = Vec::new();
    for rel in paths {
        if rel.is_empty() || rel.starts_with("../") {
            continue;
        }
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
    let mut entries = manifest
        .iter()
        .filter(|(rel, _)| !under(rel))
        .map(|(rel, known)| (rel.clone(), known.without_base()))
        .collect::<Manifest>();
    let mut notices = Vec::new();
    for rel in covered {
        if rules.skipped(rel) {
            continue;
        }
        let path = root.join(rel);
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        let mut parent = path.parent();
        let mut linked = false;
        while let Some(folder) = parent.filter(|folder| *folder != root) {
            linked |= fs::symlink_metadata(folder).is_ok_and(|meta| meta.file_type().is_symlink());
            parent = folder.parent();
        }
        if linked {
            continue;
        }
        if meta.is_dir() {
            let (found, more) = scan(root, rules, manifest, Some(&path));
            entries.extend(found);
            notices.extend(more);
        } else {
            scan_one(&path, rel, rules, manifest, &mut entries, &mut notices);
        }
    }
    (entries, notices)
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
    let path = contained_path(root, rel)?;
    if kind == ABSENT {
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() || meta.is_file() => {
                fs::remove_file(&path).map_err(MacSyncError::Io)?;
            }
            _ => {}
        }
        return Ok(None);
    }
    let parent = path.parent().ok_or(MacSyncError::Invalid)?;
    fs::create_dir_all(parent).map_err(MacSyncError::Io)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(MacSyncError::Invalid)?;
    let temporary = parent.join(format!(
        ".{name}.studio-sync-{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    ));
    if kind == "link" {
        let target = std::str::from_utf8(data).map_err(|_| MacSyncError::Invalid)?;
        std::os::unix::fs::symlink(target, &temporary).map_err(MacSyncError::Io)?;
    } else {
        use rustix::fs::{Mode, OFlags};
        let written = (|| -> std::io::Result<()> {
            let descriptor = rustix::fs::open(
                &temporary,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?;
            let mut file = File::from(descriptor);
            file.write_all(data)?;
            file.sync_all()?;
            let mode = if mode == 0 { 0o644 } else { mode & 0o777 };
            file.set_permissions(fs::Permissions::from_mode(mode))
        })();
        if let Err(error) = written {
            drop(fs::remove_file(&temporary));
            return Err(MacSyncError::Io(error));
        }
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        drop(fs::remove_file(&temporary));
        return Err(MacSyncError::Io(error));
    }
    let meta = fs::symlink_metadata(&path).map_err(MacSyncError::Io)?;
    describe(&path, &meta, true)
        .map(Some)
        .map_err(MacSyncError::Io)
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

/// What the outbound step did: agreed without a write, written, skipped, and the main
/// deletions that wait for a confirmation.
struct Outbound {
    agreed: BTreeSet<String>,
    written: BTreeSet<String>,
    skipped: BTreeSet<String>,
    removals: usize,
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
            .map_err(MacSyncError::Io)?
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

    fn rules(&self) -> Rules {
        Rules::new(&self.root).with_file_limit(self.file_limit)
    }

    /// Merge `values` into `status.json` with the current time; returns the new status.
    pub fn record_status(&self, values: Value) -> Result<Value, MacSyncError> {
        fs::create_dir_all(&self.directory).map_err(MacSyncError::Io)?;
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
    /// A lost reply keeps the operation, and the next round repeats it with the same ID;
    /// the guest answers from its receipt. A guest refusal ends the operation. A guest
    /// answer `outcome_unknown` also ends it ([`MacSyncError::Unproven`], kept in
    /// `status.json`): the guest can never prove its result. The next round then hashes
    /// the paths of that operation again on the Mac and in main before it scans: a path
    /// equal on both sides agrees, a path that changed on both sides becomes a conflict,
    /// and the hash guards of the round decide the rest.
    pub fn round(&self, paths: Option<&BTreeSet<String>>) -> Result<Value, MacSyncError> {
        let _lock = lock_round(&self.directory)?;
        self.round_locked(paths)
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
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(MacSyncError::Io(error)),
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
    fn state_hashes(
        &self,
        state: &str,
        keep: impl Fn(&str) -> bool,
    ) -> Result<BTreeMap<String, Value>, MacSyncError> {
        let mut known = BTreeMap::new();
        let mut after = String::new();
        loop {
            let page = self
                .guest
                .call(
                    "sync.mac.hashes",
                    &json!({"projectId": self.project_id, "stateId": state, "after": after}),
                    None,
                    HASHES_TIMEOUT,
                )
                .map_err(MacSyncError::Guest)?;
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
            .state_hashes(&base, |rel| !rules.skipped(rel))?
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
        let (current, _) = scan(&self.root, rules, &Manifest::new(), None);
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

    /// After a dropped operation: hash its paths again on the Mac and in main, so that
    /// the round trusts no agreed content of those paths without a check.
    fn recover(
        &self,
        rules: &Rules,
        manifest: &mut Manifest,
        held: &mut Map<String, Value>,
    ) -> Result<BTreeSet<String>, MacSyncError> {
        let record = read_json(&self.path("recover.json"), Value::Null)?;
        if record.is_null() {
            return Ok(BTreeSet::new());
        }
        let listed = string_set(&record["paths"])
            .into_iter()
            .filter(|rel| !rules.skipped(rel))
            .collect::<BTreeSet<_>>();
        if !listed.is_empty() {
            let head = self.head()?;
            let main = self.state_hashes(&head, |rel| listed.contains(rel))?;
            for rel in &listed {
                let Ok(path) = contained_path(&self.root, rel) else {
                    continue;
                };
                let mac = match fs::symlink_metadata(&path) {
                    Ok(meta) if meta.is_file() || meta.file_type().is_symlink() => {
                        if meta.is_file() && meta.len() > self.file_limit {
                            continue;
                        }
                        match describe(&path, &meta, true) {
                            Ok(entry) => Some(entry),
                            Err(_) => continue,
                        }
                    }
                    Ok(_) => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(_) => continue,
                };
                let mac_hash = mac.as_ref().map_or(ABSENT, |entry| entry.h.as_str());
                let main_hash = main
                    .get(rel)
                    .filter(|item| matches!(item["kind"].as_str(), Some("file" | "link")))
                    .and_then(|item| item["sha256"].as_str())
                    .unwrap_or(ABSENT);
                let agreed = manifest.get(rel).map_or(ABSENT, |entry| entry.h.as_str());
                if mac_hash == main_hash {
                    match mac {
                        None => {
                            manifest.remove(rel);
                        }
                        Some(entry) => {
                            manifest.insert(rel.clone(), entry.with_base(&head));
                        }
                    }
                    held.remove(rel);
                } else if main_hash == agreed || mac_hash == agreed {
                    // One side kept the agreed content: the round's hash guards decide.
                } else if held.get(rel).and_then(|value| value["h"].as_str()) != Some(mac_hash) {
                    // Both sides changed: the user chooses, nothing is overwritten.
                    held.insert(rel.clone(), json!({"h": mac_hash, "why": "conflict"}));
                }
            }
            self.save_manifest(manifest)?;
            save_json(&self.path("held.json"), &Value::Object(held.clone()))?;
        }
        self.remove("recover.json")?;
        Ok(listed)
    }

    fn round_locked(&self, paths: Option<&BTreeSet<String>>) -> Result<Value, MacSyncError> {
        let rules = self.rules();
        let mut manifest = match self.load_manifest()? {
            Some(manifest) => manifest,
            None => {
                let manifest = self.bootstrap(&rules)?;
                self.save_manifest(&manifest)?;
                manifest
            }
        };
        let previous = read_json(&self.path("status.json"), json!({}))?;
        let waiting = string_set(&previous["choices"]);
        let previous_rescan = string_set(&previous["rescan"]);
        // Mac content in a conflict, or handed to the lead: it is not sent again until the
        // Mac file changes or the conflict is resolved.
        let mut held = self.load_object("held.json")?;
        let recovered = self.recover(&rules, &mut manifest, &mut held)?;
        // Paths a refused merge or a choice left unsent, and the paths of a dropped
        // operation, are read again.
        let paths = paths.map(|paths| {
            paths
                .iter()
                .chain(&previous_rescan)
                .chain(&recovered)
                .cloned()
                .collect::<BTreeSet<_>>()
        });
        let held_hash = |rel: &str| {
            held.get(rel)
                .and_then(|value| value["h"].as_str())
                .map(str::to_owned)
        };
        let (current, notices) = match &paths {
            Some(paths) if self.path("manifest.json").exists() => {
                scan_paths(&self.root, &rules, &manifest, paths)
            }
            _ => scan(&self.root, &rules, &manifest, None),
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
                Err(error) if error.code() == Some("outcome_unknown") => {
                    let listed = params["entries"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|item| item["path"].as_str())
                        .collect::<BTreeSet<_>>();
                    save_json(
                        &self.path("recover.json"),
                        &json!({"operation": operation, "paths": listed, "at": now_seconds()}),
                    )?;
                    self.remove("pending.json")?;
                    let error = MacSyncError::Unproven {
                        operation,
                        paths: listed.len(),
                    };
                    self.record_status(json!({
                        "state": "error",
                        "reason": super::error_reason(&error),
                    }))?;
                    return Err(error);
                }
                Err(error) => {
                    if !error.uncertain() {
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
        )?;
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
            match current.get(&rel) {
                None => {
                    manifest.remove(&rel);
                }
                Some(found) => {
                    manifest.insert(rel.clone(), found.with_base(&main_state));
                }
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
            .map_err(MacSyncError::Io)?;
        let archive = scratch.path().join("changes.tar.gz");
        let mut entries = Vec::new();
        {
            let raw = File::create(&archive).map_err(MacSyncError::Io)?;
            let mut output = TarGzipWriter::new(raw);
            output
                .append_directory(".", 0o755)
                .map_err(MacSyncError::Io)?;
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
                    let target = link_target(&path).map_err(MacSyncError::Io)?;
                    let text = std::str::from_utf8(&target).map_err(|_| MacSyncError::Invalid)?;
                    output
                        .append_symlink(rel, 0o777, text)
                        .map_err(MacSyncError::Io)?;
                    hashes.insert(rel.clone(), hex_sha(&target));
                    continue;
                }
                let mut data = Vec::new();
                open_no_follow(&path)
                    .and_then(|file| file.take(self.file_limit + 1).read_to_end(&mut data))
                    .map_err(MacSyncError::Io)?;
                if data.len() as u64 > self.file_limit {
                    return Err(MacSyncError::Invalid);
                }
                let mode = if entry.x == 0 { 0o644 } else { entry.x };
                output
                    .append_file(rel, mode, data.len() as u64, &mut Cursor::new(&data))
                    .map_err(MacSyncError::Io)?;
                // The hash of what was sent: a later Mac edit is never mistaken for it.
                hashes.insert(rel.clone(), hex_sha(&data));
            }
            output
                .finish()
                .and_then(|mut raw| raw.flush())
                .map_err(MacSyncError::Io)?;
        }
        upload_archive(self.guest, &archive, operation)?;
        params["entries"] = Value::Array(entries);
        params["upload"] = json!(format!("{UPLOAD_ROOT}{}", &operation[4..28]));
        Ok((params, hashes))
    }

    /// Bring main's changes to the Mac where the Mac file still has its agreed content, or
    /// the content main took from this round.
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
    ) -> Result<Outbound, MacSyncError> {
        let mut agreed = BTreeSet::new();
        let mut written = BTreeSet::new();
        let mut skipped = BTreeSet::new();
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
        for (base, changes) in &outbound {
            for change in changes.as_array().into_iter().flatten() {
                let Some(rel) = change["path"].as_str() else {
                    continue;
                };
                if agreed.contains(rel) || written.contains(rel) {
                    continue;
                }
                let known = manifest.get(rel).cloned();
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
                    skipped.insert(rel.to_owned());
                    continue;
                }
                let path = contained_path(&self.root, rel)?;
                let meta = match fs::symlink_metadata(&path) {
                    Ok(meta) => Some(meta),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) if error.raw_os_error() == Some(libc::ENOTDIR) => {
                        // A Mac file stands where main has a folder: that file is the conflict.
                        skipped.insert(rel.to_owned());
                        continue;
                    }
                    Err(error) => return Err(MacSyncError::Io(error)),
                };
                if meta.as_ref().is_some_and(fs::Metadata::is_dir) {
                    // A Mac folder stands where main has a file; nothing replaces it.
                    skipped.insert(rel.to_owned());
                    continue;
                }
                let now = match &meta {
                    None => None,
                    Some(meta) => Some(describe(&path, meta, true).map_err(MacSyncError::Io)?),
                };
                let now_hash = now
                    .as_ref()
                    .map_or(ABSENT, |entry| entry.h.as_str())
                    .to_owned();
                let kind = change["kind"].as_str().unwrap_or(ABSENT);
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
                                fs::set_permissions(
                                    &path,
                                    fs::Permissions::from_mode((now.x & !0o111) | (mode & 0o111)),
                                )
                                .map_err(MacSyncError::Io)?;
                                let meta = fs::symlink_metadata(&path).map_err(MacSyncError::Io)?;
                                now = describe(&path, &meta, true).map_err(MacSyncError::Io)?;
                            }
                            manifest.insert(rel.to_owned(), now.with_base(&state));
                        }
                    }
                    // The Mac already has main's content: agree without touching the file.
                    agreed.insert(rel.to_owned());
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
                if !allowed.contains(&now_hash) {
                    skipped.insert(rel.to_owned());
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
                let mut data = Vec::new();
                if matches!(kind, "file" | "link") {
                    let (_, fetched, _) = fetch_entry(self.guest, &self.project_id, &state, rel)?;
                    if hex_sha(&fetched) != target {
                        skipped.insert(rel.to_owned());
                        continue;
                    }
                    data = fetched;
                }
                match write_mac(&self.root, rel, kind, &data, change_mode.unwrap_or(0o644))? {
                    None => {
                        manifest.remove(rel);
                    }
                    Some(entry) => {
                        manifest.insert(rel.to_owned(), entry.with_base(&state));
                    }
                }
                written.insert(rel.to_owned());
            }
        }
        Ok(Outbound {
            agreed,
            written,
            skipped,
            removals: if hold { removals.len() } else { 0 },
        })
    }
}

/// Whether `count` deletions wait for the user's confirmation.
fn too_many(count: usize, allowed: usize, agreed: usize) -> bool {
    count > DELETE_FLOOR
        && count > allowed
        && (count > DELETE_COUNT || count as f64 > agreed as f64 * DELETE_SHARE)
}
