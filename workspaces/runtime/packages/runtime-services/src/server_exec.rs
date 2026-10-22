//! Commands that a lead runs on this server through `orchestration_servers`.
//!
//! Python `codex_server_exec.py` (`ServerExec`, `Buffer`, `MarkerRedactor`,
//! `child`) owns these rules and the stored formats: the `runtime_server_exec`
//! record, its audit row, the input claims and the `<sha256(handle)>.output.json`
//! snapshot under `server-command-output`. One durable claim and a `.started`
//! fence precede the process start, so a command never runs twice.
//!
//! Python runs each command under its process supervisor, so a command keeps
//! running across a backend restart. Here the backend owns the process. After
//! a restart a command without a final snapshot is stopped through its
//! private marker and its outcome stays unknown; it is never run again.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_store::{RuntimeStore, StoreError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Largest `output_limit` of one command, in bytes (Python `MAX_OUTPUT`).
pub const MAX_OUTPUT: usize = 4 * 1024 * 1024;
/// Bytes of one `exec_read` page for both streams together (Python `READ_OUTPUT`).
const READ_OUTPUT: usize = 64 * 1024;
/// Seconds of a command that `exec` waits for before it returns a handle
/// (Python `SYNC_SECONDS`).
const SYNC_SECONDS: u64 = 5;
/// Maximum synchronous completion wait, including process startup and pipe drain.
const SYNC_WAIT_TIMEOUT: Duration = Duration::from_secs(8);
/// Commands in progress on one server (Python `start`).
const MAX_ACTIVE: usize = 32;
/// Input requests of one command (Python `control`).
const MAX_INPUTS: i64 = 32;
/// Bytes of one input request (Python `control`).
const MAX_INPUT_BYTES: usize = 64 * 1024;
/// Seconds that command output stays readable (Python `start`).
const OUTPUT_SECONDS: f64 = 7.0 * 86400.0;
/// Seconds that a finished audit row stays (Python `prune`).
const AUDIT_SECONDS: f64 = 90.0 * 86400.0;
/// Minimum time between changed snapshots of a running command (Python `child`).
const SNAPSHOT_PUBLISH_INTERVAL: Duration = Duration::from_millis(500);
/// Seconds that pipes may drain after the command stops (Python `child`).
const DRAIN_SECONDS: f64 = 2.0;
/// Folder under the state directory that holds command snapshots.
pub const OUTPUT_FOLDER: &str = "server-command-output";
/// Environment name that marks every process of one command (Python `child`).
const MARKER_NAME: &str = "STUDIO_EXEC_ID";
const UNKNOWN_AFTER_RESTART: &str =
    "Command outcome unknown; inspect the target server. The command was not run again.";

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// Python `ValueError` or `PermissionError`: a not-applied receipt.
    #[error("{0}")]
    Refused(String),
    /// A process, file or store failure: the outcome stays unknown.
    #[error("{0}")]
    Failed(String),
}

impl From<StoreError> for ExecError {
    fn from(error: StoreError) -> Self {
        Self::Failed(error.to_string())
    }
}

impl From<rusqlite::Error> for ExecError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

fn refused<T>(message: impl Into<String>) -> Result<T, ExecError> {
    Err(ExecError::Refused(message.into()))
}

fn now() -> f64 {
    crate::event_write::unix_time().unwrap_or_default()
}

/// Python `job_path`.
fn job_path(folder: &Path, key: &str, suffix: &str) -> PathBuf {
    folder.join(format!(
        "{}{suffix}",
        data_encoding::HEXLOWER.encode(&Sha256::digest(key.as_bytes()))
    ))
}

/// The sole JSON snapshot/config writer: build, serialize and fsync on the pool.
async fn atomic_json(
    path: PathBuf,
    value: impl FnOnce() -> Value + Send + 'static,
) -> Result<(), ExecError> {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/server_exec/json",
        move || {
            crate::atomic_file::replace(&path, value().to_string().as_bytes(), Some(0o600), true)
                .map_err(|error| ExecError::Failed(error.to_string()))
        },
    )
    .await
    .map_err(|error| ExecError::Failed(error.to_string()))?
}

/// Python `Buffer`: the first and last bytes of one output stream.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Buffer {
    limit: usize,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}

impl Buffer {
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    pub fn append(&mut self, data: &[u8]) {
        self.total += data.len();
        let head_size = self.limit.div_ceil(2);
        let take = data.len().min(head_size.saturating_sub(self.head.len()));
        self.head.extend_from_slice(&data[..take]);
        let tail_size = self.limit - head_size;
        let incoming = &data[take..];
        if incoming.len() >= tail_size {
            self.tail.clear();
            self.tail.extend(&incoming[incoming.len() - tail_size..]);
        } else {
            let excess = (self.tail.len() + incoming.len()).saturating_sub(tail_size);
            self.tail.drain(..excess);
            self.tail.extend(incoming);
        }
    }

    fn snapshot(&self) -> Value {
        let (first, second) = self.tail.as_slices();
        let mut tail = String::with_capacity(data_encoding::HEXLOWER.encode_len(self.tail.len()));
        data_encoding::HEXLOWER.encode_append(first, &mut tail);
        data_encoding::HEXLOWER.encode_append(second, &mut tail);
        json!({
            "limit": self.limit,
            "head": data_encoding::HEXLOWER.encode(&self.head),
            "tail": tail,
            "total": self.total,
        })
    }

