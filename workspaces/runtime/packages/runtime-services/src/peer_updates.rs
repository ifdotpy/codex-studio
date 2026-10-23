//! The offering side of updates from a paired server (docs/peer-updates.md).
//!
//! A paired server asks this server for the build stamp of its installed app
//! (`update.offer`), for an archive of that app (`update.export`), for the
//! archive bytes (`update.chunk`), and for the end of its pull
//! (`update.release`). Every action is a read of the installed app. The archive
//! is a cache file in `<state>/server-exports/`.
//!
//! This module owns the wire limits and shared installer values.
//! `desktop_installer_contract_matches_rust_owner` checks the retained desktop helper.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{Duration, SystemTime};

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::bounded_child::{self, ChildError};
use crate::multi_server_transport_limits::MAX_RESPONSE_BYTES;

/// The orchestration actions of this module.
pub const ACTIONS: [&str; 4] = [
    "update.offer",
    "update.export",
    "update.chunk",
    "update.release",
];
/// The only app that offers its build.
pub const INSTALLED_APP: &str = "/Applications/Codex Studio.app";
/// The build stamp that package.mjs writes, relative to the app bundle.
/// Owner of the path consumed by desktop/desktop_updates.py.
const BUILD_STAMP: &str = "Contents/Resources/studio-build.json";
/// Raw archive bytes in one `update.chunk` answer. Unit: bytes.
/// The offering service owns this chunk limit.
pub const CHUNK_BYTES: usize = 512 * 1024;
/// Largest app archive that this server offers. Unit: bytes.
/// The offering service owns this archive limit.
pub const MAX_ARCHIVE_BYTES: u64 = 1024 * 1024 * 1024;
/// Time that one `update.export` request waits for the archive before it
/// answers `preparing`. The wait is short so that a request does not hold
/// the runtime server's shutdown grace.
pub const EXPORT_WAIT: Duration = Duration::from_secs(5);
/// Time for one ditto archive or install copy, in seconds; shared with the desktop installer.
const DITTO_TIMEOUT: Duration = Duration::from_secs(1800);
/// Time for one codesign run, in seconds; shared with the desktop installer.
const CODESIGN_TIMEOUT: Duration = Duration::from_secs(300);
/// Age after which an update archive is removed although no puller released it.
const EXPORT_AGE: Duration = Duration::from_secs(86_400);
/// The folder of the archive files; Git bundles of the same channel use it too.
const EXPORTS_DIR: &str = crate::server_git::EXPORT_FOLDER;
/// File name suffixes of one export.
const EXPORT_SUFFIX: &str = ".zip";
const PARTIAL_SUFFIX: &str = ".zip.partial";
const METADATA_SUFFIX: &str = ".json";
/// Characters of a tool error that a refusal keeps.
const ERROR_TAIL_CHARS: usize = 300;
const CODESIGN: &str = "/usr/bin/codesign";
const DITTO: &str = "/usr/bin/ditto";

// A base64 chunk and its receipt must stay below the signed response limit.
const _: () = assert!(4 * CHUNK_BYTES.div_ceil(3) + 4096 < MAX_RESPONSE_BYTES);

#[derive(Debug, PartialEq, Eq)]
pub enum UpdateError {
    /// A refusal without an effect. The puller records it (`not_applied`).
    Refused(String),
    /// A file or thread failure. The outcome is unknown.
    Failed(String),
}

fn refused(message: impl Into<String>) -> UpdateError {
    UpdateError::Refused(message.into())
}

fn failed(error: impl std::fmt::Display) -> UpdateError {
    UpdateError::Failed(error.to_string())
}

enum AppSource {
    /// The app that this server process runs from, when it is the installed app.
    Installed,
    #[cfg(test)]
    Fixed(PathBuf),
}

/// The installed app and the programs that check and archive it.
pub struct Offer {
    app: AppSource,
    codesign: PathBuf,
    ditto: PathBuf,
    export_wait: Duration,
    /// Free disk space that an export needs in the state directory. Unit: bytes.
    free_space: u64,
}

impl Offer {
    /// The offer of this server process: its installed app, if it runs from one.
    pub fn installed() -> Self {
        Self {
            app: AppSource::Installed,
            codesign: PathBuf::from(CODESIGN),
            ditto: PathBuf::from(DITTO),
            export_wait: EXPORT_WAIT,
            free_space: MAX_ARCHIVE_BYTES,
        }
    }

