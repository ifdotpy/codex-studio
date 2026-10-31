//! The receiving side of updates from a paired server (docs/peer-updates.md).
//!
//! The backend of the installed app asks each reachable paired server for its build
//! (`update.offer`), pulls the newest build that is newer than its own, and stages it at
//! `<state>/updates/<builtAt>/Codex Studio.app`. It then writes `pending-update.json` for the
//! desktop installer. It never replaces the installed app itself.
//!
//! Each chunk and the whole archive are checked by hash. The archive is extracted into a
//! private folder and the app is verified like the installed app. The offering side is in
//! `peer_updates.rs`, which owns the wire limits.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

use studio_runtime_schema::desktop::{
    PeerUpdateBuild, PeerUpdateEvent, PeerUpdatePending, PeerUpdateStatus,
};
use studio_runtime_store::RuntimeStore;

use crate::atomic_file;
use crate::disk_space;
use crate::multi_server::{paired_servers, peer_auto_update};
use crate::multi_server_orchestration::{HttpsPairedTransport, PairedTransport};
use crate::peer_updates::{
    self, CHUNK_BYTES, CODESIGN, CODESIGN_TIMEOUT, DITTO, DITTO_TIMEOUT, MAX_ARCHIVE_BYTES,
};

/// Name of the installed app in the update folder. Owner of the name that the desktop
/// installer reads (`APP_NAME` in `desktop/desktop_updates.py`).
pub const APP_NAME: &str = "Codex Studio.app";
/// State directory files and folder. Owner of the names that the desktop installer reads.
pub const PENDING_FILE: &str = "pending-update.json";
pub const RECEIPT_FILE: &str = "update-receipt.json";
pub const UPDATES_DIR: &str = "updates";
/// The status file of this puller: the last check, the failures and the last pull.
pub const STATUS_FILE: &str = "peer-update.json";
/// Name prefix of the private folder that one pull extracts its archive into.
const EXTRACT_PREFIX: &str = ".extract-";
/// Seconds between two pulls. Unit: seconds.
pub const PULL_INTERVAL: Duration = Duration::from_secs(600);
/// Seconds from the start to the first pull. Unit: seconds.
pub const FIRST_PULL: Duration = Duration::from_secs(120);
/// Seconds that a build is skipped after its pull failed or its install receipt failed.
/// Unit: seconds.
const FAILURE_RETRY_SECONDS: i64 = 3600;
/// Seconds that a failure record stays in the status file. Unit: seconds.
const FAILURE_RECORD_SECONDS: i64 = 86_400;
/// Seconds that one update.offer or update.release request waits. Unit: seconds.
const SHORT_REQUEST: Duration = Duration::from_secs(20);
/// Seconds that one update.export request waits. The peer answers `preparing` after its
/// own 5-second wait. Unit: seconds.
const EXPORT_REQUEST: Duration = Duration::from_secs(30);
/// Seconds that one update.chunk request waits. Unit: seconds.
const CHUNK_REQUEST: Duration = Duration::from_secs(60);
/// Seconds between two update.export requests while the peer prepares the archive.
/// Unit: seconds.
const EXPORT_POLL: Duration = Duration::from_secs(10);
/// Seconds that one pull waits in total for a peer to prepare its archive. Unit: seconds.
const EXPORT_PREPARE: Duration = Duration::from_secs(1200);
/// Free disk space that a pull needs, as a multiple of the archive size. Unit: times.
const DISK_FACTOR: u64 = 4;
/// Message of a receipt that does not answer the request. Shared by every reply check.
const INVALID_RECEIPT: &str = "The paired server returned an invalid receipt";
/// Characters of a failure reason that the status file keeps.
const REASON_CHARS: usize = 300;
/// Label of the store reads of this module (the store names each read for its metrics).
const LABEL: &str = "runtime-services/peer_update_pull";

/// Unix time in seconds.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The puller of one installed app. A timer task runs its passes one after another.
pub struct Puller {
    store: Arc<RuntimeStore>,
    transport: Arc<dyn PairedTransport>,
    /// The installed app that this backend runs from; its build stamp is the local build.
    app: PathBuf,
    codesign: PathBuf,
    ditto: PathBuf,
    clock: Clock,
    stop: watch::Receiver<bool>,
}

/// The result of one pass.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// The `autoUpdate` setting is off. No server is asked.
    Disabled,
    /// This process does not run from the installed app, so it has no build stamp.
    NoBuild,
    /// No newer build, or a pending update already holds a build that is as new.
    Idle,
    /// The build is staged in the state directory and named by the pending file.
    Staged { build: Value, server: String },
    /// The pull of the build failed; the reason is recorded in the status file.
    Failed { build: Value, reason: String },
    /// The backend stopped during the pull. The pull records nothing.
    Stopped,
}

/// A step that ended a pull early.
#[derive(Debug)]
enum Step {
    /// The paired server refused the request (an older server does not know the action).
    Refused(String),
    /// A check or a file step failed. The text names it.
    Failed(String),
    /// The backend stops.
    Stopped,
}

impl Step {
    fn reason(&self) -> String {
        match self {
            Self::Refused(reason) | Self::Failed(reason) => reason.clone(),
            Self::Stopped => "The backend stopped".to_owned(),
        }
    }
}

fn failed(error: impl std::fmt::Display) -> Step {
    Step::Failed(error.to_string())
}

impl Puller {
    pub fn new(
        store: Arc<RuntimeStore>,
        transport: Arc<dyn PairedTransport>,
        app: PathBuf,
        codesign: PathBuf,
        ditto: PathBuf,
        clock: Clock,
        stop: watch::Receiver<bool>,
    ) -> Self {
        Self {
            store,
            transport,
            app,
            codesign,
            ditto,
            clock,
            stop,
        }
    }