    fn from_snapshot(saved: Option<&Value>) -> Self {
        let field = |name: &str| saved.and_then(|saved| saved.get(name));
        let bytes = |name: &str| {
            field(name)
                .and_then(Value::as_str)
                .and_then(|hex| {
                    data_encoding::HEXLOWER_PERMISSIVE
                        .decode(hex.as_bytes())
                        .ok()
                })
                .unwrap_or_default()
        };
        Self {
            limit: field("limit").and_then(Value::as_u64).unwrap_or(1) as usize,
            head: bytes("head"),
            tail: bytes("tail").into(),
            total: field("total").and_then(Value::as_u64).unwrap_or(0) as usize,
        }
    }

    /// Python `Buffer.read`: one UTF-8 page from `offset`. A retained tail can
    /// start inside a character that was discarded; that gap is reported.
    pub fn read(&self, offset: usize, limit: usize, last: bool) -> Value {
        let (mut start, mut data, mut gap): (usize, Vec<u8>, usize) = if self.total <= self.limit {
            let data = self
                .head
                .iter()
                .chain(self.tail.iter())
                .skip(offset)
                .take(limit)
                .copied()
                .collect();
            (offset, data, 0)
        } else if offset < self.head.len() {
            let end = self.head.len().min(offset + limit);
            (offset, self.head[offset..end].to_vec(), 0)
        } else {
            let tail_start = self.total - self.tail.len();
            let start = offset.max(tail_start);
            let from = start - tail_start;
            let data = self.tail.iter().skip(from).take(limit).copied().collect();
            (start, data, start - offset)
        };
        if gap > 0 {
            while data.first().is_some_and(|byte| byte & 0xc0 == 0x80) {
                data.remove(0);
                start += 1;
                gap += 1;
            }
        }
        let (text, pending) = decode_utf8(&data, last && start + data.len() == self.total);
        if pending > 0 {
            data.truncate(data.len() - pending);
            if data.is_empty() && self.total > self.limit && offset < self.head.len() {
                let mut page = self.read(self.head.len(), limit, last);
                let page_offset = page["offset"].as_u64().unwrap_or_default() as usize;
                page["gapBytes"] = json!(page_offset - offset);
                return page;
            }
        }
        json!({
            "offset": start,
            "nextOffset": start + data.len(),
            "gapBytes": gap,
            "text": text,
            "totalBytes": self.total,
            "truncated": self.total > self.limit,
        })
    }
}

/// Python's incremental UTF-8 decoder with `errors='replace'`: the text and,
/// unless `last`, the length of an incomplete character at the end.
fn decode_utf8(data: &[u8], last: bool) -> (String, usize) {
    let mut text = String::new();
    let mut rest = data;
    loop {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                text.push_str(valid);
                return (text, 0);
            }
            Err(error) => {
                let (valid, after) = rest.split_at(error.valid_up_to());
                text.push_str(std::str::from_utf8(valid).unwrap_or_default());
                match error.error_len() {
                    Some(length) => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        rest = &after[length..];
                    }
                    None if last => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        return (text, 0);
                    }
                    None => return (text, after.len()),
                }
            }
        }
    }
}

/// Python `MarkerRedactor`: never publish the private marker, even across
/// output chunks.
pub struct MarkerRedactor {
    secret: Vec<u8>,
    pending: Vec<u8>,
}

impl MarkerRedactor {
    pub fn new(secret: &str) -> Self {
        Self {
            secret: secret.as_bytes().to_vec(),
            pending: Vec::new(),
        }
    }

    pub fn feed(&mut self, data: &[u8], last: bool) -> Vec<u8> {
        let mut value = std::mem::take(&mut self.pending);
        value.extend_from_slice(data);
        let mut size = 0;
        if !last {
            for length in 1..=value.len().min(self.secret.len().saturating_sub(1)) {
                if value.ends_with(&self.secret[..length]) {
                    size = length;
                }
            }
        }
        self.pending = value[value.len() - size..].to_vec();
        value.truncate(value.len() - size);
        replace_bytes(&value, &self.secret, b"[private marker]")
    }
}

fn replace_bytes(value: &[u8], pattern: &[u8], replacement: &[u8]) -> Vec<u8> {
    if pattern.is_empty() {
        return value.to_vec();
    }
    let mut out = Vec::with_capacity(value.len());
    let mut index = 0;
    while index < value.len() {
        if value[index..].starts_with(pattern) {
            out.extend_from_slice(replacement);
            index += pattern.len();
        } else {
            out.push(value[index]);
            index += 1;
        }
    }
    out
}

/// Where a finished command's exit notice goes (Python `exec_completed`).
pub trait ExecEvents: Send + Sync {
    fn completed<'a>(
        &'a self,
        principal: &'a str,
        actor: &'a str,
        record: &'a Value,
    ) -> futures_util::future::BoxFuture<'a, ()>;
}

enum Control {
    Input { data: Vec<u8>, close: bool },
    Cancel,
}

/// The server's command runner. One instance serves each state directory.
pub struct ServerExec {
    store: Arc<RuntimeStore>,
    folder: PathBuf,
    events: Arc<dyn ExecEvents>,
    active: Mutex<HashMap<String, tokio::sync::mpsc::Sender<Control>>>,
    /// Every caller waits for the one reconciliation of the previous backend's commands.
    restored: tokio::sync::OnceCell<()>,
    /// The last prune of expired commands (Python prunes hourly).
    pruned: Mutex<Option<Instant>>,
    #[cfg(test)]
    snapshot_reads: std::sync::atomic::AtomicUsize,
}