    #[cfg(test)]
    pub(crate) fn fixture(app: PathBuf, codesign: PathBuf, ditto: PathBuf) -> Self {
        Self {
            app: AppSource::Fixed(app),
            codesign,
            ditto,
            export_wait: EXPORT_WAIT,
            // The fixture archives are small; a full test disk must not refuse them.
            free_space: 0,
        }
    }

    #[cfg(test)]
    fn with_export_wait(mut self, wait: Duration) -> Self {
        self.export_wait = wait;
        self
    }

    fn app(&self) -> Option<PathBuf> {
        match &self.app {
            AppSource::Installed => installed_app(),
            #[cfg(test)]
            AppSource::Fixed(path) => read_build(path).map(|_| path.clone()),
        }
    }

    /// Answer one `update.*` action of the paired server `principal`.
    /// `root` is the state directory.
    pub async fn receive(
        self: Arc<Self>,
        root: PathBuf,
        principal: String,
        action: String,
        payload: Value,
    ) -> Result<Value, UpdateError> {
        if action == "update.export" {
            return self.export(root, principal, payload).await;
        }
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/peer_updates",
            move || match action.as_str() {
                "update.offer" => {
                    prune(&root, &exports());
                    Ok(json!({"build": self.app().and_then(|app| read_build(&app))}))
                }
                "update.chunk" => chunk(&root, &payload),
                "update.release" => release(&root, &principal, &payload),
                _ => Err(refused("Unknown cross-server operation")),
            },
        )
        .await
        .map_err(failed)?
    }

    async fn export(
        self: Arc<Self>,
        root: PathBuf,
        principal: String,
        payload: Value,
    ) -> Result<Value, UpdateError> {
        let offer = Arc::clone(&self);
        let started = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/peer_updates",
            move || offer.begin_export(&root, &principal, &payload),
        )
        .await
        .map_err(failed)??;
        let (mut done, paths, principal, build) = match started {
            Started::Ready(value) => return Ok(value),
            Started::Running {
                done,
                paths,
                principal,
                build,
            } => (done, paths, principal, build),
        };
        // A timeout leaves the archive work running; the puller asks again.
        drop(tokio::time::timeout(self.export_wait, done.wait_for(|finished| *finished)).await);
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/peer_updates",
            move || finish_export(&paths, &principal, &build),
        )
        .await
        .map_err(failed)?
    }

    fn begin_export(
        &self,
        root: &Path,
        principal: &str,
        payload: &Value,
    ) -> Result<Started, UpdateError> {
        let (app, build) = self
            .app()
            .and_then(|app| read_build(&app).map(|build| (app, build)))
            .ok_or_else(|| refused("This server does not run an installed app"))?;
        if payload.get("builtAt").and_then(Value::as_f64) != build["builtAt"].as_f64() {
            return Err(refused("This server runs another build"));
        }
        let paths = ExportPaths::new(root, &export_id(&build));
        let mut registry = exports();
        // Under the lock: a worker registers before it writes, so prune skips its files.
        prune(root, &registry);
        if let Some(ready) = hold_ready(&mut registry, &paths, &build, principal) {
            return Ok(Started::Ready(ready));
        }
        let done = match registry.running.get(&paths.archive) {
            Some(done) => done.clone(),
            None => {
                registry.errors.remove(&paths.archive);
                let (sender, done) = tokio::sync::watch::channel(false);
                registry.running.insert(paths.archive.clone(), done.clone());
                let work = ExportWork {
                    app,
                    build: build.clone(),
                    paths: paths.clone(),
                    codesign: self.codesign.clone(),
                    ditto: self.ditto.clone(),
                    free_space: self.free_space,
                };
                let spawned = std::thread::Builder::new()
                    .name("studio-update-export".into())
                    .spawn(move || {
                        let result =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work.run()))
                                .unwrap_or_else(|_| Err("The app archive work stopped".into()));
                        let mut registry = exports();
                        registry.running.remove(&work.paths.archive);
                        if let Err(error) = result {
                            registry.errors.insert(work.paths.archive.clone(), error);
                        }
                        drop(registry);
                        sender.send_replace(true);
                    });
                if let Err(error) = spawned {
                    registry.running.remove(&paths.archive);
                    return Err(failed(error));
                }
                done
            }
        };
        Ok(Started::Running {
            done,
            paths,
            principal: principal.to_owned(),
            build,
        })
    }
}