    /// The puller of this process, or None when it does not run from the installed app.
    pub fn installed(store: Arc<RuntimeStore>, stop: watch::Receiver<bool>) -> Option<Self> {
        let app = peer_updates::installed_app()?;
        let transport = Arc::new(HttpsPairedTransport::new(Arc::clone(&store)));
        Some(Self::new(
            store,
            transport,
            app,
            PathBuf::from(CODESIGN),
            PathBuf::from(DITTO),
            Arc::new(unix_time),
            stop,
        ))
    }

    fn root(&self) -> &Path {
        self.store.root()
    }

    /// One pass. An error means that the pass could not read or write its files.
    pub async fn pull(&self) -> Result<Outcome, String> {
        self.pass().await.map_err(|step| step.reason())
    }

    async fn pass(&self) -> Result<Outcome, Step> {
        let Some(local) = peer_updates::read_build(&self.app) else {
            return Ok(Outcome::NoBuild);
        };
        if !self.enabled().await? {
            return Ok(Outcome::Disabled);
        }
        let now = (self.clock)();
        let root = self.root().to_path_buf();
        let offers = self.offers(&local).await?;
        let mut status = read_json(&root.join(STATUS_FILE)).unwrap_or_else(|| json!({}));
        status["checkedAt"] = json!(now);
        let failures = kept_failures(&status, now);
        status["failures"] = failures;
        if let Some(stale) =
            read_pending(&root).filter(|pending| built_at(&pending["build"]) <= built_at(&local))
        {
            // Another install already brought this build or a newer one.
            drop(std::fs::remove_file(root.join(PENDING_FILE)));
            remove_tree(&build_folder(&root, &stale["build"]));
        }
        let pending = read_pending(&root);
        let choice = offers
            .into_iter()
            .find(|(build, _)| !self.recent_failure(&status, build, now));
        let Some((build, server)) = choice.filter(|(build, _)| {
            pending
                .as_ref()
                .is_none_or(|pending| built_at(&pending["build"]) < built_at(build))
        }) else {
            write_json(&root.join(STATUS_FILE), &status)?;
            return Ok(Outcome::Idle);
        };
        match self.fetch(&server, &build).await {
            Ok(app) => {
                write_json(
                    &root.join(PENDING_FILE),
                    &json!({
                        "id": pending_id(&server, &build),
                        "app": app.to_string_lossy(),
                        "build": build,
                        "from": server,
                    }),
                )?;
                if let Some(old_folder) = pending
                    .as_ref()
                    .and_then(|pending| pending["app"].as_str())
                    .and_then(|text| Path::new(text).parent())
                    .filter(|folder| Some(*folder) != app.parent())
                {
                    remove_tree(old_folder);
                }
                status["lastPull"] = json!({
                    "at": now, "server": server, "build": build,
                    "outcome": "staged", "reason": null,
                });
                write_json(&root.join(STATUS_FILE), &status)?;
                Ok(Outcome::Staged { build, server })
            }
            Err(Step::Stopped) => Ok(Outcome::Stopped),
            Err(step) => {
                let reason: String = step.reason().chars().take(REASON_CHARS).collect();
                status["failures"][build_key(&build)] =
                    json!({"at": now, "reason": reason, "server": server});
                status["lastPull"] = json!({
                    "at": now, "server": server, "build": build,
                    "outcome": "failed", "reason": reason,
                });
                write_json(&root.join(STATUS_FILE), &status)?;
                Ok(Outcome::Failed { build, reason })
            }
        }
    }

    async fn enabled(&self) -> Result<bool, Step> {
        self.store
            .read_async(LABEL, peer_auto_update)
            .await
            .map_err(failed)
    }

    /// The newer builds that the reachable paired servers offer, newest first.
    async fn offers(&self, local: &Value) -> Result<Vec<(Value, String)>, Step> {
        let servers = self
            .store
            .read_async(LABEL, paired_servers)
            .await
            .map_err(failed)?;
        let mut found = Vec::new();
        for server in servers {
            let Some(id) = server["id"].as_str() else {
                continue;
            };
            if server["reachability"].as_str() == Some("unreachable") {
                continue;
            }
            // An offline server, or an older one that refuses the action, offers nothing.
            let Ok(value) = self
                .call(id, "update.offer", json!({}), SHORT_REQUEST)
                .await
            else {
                continue;
            };
            if let Some(build) = value.get("build").and_then(peer_updates::valid_build)
                && built_at(&build) > built_at(local)
            {
                found.push((build, id.to_owned()));
            }
        }
        found.sort_by(|a, b| built_at(&b.0).total_cmp(&built_at(&a.0)));
        Ok(found)
    }

    /// One request to a paired server. Every update.* action is a read, so each call
    /// gets its own request ID.
    async fn call(
        &self,
        server: &str,
        action: &str,
        payload: Value,
        timeout: Duration,
    ) -> Result<Value, Step> {
        let key = uuid::Uuid::new_v4().hyphenated().to_string();
        let envelope = json!({"requestId": key, "action": action, "payload": payload});
        let receipt = self
            .transport
            .exchange(server, &envelope, timeout)
            .await
            .map_err(failed)?;
        if receipt["requestId"] != key.as_str() {
            return Err(Step::Failed(INVALID_RECEIPT.to_owned()));
        }
        match receipt["outcome"].as_str() {
            Some("applied") if receipt["value"].is_object() => Ok(receipt["value"].clone()),
            Some("applied") => Err(Step::Failed(INVALID_RECEIPT.to_owned())),
            Some("not_applied") => Err(Step::Refused(
                receipt["error"]
                    .as_str()
                    .unwrap_or("The paired server refused the request")
                    .to_owned(),
            )),
            _ => Err(Step::Failed("The paired server did not answer".to_owned())),
        }
    }

    fn check_stop(&self) -> Result<(), Step> {
        if *self.stop.borrow() {
            Err(Step::Stopped)
        } else {
            Ok(())
        }
    }