impl ServerExec {
    /// The runner of this store's state directory. The first call reconciles
    /// commands that a previous backend left without a final record.
    pub async fn for_store(
        store: &Arc<RuntimeStore>,
        events: Arc<dyn ExecEvents>,
    ) -> Result<Arc<Self>, ExecError> {
        static RUNNERS: OnceLock<Mutex<HashMap<PathBuf, Arc<ServerExec>>>> = OnceLock::new();
        let key = store.root().to_path_buf();
        let runners = RUNNERS.get_or_init(|| Mutex::new(HashMap::new()));
        let existing = runners
            .lock()
            .map_err(|_| ExecError::Failed("The command runner is unavailable".into()))?
            .get(&key)
            .cloned();
        if let Some(runner) = existing {
            runner.restored.get_or_init(|| runner.restore()).await;
            return Ok(runner);
        }
        let folder = key.join(OUTPUT_FOLDER);
        create_private_folder(&folder)?;
        let runner = Arc::new(Self {
            store: Arc::clone(store),
            folder,
            events,
            active: Mutex::new(HashMap::new()),
            restored: tokio::sync::OnceCell::new(),
            pruned: Mutex::new(None),
            #[cfg(test)]
            snapshot_reads: std::sync::atomic::AtomicUsize::new(0),
        });
        let runner = {
            let mut runners = runners
                .lock()
                .map_err(|_| ExecError::Failed("The command runner is unavailable".into()))?;
            Arc::clone(runners.entry(key).or_insert(runner))
        };
        runner.restored.get_or_init(|| runner.restore()).await;
        Ok(runner)
    }

    fn snapshot(&self, key: &str) -> Option<Value> {
        #[cfg(test)]
        self.snapshot_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let raw = std::fs::read(job_path(&self.folder, key, ".output.json")).ok()?;
        serde_json::from_slice(&raw).ok()
    }

    /// Python `_record`: the record of a command that this principal and actor own.
    async fn record(&self, principal: &str, actor: &str, key: &str) -> Result<Value, ExecError> {
        let key_for_read = key.to_owned();
        let row: Option<(String, String, String)> = self
            .store
            .read_async(
                "runtime-services/server_exec",
                move |tx| -> Result<_, ExecError> {
                    use rusqlite::OptionalExtension;
                    Ok(tx
                        .query_row(
                            "SELECT owner,actor,record FROM runtime_server_exec WHERE id=?1",
                            [&key_for_read],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )
                        .optional()?)
                },
            )
            .await?;
        match row {
            Some((owner, saved_actor, record)) if owner == principal && saved_actor == actor => {
                serde_json::from_str(&record)
                    .map_err(|_| ExecError::Failed("The stored command record is invalid".into()))
            }
            _ => refused("The command handle belongs to another actor or server"),
        }
    }

    /// Python `_save`: a final record never returns to a running status, and
    /// each final record sends its exit notice once.
    async fn save(
        &self,
        principal: &str,
        actor: &str,
        key: &str,
        record: &Value,
    ) -> Result<(), ExecError> {
        let running =
            |record: &Value| matches!(record["status"].as_str(), Some("starting" | "running"));
        let final_record = !running(record);
        let key_owned = key.to_owned();
        let record_owned = record.clone();
        let saved = self
            .store
            .transaction_with(move |tx| -> Result<bool, ExecError> {
                use rusqlite::OptionalExtension;
                let previous: Option<String> = tx
                    .query_row(
                        "SELECT record FROM runtime_server_exec WHERE id=?1",
                        [&key_owned],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(previous) = previous {
                    let previous: Value = serde_json::from_str(&previous).unwrap_or(Value::Null);
                    let was_final = !matches!(previous["status"].as_str(), Some("starting" | "running"));
                    if was_final && !final_record {
                        return Ok(false);
                    }
                }
                tx.execute(
                    "UPDATE runtime_server_exec SET record=?1 WHERE id=?2",
                    rusqlite::params![record_owned.to_string(), key_owned],
                )?;
                if final_record {
                    tx.execute(
                        "UPDATE runtime_server_exec_audit SET end=?1,exit_code=?2,signal=?3 WHERE id=?4",
                        rusqlite::params![
                            record_owned["finishedAt"].as_f64(),
                            record_owned["exitCode"].as_i64(),
                            record_owned["signal"].as_i64(),
                            key_owned
                        ],
                    )?;
                }
                Ok(true)
            })
            .await?;
        if saved && final_record {
            self.events.completed(principal, actor, record).await;
        }
        Ok(())
    }

    /// Python `_restore` for a backend that does not retain its commands: a
    /// final snapshot settles the record; otherwise the command's marked
    /// processes stop and the outcome stays unknown.
    async fn restore(&self) {
        let rows: Vec<(String, String, String, String)> = match self
            .store
            .read_async("runtime-services/server_exec", |tx| -> Result<_, ExecError> {
                let mut statement = tx.prepare(
                    "SELECT id,owner,actor,record FROM runtime_server_exec WHERE json_extract(record,'$.status') IN ('starting','running')",
                )?;
                let rows = statement
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                eprintln!("server command restore failed: {error}");
                return;
            }
        };
        for (key, owner, actor, raw) in rows {
            let final_snapshot = self.snapshot(&key).and_then(|snapshot| {
                let record = snapshot.get("record")?.clone();
                (!matches!(record["status"].as_str(), Some("starting" | "running")))
                    .then_some(record)
            });
            let record = match final_snapshot {
                Some(record) => record,
                None => {
                    let config = std::fs::read(job_path(&self.folder, &key, ".config.json"))
                        .ok()
                        .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok());
                    let marker = config
                        .as_ref()
                        .and_then(|config| config["markerSecret"].as_str())
                        .map(str::to_owned);
                    let stopped = marker.map(|marker| Scope {
                        marker,
                        running_root: None,
                        group: None,
                    });
                    let stopped = match stopped {
                        Some(scope) => stop_in_pool(scope).await.pids.len(),
                        None => 0,
                    };
                    let mut record: Value =
                        serde_json::from_str(&raw).unwrap_or_else(|_| json!({}));
                    if record["finishedAt"].is_null() {
                        record["finishedAt"] = json!(now());
                    }
                    record["status"] = json!("unknown");
                    record["error"] = json!(UNKNOWN_AFTER_RESTART);
                    if stopped > 0 {
                        record["stoppedDescendants"] = json!(stopped);
                    }
                    record
                }
            };
            if let Err(error) = self.save(&owner, &actor, &key, &record).await {
                eprintln!("server command {key} restore failed: {error}");
            }
        }
        self.prune().await;
    }