enum Started {
    Ready(Value),
    Running {
        done: tokio::sync::watch::Receiver<bool>,
        paths: ExportPaths,
        principal: String,
        build: Value,
    },
}

/// Archives in work, their last failures and their holders, for this process.
#[derive(Default)]
struct Exports {
    /// Archives that a worker thread writes, with the signal that it ended.
    running: HashMap<PathBuf, tokio::sync::watch::Receiver<bool>>,
    /// The failure of the last worker of an archive, for the next request.
    errors: HashMap<PathBuf, String>,
    /// Paired servers that exported an archive and did not release it.
    holders: HashMap<PathBuf, HashSet<String>>,
}

fn exports() -> MutexGuard<'static, Exports> {
    static EXPORTS: OnceLock<Mutex<Exports>> = OnceLock::new();
    EXPORTS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

#[derive(Clone)]
struct ExportPaths {
    export: String,
    archive: PathBuf,
    metadata: PathBuf,
    partial: PathBuf,
}

impl ExportPaths {
    fn new(root: &Path, export: &str) -> Self {
        let folder = root.join(EXPORTS_DIR);
        Self {
            export: export.to_owned(),
            archive: folder.join(format!("{export}{EXPORT_SUFFIX}")),
            metadata: folder.join(format!("{export}{METADATA_SUFFIX}")),
            partial: folder.join(format!("{export}{PARTIAL_SUFFIX}")),
        }
    }

    /// The paths of an export ID from a puller. Python `_export_paths`.
    fn parse(root: &Path, export: Option<&Value>) -> Result<Self, UpdateError> {
        let export = export
            .and_then(Value::as_str)
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            .ok_or_else(|| refused("Invalid update export"))?;
        Ok(Self::new(root, &export.hyphenated().to_string()))
    }
}

/// One archive of the installed app (Python `_make_export`).
struct ExportWork {
    app: PathBuf,
    build: Value,
    paths: ExportPaths,
    codesign: PathBuf,
    ditto: PathBuf,
    free_space: u64,
}

impl ExportWork {
    fn run(&self) -> Result<(), String> {
        let result = self.write();
        if result.is_err() {
            drop(std::fs::remove_file(&self.paths.partial));
        }
        result
    }

    fn write(&self) -> Result<(), String> {
        let folder = self
            .paths
            .archive
            .parent()
            .ok_or("The export folder is unavailable")?;
        std::fs::create_dir_all(folder).map_err(|error| error.to_string())?;
        if crate::disk_space::free_bytes(folder).is_some_and(|free| free < self.free_space) {
            return Err("Not enough free disk space for the app archive".into());
        }
        match std::fs::remove_file(&self.paths.partial) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        remove_bytecode(&self.app);
        // A paired Mac verifies the same seal; an app with changed files cannot pass it on.
        tool(
            &self.codesign,
            &[
                "--verify",
                "--deep",
                "--strict",
                &self.app.to_string_lossy(),
            ],
            CODESIGN_TIMEOUT,
        )
        .map_err(|error| format!("The installed app fails its signature check: {error}"))?;
        tool(
            &self.ditto,
            &[
                "-c",
                "-k",
                "--keepParent",
                &self.app.to_string_lossy(),
                &self.paths.partial.to_string_lossy(),
            ],
            DITTO_TIMEOUT,
        )
        .map_err(|error| format!("ditto could not archive the app: {error}"))?;
        let size = std::fs::metadata(&self.paths.partial)
            .map_err(|error| error.to_string())?
            .len();
        if size == 0 || size > MAX_ARCHIVE_BYTES {
            return Err("The app archive exceeds the size limit".into());
        }
        let digest = crate::server_git::file_digest(&self.paths.partial)
            .map_err(|_| "The app archive could not be read".to_owned())?;
        std::fs::rename(&self.paths.partial, &self.paths.archive)
            .map_err(|error| error.to_string())?;
        let metadata = json!({
            "export": self.paths.export,
            "size": size,
            "sha256": digest,
            "build": self.build,
        });
        crate::atomic_file::replace(
            &self.paths.metadata,
            metadata.to_string().as_bytes(),
            Some(0o600),
            true,
        )
        .map_err(|error| error.to_string())
    }
}