    async fn wait(&self, delay: Duration) -> Result<(), Step> {
        let mut stop = self.stop.clone();
        tokio::select! {
            () = tokio::time::sleep(delay) => Ok(()),
            _ = stop.wait_for(|stopped| *stopped) => Err(Step::Stopped),
        }
    }

    /// Whether a build failed here recently: its pull, or its install receipt.
    fn recent_failure(&self, status: &Value, build: &Value, now: i64) -> bool {
        let failed_here = within(
            status["failures"][build_key(build)]["at"].as_f64(),
            now,
            FAILURE_RETRY_SECONDS,
        );
        let receipt = read_json(&self.root().join(RECEIPT_FILE));
        failed_here
            || receipt.is_some_and(|receipt| {
                receipt["outcome"] == "failed"
                    && receipt["build"] == *build
                    && within(receipt["at"].as_f64(), now, FAILURE_RETRY_SECONDS)
            })
    }

    /// Pull the archive of `build` from `server` and stage its app. The peer export is
    /// released after the pull, also after a failure.
    async fn fetch(&self, server: &str, build: &Value) -> Result<PathBuf, Step> {
        let meta = self.prepare_export(server, build).await?;
        let result = self.download(server, build, &meta).await;
        // The peer removes an export that no puller releases after a day.
        drop(
            self.call(
                server,
                "update.release",
                json!({"export": meta["export"]}),
                SHORT_REQUEST,
            )
            .await,
        );
        result
    }

    /// Ask for the export until the peer has the archive ready.
    async fn prepare_export(&self, server: &str, build: &Value) -> Result<Value, Step> {
        let deadline = tokio::time::Instant::now() + EXPORT_PREPARE;
        loop {
            self.check_stop()?;
            let meta = self
                .call(
                    server,
                    "update.export",
                    json!({"builtAt": build["builtAt"]}),
                    EXPORT_REQUEST,
                )
                .await?;
            if meta["preparing"].as_bool() != Some(true) {
                return Ok(meta);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Step::Failed(
                    "The paired server did not prepare the app archive in time".to_owned(),
                ));
            }
            self.wait(EXPORT_POLL).await?;
        }
    }

    async fn download(&self, server: &str, build: &Value, meta: &Value) -> Result<PathBuf, Step> {
        let invalid = || Step::Failed("Invalid update archive size or checksum".to_owned());
        let export = meta["export"]
            .as_str()
            .filter(|export| uuid::Uuid::parse_str(export).is_ok())
            .ok_or_else(|| Step::Failed("Invalid update export".to_owned()))?;
        let size = meta["size"]
            .as_u64()
            .filter(|size| (1..=MAX_ARCHIVE_BYTES).contains(size))
            .ok_or_else(invalid)?;
        let digest = meta["sha256"]
            .as_str()
            .filter(|digest| digest.len() == 64)
            .ok_or_else(invalid)?;
        let updates = self.root().join(UPDATES_DIR);
        tokio::fs::create_dir_all(&updates).await.map_err(failed)?;
        remove_extractions(&updates);
        if disk_space::free_bytes(&updates)
            .is_some_and(|free| free < size.saturating_mul(DISK_FACTOR))
        {
            return Err(Step::Failed(
                "Not enough free disk space for the update".to_owned(),
            ));
        }
        let partial = updates.join(format!("{}.zip.partial", build_name(build)));
        let result = match self.receive(server, export, size, &partial).await {
            Ok(actual) if actual == digest => {
                let (partial, updates, build) = (partial.clone(), updates.clone(), build.clone());
                let (installed, codesign, ditto) =
                    (self.app.clone(), self.codesign.clone(), self.ditto.clone());
                studio_runtime_process::blocking_pool::spawn_blocking(LABEL, move || {
                    stage_app(&partial, &updates, &build, &installed, &codesign, &ditto)
                })
                .await
                .map_err(failed)?
            }
            Ok(_) => Err(Step::Failed(
                "The update archive checksum differs".to_owned(),
            )),
            Err(error) => Err(error),
        };
        drop(tokio::fs::remove_file(&partial).await);
        result
    }

    /// Receive the archive in chunks from offset 0. Each chunk must match its offset and its
    /// hash. Returns the SHA-256 of the whole archive.
    async fn receive(
        &self,
        server: &str,
        export: &str,
        size: u64,
        partial: &Path,
    ) -> Result<String, Step> {
        let mut file = tokio::fs::File::create(partial).await.map_err(failed)?;
        let mut whole = Sha256::new();
        let mut offset = 0_u64;
        while offset < size {
            self.check_stop()?;
            let chunk = self
                .call(
                    server,
                    "update.chunk",
                    json!({"export": export, "offset": offset}),
                    CHUNK_REQUEST,
                )
                .await?;
            let data = chunk_data(&chunk, offset, size)
                .ok_or_else(|| Step::Failed("Invalid update archive chunk".to_owned()))?;
            file.write_all(&data).await.map_err(failed)?;
            whole.update(&data);
            offset += data.len() as u64;
        }
        file.flush().await.map_err(failed)?;
        Ok(data_encoding::HEXLOWER.encode(&whole.finalize()))
    }
}

/// The timer of the puller: one pass after `FIRST_PULL`, then one every `PULL_INTERVAL`,
/// until the backend stops.
pub async fn run(puller: Puller) {
    let mut stop = puller.stop.clone();
    let mut delay = FIRST_PULL;
    loop {
        tokio::select! {
            _ = stop.wait_for(|stopped| *stopped) => return,
            () = tokio::time::sleep(delay) => {}
        }
        if let Err(reason) = puller.pull().await {
            eprintln!("peer update pass failed: {reason}");
        }
        delay = PULL_INTERVAL;
    }
}