    /// Prune at most once an hour (Python `MultiServerService.prune`).
    async fn prune_hourly(&self) {
        let due = self.pruned.lock().is_ok_and(|mut pruned| {
            let due = pruned.is_none_or(|at| at.elapsed() >= Duration::from_secs(3600));
            if due {
                *pruned = Some(Instant::now());
            }
            due
        });
        if due {
            self.prune().await;
        }
    }

    /// Python `prune`: remove command files and rows after seven days.
    async fn prune(&self) {
        let cutoff = now() - OUTPUT_SECONDS;
        let active: Vec<String> = self
            .active
            .lock()
            .map(|active| active.keys().cloned().collect())
            .unwrap_or_default();
        let folder = self.folder.clone();
        let result = self
            .store
            .transaction_with(move |tx| -> Result<(), ExecError> {
                let mut statement = tx.prepare(
                    "SELECT id FROM runtime_server_exec WHERE json_extract(record,'$.startedAt')<?1",
                )?;
                let rows = statement
                    .query_map([cutoff], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(statement);
                for id in rows.into_iter().filter(|id| !active.contains(id)) {
                    for suffix in [".config.json", ".output.json", ".started"] {
                        drop(std::fs::remove_file(job_path(&folder, &id, suffix)));
                    }
                    tx.execute("DELETE FROM runtime_server_exec_input WHERE handle=?1", [&id])?;
                    tx.execute("DELETE FROM runtime_server_exec WHERE id=?1", [&id])?;
                }
                tx.execute(
                    "DELETE FROM runtime_server_exec_audit WHERE end<?1",
                    [now() - AUDIT_SECONDS],
                )?;
                Ok(())
            })
            .await;
        if let Err(error) = result {
            eprintln!("server command prune failed: {error}");
        }
    }

    /// Python `ServerExec.start`.
    pub async fn start(
        self: &Arc<Self>,
        principal: &str,
        local_server: &str,
        payload: &Value,
        key: &str,
    ) -> Result<Value, ExecError> {
        self.prune_hourly().await;
        let p =
            crate::multi_server_tools::validate_exec(payload, false).map_err(ExecError::Refused)?;
        let actor = p
            .get("actor")
            .and_then(Value::as_str)
            .filter(|actor| !actor.is_empty() && actor.chars().count() <= 200)
            .map(str::to_owned);
        let Some(actor) = actor else {
            return refused("Supply the lead actor identity");
        };
        let cwd = p["cwd"].as_str().unwrap_or_default().to_owned();
        if !Path::new(&cwd).is_dir() {
            return refused("The cwd is not an existing folder on this server");
        }
        let environment: Vec<(String, String)> = p["env"]
            .as_object()
            .map(|object| {
                object
                    .iter()
                    .map(|(key, value)| {
                        (key.clone(), value.as_str().unwrap_or_default().to_owned())
                    })
                    .collect()
            })
            .unwrap_or_default();
        let argv: Vec<String> = match &p["command"] {
            Value::String(command) => crate::shell_command::monitor_command(
                command,
                &json!({"shell_environment_policy": {"set": p["env"]}}),
                false,
            )
            .map_err(ExecError::Refused)?,
            Value::Array(argv) => argv
                .iter()
                .map(|value| value.as_str().unwrap_or_default().to_owned())
                .collect(),
            _ => return refused("Supply command argv of 1 to 256 strings, at most 128 KiB"),
        };
        let started_at = now();
        let timeout = p["timeout"].as_u64().unwrap_or(120);
        let output_limit = p["output_limit"].as_u64().unwrap_or(256 * 1024) as usize;
        let record = json!({
            "handle": key, "serverId": local_server, "status": "starting",
            "startedAt": started_at, "finishedAt": null, "exitCode": null, "signal": null,
            "duration": null, "timedOut": false, "outputLimit": output_limit,
            "outputExpiresAt": started_at + OUTPUT_SECONDS,
        });
        let marker = new_marker()?;
        let config = json!({"payload": p, "argv": argv, "record": record, "markerSecret": marker});
        let (sender, receiver) = tokio::sync::mpsc::channel(MAX_INPUTS as usize);
        {
            let mut active = self
                .active
                .lock()
                .map_err(|_| ExecError::Failed("The command runner is unavailable".into()))?;
            if active.len() >= MAX_ACTIVE {
                return refused("The server already has 32 command handles in progress");
            }
            if active.contains_key(key) {
                // Python's claim INSERT fails for a second start of one handle.
                return Err(ExecError::Failed(
                    "The command handle is already running".into(),
                ));
            }
            active.insert(key.to_owned(), sender);
        }
        // The claim precedes the process start and its `.started` fence.
        let argv_hash = data_encoding::HEXLOWER.encode(&Sha256::digest(
            studio_runtime_store::python_json_utf8(&json!(argv)).as_bytes(),
        ));
        let claim = {
            let (key, principal, actor, local_server, cwd) = (
                key.to_owned(),
                principal.to_owned(),
                actor.clone(),
                local_server.to_owned(),
                cwd.clone(),
            );
            let record = record.clone();
            self.store
                .transaction_with(move |tx| -> Result<(), ExecError> {
                    tx.execute(
                        "INSERT INTO runtime_server_exec VALUES(?1,?2,?3,?4)",
                        rusqlite::params![key, principal, actor, record.to_string()],
                    )?;
                    tx.execute(
                        "INSERT INTO runtime_server_exec_audit VALUES(?1,?2,?3,?4,?5,?6,?7,NULL,NULL,NULL)",
                        rusqlite::params![key, actor, principal, local_server, cwd, argv_hash, started_at],
                    )?;
                    Ok(())
                })
                .await
        };
        let written = match claim {
            Ok(()) => {
                let path = job_path(&self.folder, key, ".config.json");
                atomic_json(path, move || config).await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = written {
            if let Ok(mut active) = self.active.lock() {
                active.remove(key);
            }
            return Err(error);
        }
        let job = Job {
            runner: Arc::clone(self),
            principal: principal.to_owned(),
            actor: actor.clone(),
            key: key.to_owned(),
            argv,
            cwd,
            environment,
            marker,
            timeout,
            stdout: Buffer::new(output_limit.div_ceil(2)),
            stderr: Buffer::new(output_limit / 2),
            record: record.clone(),
            snapshot_dirty: true,
            last_publish: tokio::time::Instant::now(),
        };
        let completion = tokio::spawn(job.run(receiver));
        // A timed-out wait or dropped caller detaches the Job; its command timeout
        // and exec_cancel still stop the process.
        // Job::finish publishes and saves the final record before this wakes.
        if timeout <= SYNC_SECONDS
            && matches!(
                tokio::time::timeout(SYNC_WAIT_TIMEOUT, completion).await,
                Ok(Ok(()))
            )
        {
            return self
                .read(principal, &json!({"actor": actor, "handle": key}))
                .await;
        }
        Ok(record)
    }

    /// Python `ServerExec.read`.
    pub async fn read(&self, principal: &str, p: &Value) -> Result<Value, ExecError> {
        let (Some(key), Some(actor)) = (
            p.get("handle").and_then(Value::as_str),
            p.get("actor").and_then(Value::as_str),
        ) else {
            return refused("Supply a command handle and actor");
        };
        let mut record = self.record(principal, actor, key).await?;
        let started = record["startedAt"].as_f64().unwrap_or_default();
        let expires = record["outputExpiresAt"]
            .as_f64()
            .unwrap_or(started + OUTPUT_SECONDS);
        if now() >= expires {
            return refused("The command output has expired");
        }
        let snapshot = self.snapshot(key);
        if let Some(snapshot) = &snapshot {
            let saved = &snapshot["record"];
            if record["status"] != "unknown"
                || !matches!(saved["status"].as_str(), Some("starting" | "running"))
            {
                record = saved.clone();
            }
        }
        let mut result = record.clone();
        if result.get("outputExpiresAt").is_none() {
            result["outputExpiresAt"] = json!(started + OUTPUT_SECONDS);
        }
        let last = snapshot.as_ref().is_some_and(|snapshot| {
            studio_runtime_store::truthy(snapshot["record"].get("finishedAt"))
        });
        for name in ["stdout", "stderr"] {
            let offset = match p.get(format!("{name}_offset")) {
                None => 0,
                Some(value) => match value.as_u64() {
                    Some(offset) if value.is_u64() => offset as usize,
                    _ => return refused("Supply nonnegative output byte offsets"),
                },
            };
            let buffer =
                Buffer::from_snapshot(snapshot.as_ref().and_then(|snapshot| snapshot.get(name)));
            if offset > buffer.total {
                return refused("Output offset exceeds the retained output");
            }
            let page = buffer.read(offset, READ_OUTPUT / 2, last);
            result[name] = page["text"].clone();
            result[format!("{name}Offset")] = page["offset"].clone();
            result[format!("{name}NextOffset")] = page["nextOffset"].clone();
            result[format!("{name}GapBytes")] = page["gapBytes"].clone();
            result[format!("{name}Bytes")] = page["totalBytes"].clone();
            result[format!("{name}Truncated")] = page["truncated"].clone();
        }
        Ok(result)
    }

    /// Python `ServerExec.control` for `exec_input` and `exec_cancel`.
    pub async fn control(
        &self,
        principal: &str,
        action: &str,
        p: &Value,
        request_id: &str,
    ) -> Result<Value, ExecError> {
        let (Some(key), Some(actor)) = (
            p.get("handle").and_then(Value::as_str),
            p.get("actor").and_then(Value::as_str),
        ) else {
            return refused("Supply a command handle and actor");
        };
        let record = self.record(principal, actor, key).await?;
        let mut control = Control::Cancel;
        if action == "exec_input" {
            let input = match p.get("input") {
                None if p.get("close_stdin") == Some(&Value::Bool(true)) => Some(""),
                value => value.and_then(Value::as_str),
            };
            let Some(input) = input.filter(|input| input.len() <= MAX_INPUT_BYTES) else {
                return refused("Supply input text of at most 64 KiB");
            };
            let close = match p.get("close_stdin") {
                None => false,
                Some(Value::Bool(close)) => *close,
                Some(_) => return refused("Supply a Boolean close_stdin value"),
            };
            control = Control::Input {
                data: input.as_bytes().to_vec(),
                close,
            };
        }
        let sender = self
            .active
            .lock()
            .map_err(|_| ExecError::Failed("The command runner is unavailable".into()))?
            .get(key)
            .cloned();
        let Some(sender) = sender else {
            return Ok(json!({"handle": key, "status": record["status"], "accepted": false}));
        };
        if action == "exec_input" {
            let (key_owned, request) = (key.to_owned(), request_id.to_owned());
            self.store
                .transaction_with(move |tx| -> Result<(), ExecError> {
                    let count: i64 = tx.query_row(
                        "SELECT count(*) FROM runtime_server_exec_input WHERE handle=?1",
                        [&key_owned],
                        |row| row.get(0),
                    )?;
                    if count >= MAX_INPUTS {
                        return refused(
                            "The command has accepted 32 input requests. No more input can be sent.",
                        );
                    }
                    tx.execute(
                        "INSERT INTO runtime_server_exec_input VALUES(?1,?2)",
                        [&request, &key_owned],
                    )?;
                    Ok(())
                })
                .await?;
        }
        if sender.try_send(control).is_err() {
            return Err(ExecError::Failed(
                "The command no longer accepts controls; outcome unknown".into(),
            ));
        }
        Ok(json!({"handle": key, "accepted": true}))
    }

    fn release(&self, key: &str) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(key);
        }
    }
}

fn create_private_folder(folder: &Path) -> Result<(), ExecError> {
    match std::fs::create_dir(folder) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(ExecError::Failed(error.to_string())),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| ExecError::Failed(error.to_string()))?;
    }
    Ok(())
}