/// Run codesign or ditto with a deadline. The error keeps the end of its output.
fn tool(program: &Path, args: &[&str], timeout: Duration) -> Result<(), String> {
    bounded_child::run(bounded_child::Request {
        program: program.as_os_str(),
        git_directory: None,
        args,
        environment: &[],
        input: None,
        stdout_limit: 4096,
        stderr_limit: 4096,
        timeout,
    })
    .map(drop)
    .map_err(|error| {
        let text = match error {
            ChildError::Failed(message) | ChildError::Launch(message) => message,
            ChildError::TimedOut => "the program did not finish in time".to_owned(),
        };
        let skip = text.chars().count().saturating_sub(ERROR_TAIL_CHARS);
        text.chars().skip(skip).collect()
    })
}

/// The answer for an archive that is complete, after `principal` holds it.
fn hold_ready(
    registry: &mut Exports,
    paths: &ExportPaths,
    build: &Value,
    principal: &str,
) -> Option<Value> {
    let ready = ready_export(paths, build)?;
    registry
        .holders
        .entry(paths.archive.clone())
        .or_default()
        .insert(principal.to_owned());
    Some(ready)
}

/// Python `_ready_export`.
fn ready_export(paths: &ExportPaths, build: &Value) -> Option<Value> {
    let metadata: Value = serde_json::from_slice(&std::fs::read(&paths.metadata).ok()?).ok()?;
    if &metadata["build"] != build {
        return None;
    }
    let size = std::fs::metadata(&paths.archive)
        .ok()
        .filter(std::fs::Metadata::is_file)?
        .len();
    if metadata["size"].as_u64() != Some(size) {
        return None;
    }
    Some(json!({"export": paths.export, "size": size, "sha256": metadata["sha256"]}))
}

fn finish_export(
    paths: &ExportPaths,
    principal: &str,
    build: &Value,
) -> Result<Value, UpdateError> {
    let mut registry = exports();
    if !registry.running.contains_key(&paths.archive) {
        if let Some(error) = registry.errors.remove(&paths.archive) {
            return Err(UpdateError::Refused(error));
        }
        if let Some(ready) = hold_ready(&mut registry, paths, build, principal) {
            return Ok(ready);
        }
    }
    Ok(json!({"export": paths.export, "preparing": true}))
}

/// Python `receive` for `update.chunk`.
fn chunk(root: &Path, payload: &Value) -> Result<Value, UpdateError> {
    let paths = ExportPaths::parse(root, payload.get("export"))?;
    let mut file = match std::fs::File::open(&paths.archive) {
        Ok(file) if file.metadata().is_ok_and(|metadata| metadata.is_file()) => file,
        Ok(_) => return Err(refused("The update archive has expired or was released")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(refused("The update archive has expired or was released"));
        }
        Err(error) => return Err(failed(error)),
    };
    let size = file.metadata().map_err(failed)?.len();
    let offset = payload
        .get("offset")
        .and_then(Value::as_u64)
        .filter(|offset| *offset < size)
        .ok_or_else(|| refused("Invalid update archive offset"))?;
    file.seek(SeekFrom::Start(offset)).map_err(failed)?;
    let mut data = Vec::with_capacity(CHUNK_BYTES);
    file.take(CHUNK_BYTES as u64)
        .read_to_end(&mut data)
        .map_err(failed)?;
    Ok(json!({
        "offset": offset,
        "nextOffset": offset + data.len() as u64,
        "data": base64::engine::general_purpose::STANDARD.encode(&data),
        "sha256": data_encoding::HEXLOWER.encode(&Sha256::digest(&data)),
    }))
}

/// Python `receive` for `update.release`: the last holder removes the archive.
fn release(root: &Path, principal: &str, payload: &Value) -> Result<Value, UpdateError> {
    let paths = ExportPaths::parse(root, payload.get("export"))?;
    let mut registry = exports();
    let holders = registry.holders.entry(paths.archive.clone()).or_default();
    holders.remove(principal);
    if holders.is_empty() && !registry.running.contains_key(&paths.archive) {
        registry.holders.remove(&paths.archive);
        for path in [&paths.archive, &paths.metadata, &paths.partial] {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(failed(error)),
            }
        }
    }
    Ok(json!({"released": true}))
}