/// Extract the archive into a private folder, verify the app and move it to its build
/// folder. Runs on a blocking thread, because it starts codesign and ditto.
fn stage_app(
    partial: &Path,
    updates: &Path,
    build: &Value,
    installed: &Path,
    codesign: &Path,
    ditto: &Path,
) -> Result<PathBuf, Step> {
    let folder = updates.join(build_name(build));
    remove_tree(&folder);
    let extraction = tempfile::Builder::new()
        .prefix(EXTRACT_PREFIX)
        .tempdir_in(updates)
        .map_err(failed)?;
    let staged = extraction.path().join(APP_NAME);
    let result = extract_and_verify(
        partial,
        extraction.path(),
        &staged,
        build,
        installed,
        codesign,
        ditto,
    )
    .and_then(|()| {
        // Another program can write into the build folder while the archive extracts, so
        // the folder is replaced after the check.
        remove_tree(&folder);
        std::fs::create_dir_all(&folder).map_err(failed)?;
        std::fs::rename(&staged, folder.join(APP_NAME)).map_err(failed)?;
        Ok(folder.join(APP_NAME))
    });
    if result.is_err() {
        remove_tree(&folder);
    }
    result
}

fn extract_and_verify(
    partial: &Path,
    extraction: &Path,
    staged: &Path,
    build: &Value,
    installed: &Path,
    codesign: &Path,
    ditto: &Path,
) -> Result<(), Step> {
    let archive = partial.to_string_lossy().into_owned();
    let folder = extraction.to_string_lossy().into_owned();
    peer_updates::tool(ditto, &["-x", "-k", &archive, &folder], DITTO_TIMEOUT)
        .map_err(|error| Step::Failed(format!("ditto could not extract the update: {error}")))?;
    let mut entries = std::fs::read_dir(extraction)
        .map_err(failed)?
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(failed)?;
    entries.sort();
    if entries != [APP_NAME] {
        let names = entries
            .iter()
            .map(|name| format!("'{name}'"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Step::Failed(
            format!("The update archive does not hold only the app: {names}")
                .chars()
                .take(REASON_CHARS)
                .collect(),
        ));
    }
    verify_app(staged, installed, build, codesign)
}

/// Accept only a complete app signed by the certificate of the installed app, with the
/// offered build stamp.
fn verify_app(app: &Path, installed: &Path, build: &Value, codesign: &Path) -> Result<(), Step> {
    let missing = || Step::Failed("The staged app is missing".to_owned());
    let metadata = std::fs::symlink_metadata(app).map_err(|_| missing())?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(missing());
    }
    let staged = app.to_string_lossy().into_owned();
    peer_updates::tool(
        codesign,
        &["--verify", "--deep", "--strict", &staged],
        CODESIGN_TIMEOUT,
    )
    .map_err(|error| Step::Failed(format!("The signature check failed: {error}")))?;
    if requirement(app, codesign)? != requirement(installed, codesign)? {
        return Err(Step::Failed(
            "Another certificate signed the build".to_owned(),
        ));
    }
    if peer_updates::read_build(app).as_ref() != Some(build) {
        return Err(Step::Failed(
            "The build stamp of the app differs from the offer".to_owned(),
        ));
    }
    Ok(())
}

/// The designated requirement of a signed app. It names the signing certificate.
fn requirement(app: &Path, codesign: &Path) -> Result<String, Step> {
    let name = app
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let target = app.to_string_lossy().into_owned();
    let text =
        peer_updates::tool(codesign, &["-d", "-r-", &target], CODESIGN_TIMEOUT).map_err(|_| {
            Step::Failed(format!(
                "The designated requirement of {name} is unavailable"
            ))
        })?;
    text.lines()
        .find_map(|line| line.strip_prefix("designated => "))
        .map(|value| value.trim().to_owned())
        .ok_or_else(|| {
            Step::Failed(format!(
                "The designated requirement of {name} is unavailable"
            ))
        })
}

/// The bytes of one chunk answer, when it is the chunk that the request asked for.
fn chunk_data(chunk: &Value, offset: u64, size: u64) -> Option<Vec<u8>> {
    let data = base64::engine::general_purpose::STANDARD
        .decode(chunk["data"].as_str()?)
        .ok()?;
    let next = offset.checked_add(data.len() as u64)?;
    let hash = data_encoding::HEXLOWER.encode(&Sha256::digest(&data));
    (!data.is_empty()
        && data.len() <= CHUNK_BYTES
        && chunk["offset"].as_u64() == Some(offset)
        && chunk["nextOffset"].as_u64() == Some(next)
        && next <= size
        && chunk["sha256"].as_str() == Some(hash.as_str()))
    .then_some(data)
}

/// The staged update, or None. Its app must be the staged copy in this state directory.
fn read_pending(root: &Path) -> Option<Value> {
    let value = read_json(&root.join(PENDING_FILE))?;
    let build = value.get("build").and_then(peer_updates::valid_build)?;
    let app = value["app"].as_str()?;
    let source = value["from"].as_str()?;
    let id = value["id"].as_str()?;
    (Path::new(app) == build_folder(root, &build).join(APP_NAME))
        .then(|| json!({"id": id, "app": app, "build": build, "from": source}))
}

/// The update state of `/api/desktop` (`peerUpdate`), read from the files that the puller and
/// the installer write. `installed` is the installed app of this process, when it runs from one.
pub fn status(root: &Path, installed: Option<&Path>) -> PeerUpdateStatus {
    let saved = read_json(&root.join(STATUS_FILE)).unwrap_or_else(|| json!({}));
    PeerUpdateStatus {
        build: installed
            .and_then(peer_updates::read_build)
            .and_then(|build| build_struct(&build)),
        checked_at: saved["checkedAt"].as_f64(),
        last_pull: event(&saved["lastPull"], "server"),
        pending: read_pending(root).and_then(|pending| {
            Some(PeerUpdatePending {
                build: build_struct(&pending["build"])?,
                server: pending["from"].as_str()?.to_owned(),
            })
        }),
        last_install: event(
            &read_json(&root.join(RECEIPT_FILE)).unwrap_or_default(),
            "from",
        ),
    }
}