/// Python `secrets.token_hex(32)`.
fn new_marker() -> Result<String, ExecError> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes)
        .map_err(|_| ExecError::Failed("No random marker is available".into()))?;
    Ok(data_encoding::HEXLOWER.encode(&bytes))
}

/// The processes of one command that a stop may signal.
struct Scope {
    /// The private `STUDIO_EXEC_ID` value of the command.
    marker: String,
    /// The command's root process while it runs unreaped: its PID cannot
    /// belong to another process, so its process tree is the command's.
    running_root: Option<u32>,
    /// The command's process group (the root PID) and the Unix seconds in
    /// which a member of it can have started: from the command start to the
    /// moment its root was reaped. A group ID that another process reuses
    /// later only has members that started after that moment.
    group: Option<(u32, u64, u64)>,
}

/// Seconds that a stop may take before its result stays unknown (Python
/// `Descendants.stop`).
const STOP_SECONDS: u64 = 3;

/// The result of one stop: the stopped process IDs, and the processes that
/// still ran at the cleanup deadline.
struct Stopped {
    pids: Vec<u32>,
    remaining: usize,
}

/// The live processes of one command with their start times: the tree below
/// its running root, the members of its process group that started within its
/// lifetime, and every process whose launch environment holds its marker.
/// macOS hides the environment of platform binaries, so the group covers them.
/// Only processes of this user are selected; zombies have already stopped.
fn command_processes(scope: &Scope, system: &mut sysinfo::System) -> Vec<(u32, u64)> {
    use sysinfo::{ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, UpdateKind};
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .with_environ(UpdateKind::Always)
            .with_user(UpdateKind::Always),
    );
    let own = std::process::id();
    let own_user = system
        .process(sysinfo::Pid::from_u32(own))
        .and_then(|process| process.user_id().cloned());
    let selectable = |process: &sysinfo::Process| {
        own_user.is_some()
            && process.user_id() == own_user.as_ref()
            && !matches!(
                process.status(),
                ProcessStatus::Zombie | ProcessStatus::Dead
            )
    };
    let mut tree: Vec<u32> = scope.running_root.into_iter().collect();
    let mut index = 0;
    while index < tree.len() {
        let parent = tree[index];
        for (pid, process) in system.processes() {
            if process.parent().map(|parent| parent.as_u32()) == Some(parent)
                && !tree.contains(&pid.as_u32())
            {
                tree.push(pid.as_u32());
            }
        }
        index += 1;
    }
    // A process whose PID is the group ID after the root was reaped leads a
    // reused group, so the group selects nothing then.
    let group = scope.group.filter(|(group, _, _)| {
        scope.running_root.is_some() || system.process(sysinfo::Pid::from_u32(*group)).is_none()
    });
    let expected = format!("{MARKER_NAME}={}", scope.marker);
    let mut targets = Vec::new();
    for (pid, process) in system.processes() {
        let pid = pid.as_u32();
        if pid == own || !selectable(process) {
            continue;
        }
        let in_tree = tree.contains(&pid);
        let marked = process
            .environ()
            .iter()
            .any(|entry| entry.to_str() == Some(expected.as_str()));
        let member = group.is_some_and(|(group, first, last)| {
            let started = process.start_time();
            (first.saturating_sub(1)..=last).contains(&started)
                && i32::try_from(pid).ok().is_some_and(|raw| {
                    nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(raw)))
                        .is_ok_and(|found| i64::from(found.as_raw()) == i64::from(group))
                })
        });
        if in_tree || marked || member {
            targets.push((pid, process.start_time()));
        }
    }
    targets
}