/// Remove update archive files older than a day that no worker writes
/// (Python `MultiServerOrchestration.prune`). The caller holds the registry lock.
fn prune(root: &Path, registry: &Exports) {
    let Ok(entries) = std::fs::read_dir(root.join(EXPORTS_DIR)) else {
        return;
    };
    let Some(cutoff) = SystemTime::now().checked_sub(EXPORT_AGE) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(export) = [PARTIAL_SUFFIX, EXPORT_SUFFIX, METADATA_SUFFIX]
            .iter()
            .find_map(|suffix| name.strip_suffix(suffix))
        else {
            continue;
        };
        let archive = path.with_file_name(format!("{export}{EXPORT_SUFFIX}"));
        if registry.running.contains_key(&archive) {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified < cutoff);
        if old {
            drop(std::fs::remove_file(&path));
        }
    }
}

/// Python `remove_bytecode`: Python caches outside the signature seal would
/// fail the seal check. The package makes each cache folder read-only, so only
/// an app from before that guard has them.
fn remove_bytecode(app: &Path) {
    fn walk(folder: &Path, cache: bool) {
        let Ok(entries) = std::fs::read_dir(folder) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                walk(&path, entry.file_name() == "__pycache__");
            } else if cache
                && kind.is_file()
                && path.extension().is_some_and(|extension| extension == "pyc")
            {
                drop(std::fs::remove_file(&path));
            }
        }
    }
    walk(&app.join("Contents/Resources/workspace"), false);
}

/// Python `export_id`: one archive name per build.
fn export_id(build: &Value) -> String {
    let key = format!(
        "studio-peer-update:{}:{}",
        build["builtAt"],
        build["commit"].as_str().unwrap_or_default()
    );
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, key.as_bytes())
        .hyphenated()
        .to_string()
}

/// Python `valid_build`: the build stamp, or `None` for an incomplete stamp.
fn valid_build(value: &Value) -> Option<Value> {
    let object = value.as_object()?;
    let built = object.get("builtAt").filter(|built| built.is_number())?;
    let commit = object.get("commit").and_then(Value::as_str)?;
    let commit_time = object
        .get("commitTime")
        .filter(|time| time.is_i64() || time.is_u64())
        .and_then(Value::as_i64)?;
    let branch = object.get("branch").and_then(Value::as_str)?;
    let complete = object.get("version").and_then(Value::as_u64) == Some(1)
        && built
            .as_f64()
            .is_some_and(|built| built.is_finite() && built > 0.0)
        && matches!(commit.len(), 40 | 64)
        && commit
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        && commit_time > 0
        && branch.chars().count() <= 255;
    complete.then(|| {
        json!({
            "version": 1,
            "builtAt": built,
            "commit": commit,
            "commitTime": commit_time,
            "branch": branch,
        })
    })
}

/// Python `read_build`.
fn read_build(app: &Path) -> Option<Value> {
    valid_build(&serde_json::from_slice(&std::fs::read(app.join(BUILD_STAMP)).ok()?).ok()?)
}

/// Python `installed_app`: the installed app that this process runs from, or
/// `None` for a source build or another copy of the app.
fn installed_app() -> Option<PathBuf> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let executable = std::env::current_exe().ok()?;
    let executable =
        studio_runtime_store::path_identity::CanonicalPath::existing(executable).ok()?;
    let app = Path::new(INSTALLED_APP);
    if !executable.starts_with(app.join("Contents")) {
        return None;
    }
    read_build(app).map(|_| app.to_path_buf())
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
pub(crate) mod fixture {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use serde_json::{Value, json};

    use super::Offer;

    /// The build stamp of the fixture app.
    pub(crate) fn build() -> Value {
        json!({
            "version": 1,
            "builtAt": 1792647971.037,
            "commit": "f37b5f29ee083343f2133d34482902e416a0a5f1",
            "commitTime": 1792623937,
            "branch": "HEAD",
        })
    }