/// One attempt record: a pull in the status file, or the installer receipt.
fn event(value: &Value, server_field: &str) -> Option<PeerUpdateEvent> {
    value.is_object().then(|| PeerUpdateEvent {
        at: value["at"].as_f64(),
        server: value[server_field].as_str().map(bounded),
        build: value
            .get("build")
            .and_then(peer_updates::valid_build)
            .and_then(|build| build_struct(&build)),
        outcome: value["outcome"].as_str().map(bounded),
        reason: value["reason"].as_str().map(bounded),
    })
}

fn build_struct(build: &Value) -> Option<PeerUpdateBuild> {
    Some(PeerUpdateBuild {
        version: u8::try_from(build["version"].as_u64()?).ok()?,
        built_at: build["builtAt"].as_f64()?,
        commit: build["commit"].as_str()?.to_owned(),
        commit_time: build["commitTime"].as_u64()?,
        branch: build["branch"].as_str()?.to_owned(),
    })
}

fn bounded(text: &str) -> String {
    text.chars().take(REASON_CHARS).collect()
}

/// The failure records that are still within the record age.
fn kept_failures(status: &Value, now: i64) -> Value {
    Value::Object(
        status["failures"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, record)| within(record["at"].as_f64(), now, FAILURE_RECORD_SECONDS))
            .map(|(key, record)| (key.clone(), record.clone()))
            .collect(),
    )
}

fn within(at: Option<f64>, now: i64, seconds: i64) -> bool {
    at.is_some_and(|at| (now as f64) - at < seconds as f64)
}

fn built_at(build: &Value) -> f64 {
    build["builtAt"].as_f64().unwrap_or(0.0)
}

/// The build stamp value as the installer names its folder (Python `str(builtAt)`).
fn build_name(build: &Value) -> String {
    build["builtAt"].to_string()
}

fn build_key(build: &Value) -> String {
    format!(
        "{}:{}",
        build_name(build),
        build["commit"].as_str().unwrap_or_default()
    )
}

fn build_folder(root: &Path, build: &Value) -> PathBuf {
    root.join(UPDATES_DIR).join(build_name(build))
}

fn pending_id(server: &str, build: &Value) -> String {
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("studio-peer-update:{server}:{}", build_key(build)).as_bytes(),
    )
    .hyphenated()
    .to_string()
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?)
        .ok()
        .filter(Value::is_object)
}

fn write_json(path: &Path, value: &Value) -> Result<(), Step> {
    atomic_file::replace(path, value.to_string().as_bytes(), Some(0o600), true).map_err(failed)
}

/// Remove a file, or a folder with its contents. A symbolic link is removed as a file.
fn remove_tree(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => drop(std::fs::remove_dir_all(path)),
        Ok(_) => drop(std::fs::remove_file(path)),
        Err(_) => {}
    }
}

/// Remove the private extraction folders that an interrupted pull left.
fn remove_extractions(updates: &Path) {
    let Ok(entries) = std::fs::read_dir(updates) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(EXTRACT_PREFIX)
        {
            remove_tree(&entry.path());
        }
    }
}

fn unix_time() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use futures_util::future::BoxFuture;
    use serde_json::{Value, json};
    use studio_runtime_store::RuntimeStore;
    use tokio::sync::watch;

    use super::*;
    use crate::multi_server_auth::AccessError;
    use crate::multi_server_orchestration::{
        ExchangeOutcome, HttpsPairedTransport, OrchestrationDriver, PairedTransport,
        RuntimeOrchestrationEffects,
    };
    use crate::peer_updates::{CHUNK_BYTES, Offer, fixture};

    const NOW: i64 = 1_800_000_000;

    fn stamp(built: f64, commit: char) -> Value {
        json!({
            "version": 1,
            "builtAt": built,
            "commit": commit.to_string().repeat(40),
            "commitTime": 1791979000,
            "branch": "main",
        })
    }

    fn new_build() -> Value {
        stamp(1791980000.25, 'b')
    }

    fn old_build() -> Value {
        stamp(1791900000.5, 'a')
    }

    /// The paired server as an in-process link: each request runs on the orchestration
    /// receive path of the server, as the signed channel runs it.
    struct Link {
        peer: Arc<OrchestrationDriver>,
        actions: Mutex<Vec<String>>,
        /// Chunk answers that carry a wrong hash before the rest are right.
        corrupt_chunks: AtomicUsize,
        /// The reason that an older server gives for update.offer.
        offer_refusal: Mutex<Option<&'static str>>,
    }

    impl Link {
        fn count(&self, action: &str) -> usize {
            self.actions
                .lock()
                .unwrap()
                .iter()
                .filter(|name| name.as_str() == action)
                .count()
        }
    }

    impl PairedTransport for Link {
        fn exchange<'a>(
            &'a self,
            _server: &'a str,
            envelope: &'a Value,
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<Value, AccessError>> {
            Box::pin(async move {
                let action = envelope["action"].as_str().unwrap_or_default().to_owned();
                let key = envelope["requestId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                self.actions.lock().unwrap().push(action.clone());
                let refusal = *self.offer_refusal.lock().unwrap();
                if let (true, Some(reason)) = (action == "update.offer", refusal) {
                    return Ok(ExchangeOutcome::NotApplied(reason.to_owned()).to_value(&key));
                }
                let mut receipt = self
                    .peer
                    .receive("mac", envelope, NOW)
                    .await?
                    .to_value(&key);
                if action == "update.chunk"
                    && receipt["outcome"] == "applied"
                    && self.corrupt_chunks.load(Ordering::SeqCst) > 0
                {
                    self.corrupt_chunks.fetch_sub(1, Ordering::SeqCst);
                    receipt["value"]["sha256"] = json!("0".repeat(64));
                }
                Ok(receipt)
            })
        }
    }

    const CODESIGN_BODY: &str = r#"case "$1" in
  --verify) if [ -e "$4/Contents/BAD_SIGNATURE" ]; then echo 'a sealed resource is modified' >&2; exit 1; fi ;;
  -d) echo "designated => $(cat "$3/Contents/CERTIFICATE")" >&2 ;;