/// Run a stop in the blocking pool; a failed pool keeps the outcome unknown.
async fn stop_in_pool(scope: Scope) -> Stopped {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/server_exec",
        move || stop_command(&scope),
    )
    .await
    .unwrap_or(Stopped {
        pids: Vec::new(),
        remaining: 1,
    })
}

/// Python `Descendants.stop`: stop and kill every proven process of the
/// command until none runs or the cleanup deadline passes. Each signal goes
/// only to a PID whose start time still matches the selected process.
#[allow(
    clippy::disallowed_methods,
    reason = "stop_in_pool owns cleanup on a labeled blocking worker"
)]
fn stop_command(scope: &Scope) -> Stopped {
    use sysinfo::{ProcessRefreshKind, ProcessesToUpdate};
    let deadline = Instant::now() + Duration::from_secs(STOP_SECONDS);
    let mut system = sysinfo::System::new();
    let mut stopped: Vec<u32> = Vec::new();
    loop {
        let targets = command_processes(scope, &mut system);
        if targets.is_empty() {
            return Stopped {
                pids: stopped,
                remaining: 0,
            };
        }
        if Instant::now() >= deadline {
            return Stopped {
                pids: stopped,
                remaining: targets.len(),
            };
        }
        let mut signalled = Vec::new();
        for (pid, started) in &targets {
            let id = sysinfo::Pid::from_u32(*pid);
            system.refresh_processes_specifics(
                ProcessesToUpdate::Some(&[id]),
                true,
                ProcessRefreshKind::nothing(),
            );
            let same = system
                .process(id)
                .is_some_and(|process| process.start_time() == *started);
            let Ok(raw) = i32::try_from(*pid) else {
                continue;
            };
            if same
                && nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(raw),
                    nix::sys::signal::Signal::SIGSTOP,
                )
                .is_ok()
            {
                signalled.push(raw);
                if !stopped.contains(pid) {
                    stopped.push(*pid);
                }
            }
        }
        for raw in signalled {
            if nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(raw),
                nix::sys::signal::Signal::SIGKILL,
            )
            .is_err()
            {
                // The stopped process exited before the kill.
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Job {
    runner: Arc<ServerExec>,
    principal: String,
    actor: String,
    key: String,
    argv: Vec<String>,
    cwd: String,
    environment: Vec<(String, String)>,
    marker: String,
    timeout: u64,
    stdout: Buffer,
    stderr: Buffer,
    record: Value,
    snapshot_dirty: bool,
    last_publish: tokio::time::Instant,
}

/// The current Unix second, in the resolution of process start times.
fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

impl Job {
    /// The processes that a stop of this command may signal.
    fn scope(
        &self,
        root: Option<u32>,
        reaped: bool,
        started: u64,
        reaped_at: Option<u64>,
    ) -> Scope {
        Scope {
            marker: self.marker.clone(),
            running_root: root.filter(|_| !reaped),
            group: root.map(|root| (root, started, reaped_at.unwrap_or_else(unix_seconds))),
        }
    }

    async fn publish(&mut self) {
        let path = job_path(&self.runner.folder, &self.key, ".output.json");
        let (record, stdout, stderr) = (
            self.record.clone(),
            self.stdout.clone(),
            self.stderr.clone(),
        );
        // Serialize, replace and fsync only on the labeled blocking worker.
        // Await each write so initial/final snapshots cannot overtake each other.
        let result = atomic_json(path, move || {
            json!({
                "record": record,
                "stdout": stdout.snapshot(),
                "stderr": stderr.snapshot(),
            })
        })
        .await;
        self.last_publish = tokio::time::Instant::now();
        match result {
            Ok(()) => self.snapshot_dirty = false,
            Err(error) => {
                self.snapshot_dirty = true;
                eprintln!("server command {} snapshot failed: {error}", self.key);
            }
        }
    }

    async fn publish_if_due(&mut self) {
        if self.snapshot_dirty && self.last_publish.elapsed() >= SNAPSHOT_PUBLISH_INTERVAL {
            self.publish().await;
        }
    }

    fn append_output(&mut self, is_stdout: bool, data: &[u8]) {
        if !data.is_empty() {
            if is_stdout {
                self.stdout.append(data);
            } else {
                self.stderr.append(data);
            }
            self.snapshot_dirty = true;
        }
    }

    async fn finish(mut self, began: Instant) {
        self.record["duration"] = json!(began.elapsed().as_secs_f64());
        self.record["finishedAt"] = json!(now());
        self.publish().await;
        let (runner, principal, actor, key, record) = (
            Arc::clone(&self.runner),
            self.principal.clone(),
            self.actor.clone(),
            self.key.clone(),
            self.record.clone(),
        );
        if let Err(error) = runner.save(&principal, &actor, &key, &record).await {
            eprintln!("server command {key} record failed: {error}");
        }
        // Python removes the adapter after its final save: a control call
        // until then still reaches this command.
        runner.release(&key);
    }

    /// Python `child`, owned by the backend.
    async fn run(mut self, mut controls: tokio::sync::mpsc::Receiver<Control>) {
        let began = Instant::now();
        let fence = job_path(&self.runner.folder, &self.key, ".started");
        let fenced = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/server_exec/start_fence",
            move || {
                std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&fence)
                    .and_then(|file| file.sync_all())
            },
        )
        .await;
        let fenced = match fenced {
            Ok(result) => result,
            Err(error) => {
                self.record["status"] = json!("unknown");
                self.record["error"] = json!(error.to_string());
                return self.finish(began).await;
            }
        };
        match fenced {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                self.record["status"] = json!("unknown");
                self.record["error"] =
                    json!("A prior command start exists. The command was not run again.");
                return self.finish(began).await;
            }
            Err(error) => {
                self.record["status"] = json!("failed");
                self.record["error"] = json!(error.to_string());
                return self.finish(began).await;
            }
        }
        self.publish().await;
        let mut command = tokio::process::Command::new(&self.argv[0]);
        command
            .args(&self.argv[1..])
            .current_dir(&self.cwd)
            .envs(self.environment.iter().map(|(key, value)| (key, value)))
            .env(MARKER_NAME, &self.marker)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        command.process_group(0);
        let started_at = unix_seconds();
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.record["status"] = json!("failed");
                self.record["error"] = json!(
                    error
                        .raw_os_error()
                        // Python `os.strerror(errno)`.
                        .map(|code| nix::errno::Errno::from_raw(code).desc().to_owned())
                        .unwrap_or_else(|| "Command start failed".to_owned())
                );
                return self.finish(began).await;
            }
        };
        let root = child.id();
        let mut stdin = child.stdin.take();
        let (output, mut events) = tokio::sync::mpsc::channel::<(bool, Option<Vec<u8>>)>(64);
        let mut readers = Vec::new();
        for (is_stdout, pipe) in [
            (
                true,
                child
                    .stdout
                    .take()
                    .map(|pipe| Box::new(pipe) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            ),
            (
                false,
                child
                    .stderr
                    .take()
                    .map(|pipe| Box::new(pipe) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            ),
        ] {
            let Some(mut pipe) = pipe else { continue };
            let output = output.clone();
            readers.push(tokio::spawn(async move {
                let mut buffer = vec![0_u8; 8192];
                loop {
                    match pipe.read(&mut buffer).await {
                        Ok(0) | Err(_) => {
                            drop(output.send((is_stdout, None)).await);
                            return;
                        }
                        Ok(count) => {
                            if output
                                .send((is_stdout, Some(buffer[..count].to_vec())))
                                .await
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }));
        }
        drop(output);
        self.record["status"] = json!("running");
        self.publish().await;
        let (runner, principal, actor, key, record) = (
            Arc::clone(&self.runner),
            self.principal.clone(),
            self.actor.clone(),
            self.key.clone(),
            self.record.clone(),
        );
        if let Err(error) = runner.save(&principal, &actor, &key, &record).await {
            eprintln!("server command {key} record failed: {error}");
        }
        let mut redactors = [
            MarkerRedactor::new(&self.marker),
            MarkerRedactor::new(&self.marker),
        ];
        let deadline = began + Duration::from_secs(self.timeout);
        let mut ended = 0;
        let mut pending: Vec<u8> = Vec::new();
        let mut close_input = false;
        let mut cancelled = false;
        let mut killed_at: Option<Instant> = None;
        let mut exit: Option<std::process::ExitStatus> = None;
        let mut stopped: Vec<u32> = Vec::new();
        let mut controls_open = true;
        let mut reaped_at: Option<u64> = None;
        let mut wait_failed = false;
        loop {
            if ended >= 2 && exit.is_some() {
                break;
            }
            if killed_at.is_some_and(|at| at.elapsed().as_secs_f64() > DRAIN_SECONDS) {
                break;
            }
            if killed_at.is_none()
                && (cancelled || Instant::now() >= deadline || exit.is_some() || wait_failed)
            {
                self.record["timedOut"] = json!(!cancelled && Instant::now() >= deadline);
                let scope = self.scope(root, exit.is_some(), started_at, reaped_at);
                let result = stop_in_pool(scope).await;
                for pid in result.pids {
                    if Some(pid) != root && !stopped.contains(&pid) {
                        stopped.push(pid);
                    }
                }
                self.record["stoppedDescendants"] = json!(stopped.len());
                self.snapshot_dirty = true;
                killed_at = Some(Instant::now());
                close_input = true;
            }
            if let Some(input) = stdin.as_mut() {
                if !pending.is_empty() {
                    let count = pending.len().min(8192);
                    match tokio::time::timeout(
                        Duration::from_millis(50),
                        input.write(&pending[..count]),
                    )
                    .await
                    {
                        Ok(Ok(written)) => {
                            pending.drain(..written);
                        }
                        Ok(Err(_)) => pending.clear(),
                        Err(_) => {}
                    }
                }
                if close_input && pending.is_empty() {
                    stdin = None;
                }
            }
            tokio::select! {
                control = controls.recv(), if controls_open && killed_at.is_none() => match control {
                    Some(Control::Cancel) => cancelled = true,
                    Some(Control::Input { data, close }) => {
                        pending.extend_from_slice(&data);
                        close_input |= close;
                    }
                    None => controls_open = false,
                },
                event = events.recv(), if ended < 2 => match event {
                    Some((is_stdout, data)) => {
                        let index = usize::from(!is_stdout);
                        let last = data.is_none();
                        let redacted = redactors[index].feed(data.as_deref().unwrap_or_default(), last);
                        self.append_output(is_stdout, &redacted);
                        if last {
                            ended += 1;
                        }
                    }
                    None => ended = 2,
                },
                status = child.wait(), if exit.is_none() && !wait_failed => match status {
                    Ok(status) => {
                        exit = Some(status);
                        reaped_at = Some(unix_seconds());
                    }
                    // The exit status is unknown; the stop below still runs.
                    Err(_) => wait_failed = true,
                },
                () = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
            self.publish_if_due().await;
        }
        let status = match exit {
            Some(status) => Some(status),
            None if wait_failed => None,
            None => tokio::time::timeout(Duration::from_secs(2), child.wait())
                .await
                .ok()
                .and_then(Result::ok),
        };
        // Python `stop_descendants` in its `finally`: no process of the command remains.
        let reaped_at = reaped_at.or_else(|| status.is_some().then(unix_seconds));
        let scope = self.scope(root, status.is_some(), started_at, reaped_at);
        let result = stop_in_pool(scope).await;
        for pid in result.pids {
            if Some(pid) != root && !stopped.contains(&pid) {
                stopped.push(pid);
            }
        }
        for reader in readers {
            reader.abort();
        }
        self.record["stoppedDescendants"] = json!(stopped.len());
        self.record["cleanupUnknownForks"] = json!(result.remaining);
        self.record["status"] = json!(if cancelled { "cancelled" } else { "completed" });
        match status {
            Some(status) => {
                #[cfg(unix)]
                let signal = std::os::unix::process::ExitStatusExt::signal(&status);
                #[cfg(not(unix))]
                let signal: Option<i32> = None;
                self.record["exitCode"] = json!(status.code());
                self.record["signal"] = json!(signal);
            }
            None => {
                self.record["status"] = json!("unknown");
                self.record["error"] =
                    json!("The command process still runs; inspect the target server");
                self.record["cleanupUnknownForks"] = json!(result.remaining.max(1));
            }
        }
        if result.remaining > 0 && status.is_some() {
            self.record["status"] = json!("unknown");
            self.record["error"] =
                json!("Descendant cleanup is incomplete; inspect the target server");
        }
        self.finish(began).await;
    }
}

#[cfg(test)]
#[path = "server_exec/tests.rs"]
mod tests;