    /// A fake installed app with the fixture build stamp.
    pub(crate) fn app(folder: &Path) -> PathBuf {
        let app = folder.join("Codex Studio.app");
        let resources = app.join("Contents/Resources");
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::write(resources.join("studio-build.json"), build().to_string()).unwrap();
        app
    }

    /// A fake ditto that waits for `gate`, when given, and then writes
    /// `archive` as the app archive (its fifth argument).
    pub(crate) fn ditto(folder: &Path, name: &str, archive: &[u8], gate: Option<&Path>) -> PathBuf {
        let blob = folder.join(format!("{name}.bin"));
        std::fs::write(&blob, archive).unwrap();
        let wait = gate.map_or_else(String::new, |gate| {
            format!("while [ ! -e '{}' ]; do sleep 0.05; done\n", gate.display())
        });
        script(
            folder,
            name,
            &format!("{wait}cp '{}' \"$5\"", blob.display()),
        )
    }

    /// The fake app, a codesign that accepts it, and a ditto that writes `archive`.
    pub(crate) fn offer(folder: &Path, archive: &[u8]) -> Offer {
        Offer::fixture(
            app(folder),
            script(folder, "codesign", "exit 0"),
            ditto(folder, "ditto", archive, None),
        )
    }

    pub(crate) fn script(folder: &Path, name: &str, body: &str) -> PathBuf {
        let path = folder.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use base64::Engine;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    use super::{CHUNK_BYTES, EXPORT_WAIT, Offer, UpdateError, fixture};

    async fn call(
        offer: &Arc<Offer>,
        root: &std::path::Path,
        action: &str,
        payload: Value,
    ) -> Result<Value, UpdateError> {
        Arc::clone(offer)
            .receive(root.to_path_buf(), "peer-a".into(), action.into(), payload)
            .await
    }

    fn archive_bytes() -> Vec<u8> {
        (0..2 * CHUNK_BYTES + 123)
            .map(|index| (index % 251) as u8)
            .collect()
    }

    #[tokio::test]
    async fn offer_names_the_installed_build_and_no_build_without_an_app() {
        let folder = tempfile::tempdir().unwrap();
        let offer = Arc::new(fixture::offer(folder.path(), b"zip"));
        let value = call(&offer, folder.path(), "update.offer", json!({}))
            .await
            .unwrap();
        assert_eq!(value, json!({"build": fixture::build()}));

        let missing = Arc::new(Offer::fixture(
            folder.path().join("missing.app"),
            folder.path().join("codesign"),
            folder.path().join("ditto"),
        ));
        let value = call(&missing, folder.path(), "update.offer", json!({}))
            .await
            .unwrap();
        assert_eq!(value, json!({"build": null}));
        assert_eq!(
            call(
                &missing,
                folder.path(),
                "update.export",
                json!({"builtAt": 1792647971.037})
            )
            .await,
            Err(UpdateError::Refused(
                "This server does not run an installed app".into()
            ))
        );
    }

    #[tokio::test]
    async fn export_chunks_and_release_return_the_exact_archive() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path().join("state");
        std::fs::create_dir(&root).unwrap();
        let expected = archive_bytes();
        let offer = Arc::new(fixture::offer(folder.path(), &expected));
        let cache = folder
            .path()
            .join("Codex Studio.app/Contents/Resources/workspace/scripts/__pycache__");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("module.cpython-314.pyc"), b"cache").unwrap();
        std::fs::write(cache.join("keep.txt"), b"not bytecode").unwrap();

        let meta = call(
            &offer,
            &root,
            "update.export",
            json!({"builtAt": 1792647971.037}),
        )
        .await
        .unwrap();
        assert_eq!(meta["size"], json!(expected.len()));
        assert_eq!(
            meta["sha256"],
            json!(data_encoding::HEXLOWER.encode(&Sha256::digest(&expected)))
        );
        assert!(!cache.join("module.cpython-314.pyc").exists());
        assert!(cache.join("keep.txt").exists());
        let export = meta["export"].as_str().unwrap().to_owned();
        let again = call(
            &offer,
            &root,
            "update.export",
            json!({"builtAt": 1792647971.037}),
        )
        .await
        .unwrap();
        assert_eq!(again, meta, "a second export reuses the archive");