esac
exit 0"#;

    /// The peer's ditto: `extra` runs before the archive is written.
    fn archive_body(extra: &str) -> String {
        format!(
            "case \"$1\" in\n  -c) {extra}tar -cf \"$5\" -C \"$(dirname \"$4\")\" \"$(basename \"$4\")\" ;;\nesac\nexit 0"
        )
    }

    /// The puller's ditto: `extra` runs after the archive is extracted.
    fn extract_body(extra: &str) -> String {
        format!("case \"$1\" in\n  -x) tar -xf \"$3\" -C \"$4\"; {extra}: ;;\nesac\nexit 0")
    }

    /// The app and the archive of one pass are written to this folder.
    struct Fixture {
        mac: Arc<RuntimeStore>,
        peer: Arc<RuntimeStore>,
        peer_app: PathBuf,
        tools: PathBuf,
        link: Arc<Link>,
        clock: Arc<AtomicI64>,
        puller: Puller,
        _stop: watch::Sender<bool>,
        _dirs: Vec<tempfile::TempDir>,
    }

    impl Fixture {
        fn state(&self) -> PathBuf {
            self.mac.root().to_path_buf()
        }
    }

    async fn open_store(folder: &Path) -> Arc<RuntimeStore> {
        Arc::new(RuntimeStore::open(Some(folder)).await.unwrap())
    }

    async fn pair(store: &Arc<RuntimeStore>, id: &'static str) {
        let record = json!({
            "id": id, "clientId": id, "kind": "server", "label": id, "created": 1,
            "publicKey": "public-key", "origin": "https://peer.invalid", "status": "paired",
        });
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id,record) VALUES (?1,?2)",
                    rusqlite::params![id, record.to_string()],
                )?;
                Ok::<(), studio_runtime_store::StoreError>(())
            })
            .await
            .unwrap();
    }

    fn make_app(folder: &Path, build: &Value, certificate: &str) -> PathBuf {
        let app = folder.join(APP_NAME);
        let resources = app.join("Contents/Resources");
        std::fs::create_dir_all(&resources).unwrap();
        std::fs::write(resources.join("studio-build.json"), build.to_string()).unwrap();
        std::fs::write(app.join("Contents/CERTIFICATE"), certificate).unwrap();
        // Incompressible bytes, so that the archive needs several chunks.
        let payload: Vec<u8> = (0..2 * CHUNK_BYTES + 1000)
            .map(|index| (index.wrapping_mul(2_654_435_761) >> 7) as u8)
            .collect();
        std::fs::write(resources.join("payload.bin"), payload).unwrap();
        app
    }

    async fn fixture_with(archive_extra: &str) -> Fixture {
        let mac_dir = tempfile::tempdir().unwrap();
        let peer_dir = tempfile::tempdir().unwrap();
        let apps = tempfile::tempdir().unwrap();
        let mac = open_store(mac_dir.path()).await;
        let peer = open_store(peer_dir.path()).await;
        pair(&mac, "server").await;
        pair(&peer, "mac").await;
        let tools = tempfile::tempdir().unwrap();
        let codesign = fixture::script(tools.path(), "codesign", CODESIGN_BODY);
        let archive = fixture::script(tools.path(), "ditto-archive", &archive_body(archive_extra));
        let extract = fixture::script(tools.path(), "ditto-extract", &extract_body(""));
        let peer_app = make_app(
            &apps.path().join("server"),
            &new_build(),
            "Studio Local Signing",
        );
        let local_app = make_app(
            &apps.path().join("mac"),
            &old_build(),
            "Studio Local Signing",
        );
        let offer = Offer::fixture(peer_app.clone(), codesign.clone(), archive);
        let peer_driver = Arc::new(OrchestrationDriver::new(
            Arc::clone(&peer),
            Arc::new(HttpsPairedTransport::new(Arc::clone(&peer))),
            Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&peer)).with_update_offer(offer)),
        ));
        let link = Arc::new(Link {
            peer: peer_driver,
            actions: Mutex::new(Vec::new()),
            corrupt_chunks: AtomicUsize::new(0),
            offer_refusal: Mutex::new(None),
        });
        let clock = Arc::new(AtomicI64::new(NOW));
        let (stop, stop_rx) = watch::channel(false);
        let clock_for_puller = Arc::clone(&clock);
        let puller = Puller::new(
            Arc::clone(&mac),
            Arc::clone(&link) as Arc<dyn PairedTransport>,
            local_app,
            codesign,
            extract,
            Arc::new(move || clock_for_puller.load(Ordering::SeqCst)),
            stop_rx,
        );
        Fixture {
            mac,
            peer,
            peer_app,
            tools: tools.path().to_path_buf(),
            link,
            clock,
            puller,
            _stop: stop,
            _dirs: vec![mac_dir, peer_dir, apps, tools],
        }
    }

    async fn fixture() -> Fixture {
        fixture_with("").await
    }

    fn read_names(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(folder)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    fn write_build(app: &Path, build: &Value) {
        std::fs::write(
            app.join("Contents/Resources/studio-build.json"),
            build.to_string(),
        )
        .unwrap();
    }

    fn status_of(f: &Fixture) -> Value {
        read_json(&f.state().join(STATUS_FILE)).unwrap()
    }

    fn peer_exports(f: &Fixture) -> Vec<String> {
        read_names(&f.peer.root().join("server-exports"))
    }

    #[tokio::test]
    async fn newer_offer_is_pulled_verified_and_staged() {
        let f = fixture().await;
        assert_eq!(
            f.puller.pull().await.unwrap(),
            Outcome::Staged {
                build: new_build(),
                server: "server".to_owned()
            }
        );
        let pending = read_json(&f.state().join(PENDING_FILE)).unwrap();
        assert_eq!(pending["build"], new_build());
        assert_eq!(pending["from"], "server");
        assert_eq!(pending["id"], pending_id("server", &new_build()));
        let staged = PathBuf::from(pending["app"].as_str().unwrap());
        assert_eq!(
            staged,
            f.state()
                .join("updates")
                .join("1791980000.25")
                .join(APP_NAME)
        );
        assert_eq!(
            std::fs::read(staged.join("Contents/Resources/payload.bin")).unwrap(),
            std::fs::read(f.peer_app.join("Contents/Resources/payload.bin")).unwrap()
        );
        assert!(f.link.count("update.chunk") >= 3);
        assert_eq!(f.link.count("update.release"), 1);
        assert!(peer_exports(&f).is_empty(), "the export is released");
        assert_eq!(read_names(&f.state().join("updates")), ["1791980000.25"]);
        assert_eq!(status_of(&f)["lastPull"]["outcome"], "staged");
        // The next pass finds the staged build and pulls nothing again.
        assert_eq!(f.puller.pull().await.unwrap(), Outcome::Idle);
        assert_eq!(f.link.count("update.export"), 1);
    }

    #[tokio::test]
    async fn a_file_that_finder_writes_into_the_build_folder_does_not_fail_the_pull() {
        let mut f = fixture().await;
        let folder = f.state().join("updates").join("1791980000.25");
        f.puller.ditto = fixture::script(
            &f.tools,
            "ditto-finder",
            &extract_body(
                "mkdir -p \"$(dirname \"$4\")/1791980000.25\"; \
                 printf Bud1 > \"$(dirname \"$4\")/1791980000.25/.DS_Store\"; ",
            ),
        );
        assert!(matches!(
            f.puller.pull().await.unwrap(),
            Outcome::Staged { .. }
        ));
        let pending = read_json(&f.state().join(PENDING_FILE)).unwrap();
        assert_eq!(
            pending["app"],
            folder.join(APP_NAME).to_string_lossy().as_ref()
        );
        assert_eq!(read_names(&folder), [APP_NAME]);
    }

    #[tokio::test]
    async fn an_archive_with_an_entry_beside_the_app_is_refused() {
        let mut f = fixture().await;
        f.puller.ditto = fixture::script(
            &f.tools,
            "ditto-extra-entry",
            &extract_body("mkdir \"$4/__MACOSX\"; "),
        );
        assert_eq!(
            f.puller.pull().await.unwrap(),
            Outcome::Failed {
                build: new_build(),
                reason:
                    "The update archive does not hold only the app: 'Codex Studio.app', '__MACOSX'"
                        .to_owned(),
            }
        );
        assert!(read_pending(&f.state()).is_none());
        assert!(read_names(&f.state().join("updates")).is_empty());
    }

    #[tokio::test]
    async fn same_or_older_build_is_not_pulled() {
        let f = fixture().await;
        for build in [old_build(), stamp(1791899999.5, 'c')] {
            write_build(&f.peer_app, &build);
            assert_eq!(f.puller.pull().await.unwrap(), Outcome::Idle);
        }
        assert_eq!(f.link.count("update.offer"), 2);
        assert_eq!(f.link.count("update.export"), 0);
        assert!(read_pending(&f.state()).is_none());
    }

    #[tokio::test]
    async fn disabled_setting_asks_no_server() {
        let f = fixture().await;
        f.mac
            .transaction(|tx| {
                tx.execute(
                    "CREATE TABLE runtime_peer_update_settings(id INTEGER PRIMARY KEY CHECK(id=1), auto_update INTEGER NOT NULL)",
                    [],
                )?;
                tx.execute("INSERT INTO runtime_peer_update_settings VALUES(1,0)", [])?;
                Ok::<(), studio_runtime_store::StoreError>(())
            })
            .await
            .unwrap();
        assert_eq!(f.puller.pull().await.unwrap(), Outcome::Disabled);
        assert!(f.link.actions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn bad_chunk_hash_is_recorded_and_skipped_for_an_hour() {
        let f = fixture().await;
        f.link.corrupt_chunks.store(1, Ordering::SeqCst);
        assert_eq!(
            f.puller.pull().await.unwrap(),
            Outcome::Failed {
                build: new_build(),
                reason: "Invalid update archive chunk".to_owned(),
            }
        );
        assert!(read_pending(&f.state()).is_none());
        assert!(read_names(&f.state().join("updates")).is_empty());
        let status = status_of(&f);
        let key = format!("1791980000.25:{}", "b".repeat(40));
        assert!(
            status["failures"][&key]["at"].is_number(),
            "keyed by builtAt and commit"
        );
        assert_eq!(status["lastPull"]["outcome"], "failed");
        assert_eq!(f.link.count("update.release"), 1);
        f.clock
            .fetch_add(FAILURE_RETRY_SECONDS - 1, Ordering::SeqCst);
        assert_eq!(f.puller.pull().await.unwrap(), Outcome::Idle);
        assert_eq!(f.link.count("update.export"), 1);
        f.clock.fetch_add(2, Ordering::SeqCst);
        assert!(matches!(
            f.puller.pull().await.unwrap(),
            Outcome::Staged { .. }
        ));
        assert_eq!(f.link.count("update.export"), 2);
    }

    #[tokio::test]
    async fn failed_install_receipt_skips_the_build_for_an_hour() {
        let f = fixture().await;
        std::fs::write(
            f.state().join(RECEIPT_FILE),
            json!({"build": new_build(), "outcome": "failed", "at": NOW, "from": "server"})
                .to_string(),
        )
        .unwrap();
        assert_eq!(f.puller.pull().await.unwrap(), Outcome::Idle);
        assert_eq!(f.link.count("update.export"), 0);
    }

    #[tokio::test]
    async fn other_certificate_is_refused() {
        let f = fixture().await;
        std::fs::write(f.peer_app.join("Contents/CERTIFICATE"), "Another Signing").unwrap();
        assert_eq!(
            f.puller.pull().await.unwrap(),
            Outcome::Failed {
                build: new_build(),
                reason: "Another certificate signed the build".to_owned(),
            }
        );
        assert!(read_pending(&f.state()).is_none());
        assert!(!f.state().join("updates").join("1791980000.25").exists());
    }

    #[tokio::test]
    async fn broken_signature_is_refused() {
        // The archive gets a file that breaks the seal of the app.
        let f = fixture_with("touch \"$4/Contents/BAD_SIGNATURE\"; ").await;
        let Outcome::Failed { reason, .. } = f.puller.pull().await.unwrap() else {
            panic!("the pull fails");
        };
        assert!(reason.starts_with("The signature check failed"), "{reason}");
        assert!(read_pending(&f.state()).is_none());
    }

    #[tokio::test]
    async fn older_peer_without_update_actions_offers_nothing() {
        let f = fixture().await;
        *f.link.offer_refusal.lock().unwrap() = Some("Unknown cross-server operation");
        assert_eq!(f.puller.pull().await.unwrap(), Outcome::Idle);
        assert_eq!(f.link.count("update.offer"), 1);
        assert_eq!(f.link.count("update.export"), 0);
        assert!(read_pending(&f.state()).is_none());
        assert_eq!(status_of(&f)["failures"], json!({}));
    }

    #[tokio::test]
    async fn older_peer_that_asks_for_a_link_offers_nothing() {
        let f = fixture().await;
        *f.link.offer_refusal.lock().unwrap() = Some("Supply a remote-parent link");
        assert_eq!(f.puller.pull().await.unwrap(), Outcome::Idle);
        assert!(read_pending(&f.state()).is_none());
    }

    #[tokio::test]
    async fn stale_pending_update_is_removed() {
        let f = fixture().await;
        write_build(&f.peer_app, &old_build());
        let folder = f.state().join("updates").join("1791900000.5");
        std::fs::create_dir_all(folder.join(APP_NAME)).unwrap();
        std::fs::write(
            f.state().join(PENDING_FILE),
            json!({"id": "x", "app": folder.join(APP_NAME).to_string_lossy(),
                   "build": old_build(), "from": "server"})
            .to_string(),
        )
        .unwrap();
        assert_eq!(f.puller.pull().await.unwrap(), Outcome::Idle);
        assert!(!f.state().join(PENDING_FILE).exists());
        assert!(!folder.exists());
    }

    #[tokio::test]
    async fn pending_file_outside_the_update_folder_is_ignored() {
        let f = fixture().await;
        std::fs::write(
            f.state().join(PENDING_FILE),
            json!({"id": "x", "app": f.peer_app.to_string_lossy(),
                   "build": new_build(), "from": "server"})
            .to_string(),
        )
        .unwrap();
        assert!(read_pending(&f.state()).is_none());
    }

    #[tokio::test]
    async fn stopped_pull_records_no_failure() {
        let f = fixture().await;
        f._stop.send_replace(true);
        assert_eq!(f.puller.pull().await.unwrap(), Outcome::Stopped);
        assert!(read_pending(&f.state()).is_none());
        assert!(read_names(&f.state().join("updates")).is_empty());
    }

    #[test]
    fn status_reads_the_files_of_the_last_pass() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let installed = make_app(&root.join("apps"), &old_build(), "Studio Local Signing");
        std::fs::write(
            root.join(STATUS_FILE),
            json!({
                "checkedAt": NOW,
                "failures": {},
                "lastPull": {"at": NOW, "server": "server", "build": new_build(),
                             "outcome": "failed", "reason": "Invalid update archive chunk"},
            })
            .to_string(),
        )
        .unwrap();
        let staged = root.join(UPDATES_DIR).join("1791980000.25").join(APP_NAME);
        std::fs::write(
            root.join(PENDING_FILE),
            json!({"id": "x", "app": staged.to_string_lossy(), "build": new_build(), "from": "server"})
                .to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join(RECEIPT_FILE),
            json!({"id": "r", "build": old_build(), "outcome": "installed", "at": NOW - 100,
                   "from": "server", "backend": "rust", "window": null})
            .to_string(),
        )
        .unwrap();
        let value = serde_json::from_str::<Value>(
            &serde_json::to_string(&status(root, Some(installed.as_path()))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            value,
            json!({
                "build": old_build(),
                "checkedAt": NOW as f64,
                "lastPull": {"at": NOW as f64, "server": "server", "build": new_build(),
                             "outcome": "failed", "reason": "Invalid update archive chunk"},
                "pending": {"build": new_build(), "server": "server"},
                "lastInstall": {"at": (NOW - 100) as f64, "server": "server",
                                "build": old_build(), "outcome": "installed", "reason": null},
            })
        );
        // Without any file, the status has no build, pass or update.
        let empty = serde_json::from_str::<Value>(
            &serde_json::to_string(&status(&root.join("empty"), None)).unwrap(),
        )
        .unwrap();
        assert_eq!(
            empty,
            json!({"build": null, "checkedAt": null, "lastPull": null,
                   "pending": null, "lastInstall": null})
        );
    }

    #[test]
    fn installer_file_names_match_desktop_updates() {
        const INSTALLER: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../client/apps/desktop/desktop_updates.py"
        ));
        for line in [
            format!("APP_NAME = '{APP_NAME}'"),
            format!("PENDING_FILE = '{PENDING_FILE}'"),
            format!("RECEIPT_FILE = '{RECEIPT_FILE}'"),
            format!("UPDATES_DIR = '{UPDATES_DIR}'"),
        ] {
            assert!(
                INSTALLER.lines().any(|actual| actual == line),
                "desktop_updates.py must consume the owner: {line}"
            );
        }
    }
}