        let mut received = Vec::new();
        while received.len() < expected.len() {
            let chunk = call(
                &offer,
                &root,
                "update.chunk",
                json!({"export": export, "offset": received.len()}),
            )
            .await
            .unwrap();
            let data = base64::engine::general_purpose::STANDARD
                .decode(chunk["data"].as_str().unwrap())
                .unwrap();
            assert!(!data.is_empty() && data.len() <= CHUNK_BYTES);
            assert_eq!(chunk["offset"], json!(received.len()));
            assert_eq!(chunk["nextOffset"], json!(received.len() + data.len()));
            assert_eq!(
                chunk["sha256"],
                json!(data_encoding::HEXLOWER.encode(&Sha256::digest(&data)))
            );
            received.extend(data);
        }
        assert_eq!(received, expected);
        assert_eq!(
            call(
                &offer,
                &root,
                "update.chunk",
                json!({"export": export, "offset": expected.len()})
            )
            .await,
            Err(UpdateError::Refused("Invalid update archive offset".into()))
        );

        assert_eq!(
            call(&offer, &root, "update.release", json!({"export": export}))
                .await
                .unwrap(),
            json!({"released": true})
        );
        let exports = root.join("server-exports");
        assert_eq!(std::fs::read_dir(&exports).unwrap().count(), 0);
        assert_eq!(
            call(
                &offer,
                &root,
                "update.chunk",
                json!({"export": export, "offset": 0})
            )
            .await,
            Err(UpdateError::Refused(
                "The update archive has expired or was released".into()
            ))
        );
    }

    #[tokio::test]
    async fn export_refuses_another_build_and_an_app_that_fails_its_seal() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path().join("state");
        std::fs::create_dir(&root).unwrap();
        let offer = fixture::offer(folder.path(), b"zip");
        assert_eq!(
            call(
                &Arc::new(offer),
                &root,
                "update.export",
                json!({"builtAt": 1792606701.157})
            )
            .await,
            Err(UpdateError::Refused(
                "This server runs another build".into()
            ))
        );

        let broken = Arc::new(Offer::fixture(
            fixture::app(folder.path()),
            fixture::script(
                folder.path(),
                "codesign-broken",
                "echo 'a sealed resource is modified' >&2; exit 1",
            ),
            folder.path().join("ditto"),
        ));
        assert_eq!(
            call(
                &broken,
                &root,
                "update.export",
                json!({"builtAt": 1792647971.037})
            )
            .await,
            Err(UpdateError::Refused(
                "The installed app fails its signature check: a sealed resource is modified".into()
            ))
        );
        assert_eq!(
            std::fs::read_dir(root.join("server-exports"))
                .unwrap()
                .count(),
            0,
            "a failed export leaves no archive"
        );
    }

    #[tokio::test]
    async fn export_answers_preparing_until_the_archive_is_ready() {
        let folder = tempfile::tempdir().unwrap();
        let root = folder.path().join("state");
        std::fs::create_dir(&root).unwrap();
        let gate = folder.path().join("gate");
        let app = fixture::app(folder.path());
        let codesign = fixture::script(folder.path(), "codesign", "exit 0");
        let ditto = fixture::ditto(folder.path(), "ditto-slow", b"slow archive", Some(&gate));
        let waiting = Arc::new(
            Offer::fixture(app.clone(), codesign.clone(), ditto.clone())
                .with_export_wait(Duration::from_millis(20)),
        );
        let first = call(
            &waiting,
            &root,
            "update.export",
            json!({"builtAt": 1792647971.037}),
        )
        .await
        .unwrap();
        assert_eq!(first["preparing"], json!(true));
        std::fs::write(&gate, b"").unwrap();
        let patient = Arc::new(Offer::fixture(app, codesign, ditto));
        let ready = call(
            &patient,
            &root,
            "update.export",
            json!({"builtAt": 1792647971.037}),
        )
        .await
        .unwrap();
        assert_eq!(ready["export"], first["export"]);
        assert_eq!(ready["size"], json!(b"slow archive".len()));
    }

    #[tokio::test]
    async fn chunk_and_release_refuse_an_invalid_export() {
        let folder = tempfile::tempdir().unwrap();
        let offer = Arc::new(fixture::offer(folder.path(), b"zip"));
        for payload in [
            json!({"export": "not-a-uuid", "offset": 0}),
            json!({"offset": 0}),
        ] {
            assert_eq!(
                call(&offer, folder.path(), "update.chunk", payload).await,
                Err(UpdateError::Refused("Invalid update export".into()))
            );
        }
        assert_eq!(
            call(
                &offer,
                folder.path(),
                "update.release",
                json!({"export": 7})
            )
            .await,
            Err(UpdateError::Refused("Invalid update export".into()))
        );
    }

    #[tokio::test]
    async fn offer_removes_only_day_old_update_archives() {
        let folder = tempfile::tempdir().unwrap();
        let exports = folder.path().join("server-exports");
        std::fs::create_dir(&exports).unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(2 * 86_400);
        for name in [
            "old.zip",
            "old.json",
            "old.zip.partial",
            "old.bundle",
            "new.zip",
        ] {
            let file = std::fs::File::create(exports.join(name)).unwrap();
            if name.starts_with("old") {
                file.set_modified(old).unwrap();
            }
        }
        let offer = Arc::new(fixture::offer(folder.path(), b"zip"));
        call(&offer, folder.path(), "update.offer", json!({}))
            .await
            .unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&exports)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(
            left,
            ["new.zip", "old.bundle"],
            "Git bundles are not update archives"
        );
    }

    #[test]
    fn build_stamps_follow_the_python_validation() {
        assert_eq!(
            super::valid_build(&fixture::build()),
            Some(fixture::build())
        );
        let mut extra = fixture::build();
        extra["unknown"] = json!(true);
        assert_eq!(super::valid_build(&extra), Some(fixture::build()));
        for (field, value) in [
            ("version", json!(2)),
            ("builtAt", json!("1792647971.037")),
            ("builtAt", json!(0)),
            ("commit", json!("F37B5F29EE083343F2133D34482902E416A0A5F1")),
            ("commit", json!("f37b5f29e")),
            ("commitTime", json!(1792623937.5)),
            ("commitTime", json!(true)),
            ("branch", json!("b".repeat(256))),
        ] {
            let mut stamp = fixture::build();
            stamp[field] = value;
            assert_eq!(super::valid_build(&stamp), None, "{field}");
        }
    }

    #[test]
    fn desktop_installer_contract_matches_rust_owner() {
        const INSTALLER: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../client/apps/desktop/desktop_updates.py"
        ));
        // Rust owns the shared values; the retained Python helper is the consumer.
        for line in [
            format!("BUILD_STAMP = Path('{}')", super::BUILD_STAMP),
            format!("DITTO_SECONDS = {}", super::DITTO_TIMEOUT.as_secs()),
            format!("CODESIGN_SECONDS = {}", super::CODESIGN_TIMEOUT.as_secs()),
            format!("DITTO = '{}'", super::DITTO),
            format!("CODESIGN = '{}'", super::CODESIGN),
        ] {
            assert!(
                INSTALLER.lines().any(|actual| actual == line),
                "desktop_updates.py must consume peer_updates.rs owner: {line}"
            );
        }
        assert_eq!(super::INSTALLED_APP, "/Applications/Codex Studio.app");
        assert_eq!(super::BUILD_STAMP, "Contents/Resources/studio-build.json");
        assert_eq!(CHUNK_BYTES, 512 * 1024);
        assert_eq!(super::MAX_ARCHIVE_BYTES, 1024 * 1024 * 1024);
        assert!(EXPORT_WAIT <= Duration::from_secs(90));
        assert_eq!(super::PARTIAL_SUFFIX, ".zip.partial");
        assert_eq!(super::DITTO_TIMEOUT, Duration::from_secs(1800));
        assert_eq!(super::CODESIGN_TIMEOUT, Duration::from_secs(300));
        assert_eq!(super::EXPORTS_DIR, "server-exports");
        assert_eq!(
            (super::EXPORT_SUFFIX, super::METADATA_SUFFIX),
            (".zip", ".json")
        );
        assert_eq!(super::EXPORT_AGE, Duration::from_secs(86_400));
        assert_eq!(
            super::ACTIONS,
            [
                "update.offer",
                "update.export",
                "update.chunk",
                "update.release"
            ]
        );
    }
}
