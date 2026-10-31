//! The Mac service of `host_exec` (docs/host-exec.md).
//!
//! One process per VM state folder serves `host-exec.sock`, which the native
//! VM helper connects to the guest's port 4051. It owns the Mac slots, the
//! command processes and their results. It runs in its own session, so a
//! running Mac command survives a backend restart. The backend starts it with
//! [`lifecycle::ensure_service`] and replaces an idle service of another build.
//!
//! Each mutating request keeps a durable receipt under its request ID: an exact
//! retry gets the saved answer, a changed request under the same ID is refused,
//! and a request whose answer was never saved stays an unknown outcome. A
//! command never runs twice.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use rusqlite::OptionalExtension;
use serde_json::{Map, Value, json};

mod clone_ffi;
mod command;
pub mod lifecycle;
mod protocol;
mod slots;
mod transfer;

use protocol::{Failure, HostExecError, INVALID, Outcome, require};

/// The subcommand of `studio-runtime-server` that runs this service.
pub const SUBCOMMAND: &str = "host-exec-service";
/// Stdout line proving the host service has bound and secured its Unix socket.
pub const SERVICE_READY_LINE: &str = "studio-host-exec-ready";
/// The optional argument that names the Studio server's host bridge folder.
pub const BROWSER_HOST_ARGUMENT: &str = "--browser-host";
/// The service socket in the VM state folder; the native helper forwards the
/// guest port to it (`linux-vm/main.swift`).
pub const SOCKET_FILE: &str = "host-exec.sock";
/// The channel token file in the VM state folder; the guest gets it with `host.configure`.
pub const TOKEN_FILE: &str = crate::host_exec::TOKEN_FILE;
/// The lease that one running service holds.
pub const LEASE_FILE: &str = "host-exec.lock";
/// The lease of a service start, held by [`lifecycle::ensure_service`].
pub(crate) const START_LEASE_FILE: &str = "host-exec-start.lock";
/// The output of the service process.
pub(crate) const LOG_FILE: &str = "host-exec.log";
/// Receipts and command output events.
const DATABASE_FILE: &str = "host-exec.sqlite3";
/// The protocol version that `health` reports.
pub(crate) const PROTOCOL: u64 = 1;

/// Mac slots of a project without a settings file.
pub(crate) const DEFAULT_SLOTS: i64 = 4;
/// Disk budget of a project without a settings file, in bytes.
pub(crate) const DEFAULT_BUDGET: u64 = 20 * 1024 * 1024 * 1024;
/// Largest slot count of a project.
const MAX_SLOTS: i64 = 64;
/// Largest `diskBudgetGiB` of a project, in GiB.
const MAX_BUDGET_GIB: f64 = 1024.0 * 1024.0;
/// Largest `macOnly` list of a project.
const MAX_MAC_ONLY: usize = 256;
/// The project settings file in `layr-projects/<project>/`.
pub(crate) const SETTINGS_FILE: &str = "host-exec.json";
/// What this service accepts beyond the first protocol; the guest reads it from the lease.
pub(crate) const FEATURES: [&str; 4] = ["prime", "batch", "mode", "clone"];
/// The last slot generation number of a project folder.
pub(crate) const GENERATION_COUNTER: &str = ".generation-counter.json";
/// Environment names of the Mac command; the tool description names the first three.
pub(crate) const ENV_DERIVED_DATA: &str = "HOST_EXEC_DERIVED_DATA";
pub(crate) const ENV_PACKAGE_CACHE: &str = "HOST_EXEC_PACKAGE_CACHE";
pub(crate) const ENV_ARTIFACTS: &str = "HOST_EXEC_ARTIFACTS";
/// The ID of the layr state that the slot holds.
pub(crate) const ENV_STATE: &str = "HOST_EXEC_STATE";
/// The git tree ID of that state: what `layr rev-parse <state>^{tree}` prints.
pub(crate) const ENV_TREE: &str = "HOST_EXEC_TREE";
/// Connections that the service serves at the same time.
const MAX_CLIENTS: usize = 64;
/// Wait for the request line of a connection, in seconds.
const READ_SECONDS: u64 = 30;
/// Wait for the answer write of a connection, in seconds.
const WRITE_SECONDS: u64 = 5;
/// Longest failure text in a receipt or status, in characters.
const ERROR_CHARS: usize = 1000;
/// Operations that `list` returns.
const LIST_LIMIT: usize = 50;
/// Largest `waitSeconds` of `attach`.
const ATTACH_MAX_SECONDS: f64 = 10.0;
/// Poll interval of `attach`, in milliseconds.
const ATTACH_POLL_MS: u64 = 25;
/// Events that one `attach` answer returns.
const ATTACH_EVENTS: i64 = 16;
/// Methods without a receipt: they change no operation.
const READ_METHODS: [&str; 6] = [
    "status",
    "manifest",
    "attach",
    "read",
    "result.manifest",
    "list",
];

/// One operation record (`host-operations/<key>/status.json`).
pub(crate) type Op = Map<String, Value>;

#[derive(serde::Deserialize)]
struct IncomingFrame {
    id: Option<Value>,
    token: Option<Value>,
    method: Option<Value>,
    params: Option<Box<serde_json::value::RawValue>>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RetireParams {
    build_id: String,
    pid: u64,
}

#[derive(serde::Deserialize)]
struct HealthParams {}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RunParams {
    operation_id: String,
    agent_id: String,
    argv: Vec<String>,
    manifest_sha256: String,
    #[serde(default)]
    timeout_seconds: Option<f64>,
    #[serde(default)]
    caches: Vec<String>,
    #[serde(default)]
    layr_state: Option<String>,
    #[serde(default)]
    layr_tree: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestParams {
    operation_id: String,
    agent_id: String,
    #[serde(default)]
    after: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PrimeParams {
    operation_id: String,
    agent_id: String,
    #[serde(default)]
    caches: Vec<String>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncModeParams {
    operation_id: String,
    agent_id: String,
    items: Vec<SyncModeItem>,
}

#[derive(Clone, serde::Deserialize)]
pub(crate) struct SyncModeItem {
    path: String,
    entry: protocol::ManifestEntry,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncCloneParams {
    operation_id: String,
    agent_id: String,
    path: String,
    entry: protocol::ManifestEntry,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResultManifestParams {
    operation_id: String,
    agent_id: String,
    area: ResultArea,
    #[serde(default)]
    after: String,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "lowercase")]
enum ResultArea {
    Changes,
    Artifacts,
}

impl ResultArea {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Changes => "changes",
            Self::Artifacts => "artifacts",
        }
    }
}

enum FrameParams {
    Health,
    Retire(RetireParams),
    Run(RunParams),
    Manifest(ManifestParams),
    Prime(PrimeParams),
    SyncMode(SyncModeParams),
    SyncClone(SyncCloneParams),
    ResultManifest(ResultManifestParams),
    // Existing dispatch until each remaining method has its own typed variant.
    Legacy(Map<String, Value>),
}

fn receipt_digest(method: &str, params: &str) -> String {
    protocol::key(&format!("{method}\0{params}"))
}

/// A command process: its process group and its exit.
#[derive(Clone)]
pub(crate) struct ChildHandle {
    pub(crate) pid: i32,
    pub(crate) exited: tokio::sync::watch::Receiver<bool>,
    /// True once the leader is reaped. After that the group ID may name another
    /// group, so every group signal holds this lock and sends nothing then.
    pub(crate) reaped: Arc<std::sync::Mutex<bool>>,
}

#[derive(Default)]
pub(crate) struct Inner {
    active: HashMap<String, tokio::sync::watch::Receiver<Option<Value>>>,
    pub(crate) commands: HashSet<String>,
    pub(crate) children: HashMap<String, ChildHandle>,
    /// Per project folder: the bytes of the last walk plus the files written since then.
    pub(crate) usage: HashMap<PathBuf, u64>,
    /// Sorted manifests during paging, by operation and area.
    pages: HashMap<String, protocol::ManifestPageItems>,
    /// Hashes of files cloned into a slot, for its next scan, by slot folder.
    pub(crate) cloned: HashMap<PathBuf, protocol::HashCache>,
    clients: usize,
    reading: usize,
    retiring: bool,
}

pub(crate) struct HostService {
    pub(crate) state: PathBuf,
    /// The host bridge folder of the Studio server (`<state>/browser-host`). A command gets
    /// its launcher as `BROWSER` with the calling chat as the session, so a sign-in that a VM
    /// chat runs on this Mac shows a card (docs/vm-browser-bridge.md, "Native chats").
    pub(crate) browser_host: Option<PathBuf>,
    pub(crate) pool: PathBuf,
    pub(crate) default_slots: i64,
    pub(crate) default_budget: u64,
    token: String,
    build_id: String,
    _lease: fs::File,
    /// Shared with the periodic WAL flush thread, which ends when the service drops.
    db: Arc<Mutex<rusqlite::Connection>>,
    inner: Mutex<Inner>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    stopped: tokio::sync::Notify,
    #[cfg(test)]
    command_finished: tokio::sync::Notify,
}

/// The project settings of one operation.
pub(crate) struct Settings {
    pub(crate) slots: i64,
    pub(crate) disk_budget: u64,
    pub(crate) mac_only: Vec<String>,
}

pub(crate) fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64())
}

fn truncate(text: &str) -> String {
    text.chars().take(ERROR_CHARS).collect()
}

pub(crate) fn private_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)
}

pub(crate) fn private_dirs(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Remove a folder tree; a failure leaves the rest in place.
pub(crate) fn remove_tree_quietly(path: &Path) {
    if fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) {
        drop(fs::remove_dir_all(path));
    }
}

/// Remove a file or a link, or a folder with its content.
pub(crate) fn remove_entry(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn text<'a>(op: &'a Op, field: &str) -> &'a str {
    op.get(field).and_then(Value::as_str).unwrap_or_default()
}

pub(crate) fn string_list(op: &Op, field: &str) -> Vec<String> {
    op.get(field)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// A JSON integer: not a boolean and not a fraction.
pub(crate) fn integer(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Number(number)) if !number.is_f64() => number.as_i64(),
        _ => None,
    }
}

/// A JSON number that is not a boolean.
pub(crate) fn number(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(number)) => number.as_f64(),
        _ => None,
    }
}

impl HostService {
    pub(crate) fn open(state: &Path, build_id: String) -> std::io::Result<Self> {
        Self::with_limits(state, build_id, DEFAULT_SLOTS, DEFAULT_BUDGET)
    }

    pub(crate) fn with_limits(
        state: &Path,
        build_id: String,
        slots: i64,
        budget: u64,
    ) -> std::io::Result<Self> {
        private_dirs(state)?;
        let state = studio_runtime_store::path_identity::CanonicalPath::existing_directory(state)?
            .into_path_buf();
        let pool = state.join("host-slots");
        if !pool.is_dir() {
            private_dir(&pool)?;
        }
        let token = fs::read_to_string(state.join(TOKEN_FILE))?;
        let lease = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(state.join(LEASE_FILE))?;
        rustix::fs::flock(&lease, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(std::io::Error::from)?;
        let db =
            rusqlite::Connection::open(state.join(DATABASE_FILE)).map_err(std::io::Error::other)?;
        studio_runtime_store::sqlite_durability::configure_writer(&db)
            .map_err(std::io::Error::other)?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS receipts (id TEXT PRIMARY KEY, digest TEXT, response TEXT);
             CREATE TABLE IF NOT EXISTS events (operation TEXT, seq INTEGER, value TEXT, PRIMARY KEY(operation,seq));",
        )
        .map_err(std::io::Error::other)?;
        let db = Arc::new(Mutex::new(db));
        studio_runtime_store::sqlite_durability::spawn_periodic_flush(
            "host-exec",
            Arc::downgrade(&db),
        )?;
        Ok(Self {
            state,
            browser_host: None,
            pool,
            default_slots: slots,
            default_budget: budget,
            token,
            build_id,
            _lease: lease,
            db,
            inner: Mutex::new(Inner::default()),
            locks: Mutex::new(HashMap::new()),
            stopped: tokio::sync::Notify::new(),
            #[cfg(test)]
            command_finished: tokio::sync::Notify::new(),
        })
    }

    /// Send the sign-in links of commands to the host bridge in `folder`.
    pub(crate) fn with_browser_host(mut self, folder: Option<PathBuf>) -> Self {
        self.browser_host = folder;
        self
    }

    pub(crate) fn inner(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn db(&self) -> MutexGuard<'_, rusqlite::Connection> {
        self.db
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn lock_for(&self, name: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(locks.entry(name.to_owned()).or_default())
    }

    /// Whether a request holds the named lock now.
    pub(crate) fn is_locked(&self, name: &str) -> bool {
        let lock = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .cloned();
        lock.is_some_and(|lock| lock.try_lock().is_err())
    }

    pub(crate) fn forget_lock(&self, name: &str) {
        self.locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name);
    }

    pub(crate) fn opdir(&self, identity: &str) -> PathBuf {
        self.state
            .join("host-operations")
            .join(protocol::key(identity))
    }

    pub(crate) fn load(&self, identity: &str) -> Outcome<Op> {
        let path = self.opdir(identity).join("status.json");
        require(
            path.is_file(),
            "The host command does not exist",
            "not_found",
        )?;
        let Value::Object(mut value) = protocol::read_json(&path)? else {
            return Err(Failure::Unexpected(
                "The host command status is not an object".into(),
            ));
        };
        if value.get("state").and_then(Value::as_str) == Some("running")
            && !self.inner().commands.contains(identity)
        {
            value.insert("state".into(), json!("unknown"));
            value.insert("reason".into(), json!("host_owner_lost"));
        }
        Ok(value)
    }

    pub(crate) fn save(&self, identity: &str, op: &Op) -> Outcome<()> {
        protocol::atomic(
            &self.opdir(identity).join("status.json"),
            &Value::Object(op.clone()),
        )?;
        Ok(())
    }

    pub(crate) fn event(&self, identity: &str, value: Map<String, Value>) -> Outcome<()> {
        let db = self.db();
        let seq: i64 = db.query_row(
            "SELECT coalesce(max(seq),0)+1 FROM events WHERE operation=?1",
            [identity],
            |row| row.get(0),
        )?;
        let mut row = Map::new();
        row.insert("seq".into(), json!(seq));
        row.extend(value);
        db.execute(
            "INSERT INTO events VALUES (?1,?2,?3)",
            rusqlite::params![identity, seq, Value::Object(row).to_string()],
        )?;
        Ok(())
    }

    fn has_event_after(&self, identity: &str, after: i64) -> Outcome<bool> {
        let db = self.db();
        let mut statement =
            db.prepare_cached("SELECT 1 FROM events WHERE operation=?1 AND seq>?2")?;
        Ok(statement.exists(rusqlite::params![identity, after])?)
    }

    fn events_after(&self, identity: &str, after: i64) -> Outcome<Vec<Value>> {
        let db = self.db();
        let mut statement = db.prepare_cached(
            "SELECT value FROM events WHERE operation=?1 AND seq>?2 ORDER BY seq LIMIT ?3",
        )?;
        let rows = statement
            .query_map(rusqlite::params![identity, after, ATTACH_EVENTS], |row| {
                row.get::<_, String>(0)
            })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(serde_json::from_str(&row?)?);
        }
        Ok(events)
    }

    /// The slot count, disk budget and Mac-only paths of a project. The operator
    /// sets them in `layr-projects/<project>/host-exec.json`; the VM cannot write it.
    pub(crate) fn settings(&self, project_id: &str) -> Result<Settings, HostExecError> {
        let path = self
            .state
            .join("layr-projects")
            .join(project_id)
            .join(SETTINGS_FILE);
        let shown = path.display().to_string();
        let invalid = |message: String| HostExecError::new("invalid_config", message);
        let mut value = Value::Object(Map::new());
        if slots::project_folder_name(project_id) {
            match fs::read_to_string(&path) {
                Ok(text) => {
                    value = serde_json::from_str(&text).map_err(|error| {
                        invalid(format!("{shown} is not readable JSON: {error}"))
                    })?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(invalid(format!("{shown} is not readable JSON: {error}")));
                }
            }
        }
        let fields = match &value {
            Value::Object(fields)
                if fields
                    .keys()
                    .all(|key| ["slots", "diskBudgetGiB", "macOnly"].contains(&key.as_str())) =>
            {
                fields
            }
            Value::Null
            | Value::Bool(_)
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_)
            | Value::Object(_) => {
                return Err(invalid(format!(
                    "{shown} accepts only slots, diskBudgetGiB and macOnly"
                )));
            }
        };
        let slots = match fields.get("slots") {
            None => self.default_slots,
            Some(value) => integer(Some(value))
                .filter(|slots| (1..=MAX_SLOTS).contains(slots))
                .ok_or_else(|| {
                    invalid(format!(
                        "{shown}: slots must be an integer from 1 to {MAX_SLOTS}"
                    ))
                })?,
        };
        let disk_budget = match fields.get("diskBudgetGiB") {
            None => self.default_budget,
            Some(value) => {
                let refusal =
                    || invalid(format!("{shown}: diskBudgetGiB must be a number above 0"));
                match value {
                    Value::Number(size) if !size.is_f64() => {
                        let size = size
                            .as_i64()
                            .filter(|size| *size > 0 && (*size as f64) <= MAX_BUDGET_GIB)
                            .ok_or_else(refusal)?;
                        (size as u64) * 1024 * 1024 * 1024
                    }
                    Value::Number(size) => {
                        let size = size
                            .as_f64()
                            .filter(|size| *size > 0.0 && *size <= MAX_BUDGET_GIB)
                            .ok_or_else(refusal)?;
                        (size * 1024.0 * 1024.0 * 1024.0) as u64
                    }
                    Value::Null
                    | Value::Bool(_)
                    | Value::String(_)
                    | Value::Array(_)
                    | Value::Object(_) => return Err(refusal()),
                }
            }
        };
        let mac_only = match fields.get("macOnly") {
            None => Vec::new(),
            Some(Value::Array(items)) if items.len() <= MAX_MAC_ONLY => {
                let mut paths = Vec::new();
                for item in items {
                    let path = protocol::relative(Some(item)).map_err(|error| {
                        invalid(format!("{shown}: macOnly {item}: {}", error.message))
                    })?;
                    paths.push(path);
                }
                paths
            }
            Some(_) => return Err(invalid(format!("{shown}: macOnly must be a list of paths"))),
        };
        Ok(Settings {
            slots,
            disk_budget,
            mac_only,
        })
    }

    pub(crate) fn budget_of(&self, op: &Op) -> u64 {
        op.get("diskBudget")
            .and_then(Value::as_u64)
            .unwrap_or(self.default_budget)
    }

    /// A page of a saved manifest. The sorted entries stay in memory until the last page.
    pub(crate) fn listing(
        &self,
        name: &str,
        path: &Path,
        after: &str,
        entries: Option<&protocol::Manifest>,
    ) -> Outcome<Value> {
        let saved = self.inner().pages.get(name).cloned();
        let items = match (entries, saved) {
            (Some(entries), _) => entries
                .iter()
                .map(|(path, entry)| (path.clone(), Some(entry.clone())))
                .collect(),
            (None, Some(items)) => items,
            (None, None) => {
                let map: std::collections::BTreeMap<String, Option<protocol::ManifestEntry>> =
                    serde_json::from_str(&fs::read_to_string(path)?)?;
                map.into_iter().collect()
            }
        };
        let result = protocol::page(&items, after)?;
        let mut inner = self.inner();
        if result["next"].is_null() {
            inner.pages.remove(name);
        } else {
            inner.pages.insert(name.to_owned(), items);
        }
        Ok(result)
    }

    /// Work of this process that a stop would cut. Older unknown outcomes and
    /// pending receipts stay in the state folder; the next service reports them.
    fn busy(&self) -> bool {
        let inner = self.inner();
        inner.retiring
            || !inner.commands.is_empty()
            || !inner.active.is_empty()
            || !inner.children.is_empty()
            || inner.reading > 0
    }

    async fn dispatch(
        self: &Arc<Self>,
        method: &str,
        params: &Map<String, Value>,
    ) -> Outcome<Value> {
        match method {
            "acquire" => return self.acquire(params).await.map(Value::Object),
            "list" => return self.operations(params.get("agentId")),
            "recover" => return self.recover(params.get("owner")),
            _ => {}
        }
        let identity = params
            .get("operationId")
            .and_then(Value::as_str)
            .ok_or_else(|| Failure::Unexpected("'operationId'".into()))?
            .to_owned();
        let mut op = self.load(&identity)?;
        // The broker sets these fields; the host never accepts a slot path from the caller.
        require(
            params.get("agentId") == op.get("agentId"),
            "The command belongs to another agent",
            "forbidden",
        )?;
        match method {
            "status" => return Ok(Value::Object(op)),
            "attach" => return self.attach(&identity, params).await,
            "read" => return self.read_result(&identity, &op, params),
            _ => {}
        }
        let slot = self.slot(&op)?;
        let mac_only = string_list(&op, "macOnly");
        match method {
            "sync.batch" => self.sync_batch(&op, &slot, params),
            method if method.starts_with("sync.") => {
                self.sync_entry(method, &identity, &op, &slot, &mac_only, params)
            }
            "cancel" => self.cancel(&identity, &mut op).await.map(Value::Object),
            "release" => self
                .release(&identity, op, &slot, params)
                .map(Value::Object),
            _ => Err(HostExecError::new(INVALID, "Unknown host method").into()),
        }
    }

    async fn dispatch_frame(
        self: &Arc<Self>,
        method: &str,
        params: &FrameParams,
    ) -> Outcome<Value> {
        match params {
            FrameParams::Legacy(params) => self.dispatch(method, params).await,
            FrameParams::Run(params) => {
                let op = self.load(&params.operation_id)?;
                require(
                    text(&op, "agentId") == params.agent_id,
                    "The command belongs to another agent",
                    "forbidden",
                )?;
                let slot = self.slot(&op)?;
                let mac_only = string_list(&op, "macOnly");
                self.run(&params.operation_id, op, slot, mac_only, params)
                    .await
                    .map(Value::Object)
            }
            FrameParams::Manifest(params) => {
                let op = self.load(&params.operation_id)?;
                require(
                    text(&op, "agentId") == params.agent_id,
                    "The command belongs to another agent",
                    "forbidden",
                )?;
                let slot = self.slot(&op)?;
                self.held_manifest(
                    &params.operation_id,
                    &slot,
                    string_list(&op, "macOnly"),
                    &params.after,
                )
                .await
            }
            FrameParams::Prime(params) => {
                let op = self.load(&params.operation_id)?;
                require(
                    text(&op, "agentId") == params.agent_id,
                    "The command belongs to another agent",
                    "forbidden",
                )?;
                let slot = self.slot(&op)?;
                self.prime_request(&params.operation_id, &op, &slot, &params.caches)
                    .await
            }
            FrameParams::SyncMode(params) => {
                let op = self.load(&params.operation_id)?;
                require(
                    text(&op, "agentId") == params.agent_id,
                    "The command belongs to another agent",
                    "forbidden",
                )?;
                let slot = self.slot(&op)?;
                self.sync_mode(&op, &slot, string_list(&op, "macOnly"), &params.items)
                    .await
            }
            FrameParams::SyncClone(params) => {
                let op = self.load(&params.operation_id)?;
                require(
                    text(&op, "agentId") == params.agent_id,
                    "The command belongs to another agent",
                    "forbidden",
                )?;
                let slot = self.slot(&op)?;
                self.sync_clone(&op, &slot, &params.path, &params.entry)
                    .await
            }
            FrameParams::ResultManifest(params) => {
                let op = self.load(&params.operation_id)?;
                require(
                    text(&op, "agentId") == params.agent_id,
                    "The command belongs to another agent",
                    "forbidden",
                )?;
                let area = params.area.as_str();
                require(
                    ["exited", "released"].contains(&text(&op, "state")),
                    "The command has no final files",
                    "outcome_unknown",
                )?;
                self.listing(
                    &format!("{}\0{area}", params.operation_id),
                    &self
                        .opdir(&params.operation_id)
                        .join(format!("{area}.json")),
                    &params.after,
                    None,
                )
            }
            FrameParams::Health | FrameParams::Retire(_) => {
                Err(HostExecError::new(INVALID, "Invalid request").into())
            }
        }
    }

    async fn attach(&self, identity: &str, params: &Map<String, Value>) -> Outcome<Value> {
        let after = match params.get("afterSeq") {
            None => 0,
            Some(value) => integer(Some(value))
                .filter(|after| *after >= 0)
                .ok_or_else(|| HostExecError::new(INVALID, "Invalid event cursor"))?,
        };
        let wait = match params.get("waitSeconds") {
            None => 0.0,
            Some(value) => {
                number(Some(value)).ok_or_else(|| Failure::Unexpected("Invalid wait".into()))?
            }
        };
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs_f64(wait.clamp(0.0, ATTACH_MAX_SECONDS));
        while tokio::time::Instant::now() < deadline && self.inner().commands.contains(identity) {
            if self.has_event_after(identity, after)? {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(ATTACH_POLL_MS)).await;
        }
        let events = self.events_after(identity, after)?;
        Ok(json!({"events": events, "status": self.load(identity)?}))
    }

    fn read_result(&self, identity: &str, op: &Op, params: &Map<String, Value>) -> Outcome<Value> {
        use std::io::{Read, Seek, SeekFrom};
        require(
            ["exited", "released"].contains(&text(op, "state")),
            "The command has no final files",
            "outcome_unknown",
        )?;
        let area = params
            .get("area")
            .and_then(Value::as_str)
            .unwrap_or_default();
        require(
            ["changes", "artifacts"].contains(&area),
            "Invalid result file area",
            INVALID,
        )?;
        let path = protocol::inside(&self.opdir(identity).join(area), params.get("path"), true)?;
        let offset = match params.get("offset") {
            None => 0,
            Some(value) => integer(Some(value))
                .and_then(|offset| u64::try_from(offset).ok())
                .ok_or_else(|| HostExecError::new(INVALID, "Invalid file offset"))?,
        };
        let mut file = fs::File::open(&path)?;
        file.seek(SeekFrom::Start(offset))?;
        let mut data = Vec::with_capacity(protocol::CHUNK);
        file.take(protocol::CHUNK as u64).read_to_end(&mut data)?;
        use base64::Engine;
        Ok(
            json!({"data": base64::engine::general_purpose::STANDARD.encode(&data), "bytes": data.len()}),
        )
    }

    /// One request frame: its answer, or `None` when the frame is not an object.
    #[cfg(test)]
    pub(crate) async fn request(self: &Arc<Self>, request: &Value) -> Option<Value> {
        self.request_frame(&serde_json::to_vec(request).ok()?).await
    }

    async fn request_frame(self: &Arc<Self>, bytes: &[u8]) -> Option<Value> {
        let request: IncomingFrame = serde_json::from_slice(bytes).ok()?;
        let identity = request.id.clone().unwrap_or(Value::Null);
        Some(match self.answer(&identity, request).await {
            Ok(value) => value,
            Err(Failure::Refused(error)) => json!({"id": identity, "error": error.object()}),
            Err(Failure::Unexpected(_)) => {
                json!({"id": identity, "error": {"code": "invalid_request", "message": "Invalid host request"}})
            }
        })
    }

    async fn answer(self: &Arc<Self>, identity: &Value, request: IncomingFrame) -> Outcome<Value> {
        let id = identity
            .as_str()
            .filter(|id| !id.is_empty() && id.chars().count() <= 128)
            .ok_or_else(|| HostExecError::new(INVALID, "Invalid request ID"))?
            .to_owned();
        let token = request.token.unwrap_or(Value::String(String::new()));
        let matches = token.as_str().is_some_and(|token| {
            use subtle::ConstantTimeEq;
            bool::from(token.as_bytes().ct_eq(self.token.as_bytes()))
        });
        require(matches, "The guest host token differs", "forbidden")?;
        let method = request
            .method
            .as_ref()
            .ok_or_else(|| Failure::Unexpected("'method'".into()))?;
        let method = method
            .as_str()
            .ok_or_else(|| HostExecError::new(INVALID, "Invalid request"))?
            .to_owned();
        let raw = request
            .params
            .as_deref()
            .map_or("{}", serde_json::value::RawValue::get);
        let digest = receipt_digest(&method, raw);
        let params = match method.as_str() {
            "health" => {
                let _: HealthParams = serde_json::from_str(raw)
                    .map_err(|_| HostExecError::new(INVALID, "Invalid request"))?;
                FrameParams::Health
            }
            "service.retire" => FrameParams::Retire(
                serde_json::from_str(raw)
                    .map_err(|_| HostExecError::new(INVALID, "Invalid request"))?,
            ),
            "run" => FrameParams::Run(
                serde_json::from_str(raw)
                    .map_err(|_| HostExecError::new(INVALID, "Invalid request"))?,
            ),
            "manifest" => FrameParams::Manifest(
                serde_json::from_str(raw)
                    .map_err(|_| HostExecError::new(INVALID, "Invalid request"))?,
            ),
            "prime" => FrameParams::Prime(
                serde_json::from_str(raw)
                    .map_err(|_| HostExecError::new(INVALID, "Invalid request"))?,
            ),
            "sync.mode" => FrameParams::SyncMode(
                serde_json::from_str(raw)
                    .map_err(|_| HostExecError::new(INVALID, "Invalid request"))?,
            ),
            "sync.clone" => FrameParams::SyncClone(
                serde_json::from_str(raw)
                    .map_err(|_| HostExecError::new(INVALID, "Invalid request"))?,
            ),
            "result.manifest" => FrameParams::ResultManifest(
                serde_json::from_str(raw)
                    .map_err(|_| HostExecError::new(INVALID, "Invalid request"))?,
            ),
            _ => FrameParams::Legacy(
                serde_json::from_str(raw)
                    .map_err(|_| HostExecError::new(INVALID, "Invalid request"))?,
            ),
        };
        if method == "health" {
            let (commands, active) = {
                let inner = self.inner();
                (inner.commands.len(), inner.active.len())
            };
            return Ok(json!({"id": id, "result": {
                "protocol": PROTOCOL, "buildId": self.build_id, "pid": std::process::id(),
                "activeCommands": commands, "activeRequests": active, "busy": self.busy()}}));
        }
        require(
            !self.inner().retiring,
            "The host_exec service is retiring",
            "busy",
        )?;
        if let FrameParams::Retire(params) = &params {
            require(
                params.build_id == self.build_id && params.pid == u64::from(std::process::id()),
                "The service build or process differs",
                "id_conflict",
            )?;
            require(
                !self.busy(),
                "The host_exec service has a running command or request",
                "busy",
            )?;
            self.inner().retiring = true;
            return Ok(json!({"id": id, "result": {
                "state": "retiring", "buildId": self.build_id, "pid": std::process::id()}}));
        }
        if READ_METHODS.contains(&method.as_str()) {
            self.inner().reading += 1;
            let result = self.dispatch_frame(&method, &params).await;
            self.inner().reading -= 1;
            return Ok(json!({"id": id, "result": result?}));
        }
        let saved = {
            let db = self.db();
            let saved: Option<(String, Option<String>)> = db
                .query_row(
                    "SELECT digest,response FROM receipts WHERE id=?1",
                    [&id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if saved.is_none() {
                db.execute(
                    "INSERT INTO receipts VALUES (?1,?2,NULL)",
                    rusqlite::params![id, digest],
                )?;
            }
            saved
        };
        if let Some((saved_digest, response)) = saved {
            require(
                saved_digest == digest,
                "The request ID has different content",
                "id_conflict",
            )?;
            let running = self.inner().active.get(&id).cloned();
            if let Some(mut running) = running {
                return match running.wait_for(Option::is_some).await {
                    Ok(value) => Ok(value.clone().unwrap_or(Value::Null)),
                    Err(_) => Err(HostExecError::new(
                        "outcome_unknown",
                        "The request has no proven result; inspect its operation",
                    )
                    .into()),
                };
            }
            let response = response.ok_or_else(|| {
                HostExecError::new(
                    "outcome_unknown",
                    "The request has no proven result; inspect its operation",
                )
            })?;
            return Ok(serde_json::from_str(&response)?);
        }
        let (sender, mut receiver) = tokio::sync::watch::channel(None);
        self.inner().active.insert(id.clone(), receiver.clone());
        let service = Arc::clone(self);
        let task_id = id.clone();
        tokio::spawn(async move {
            let result = match service.dispatch_frame(&method, &params).await {
                Ok(value) => json!({"id": task_id, "result": value}),
                Err(Failure::Refused(error)) => json!({"id": task_id, "error": error.object()}),
                Err(Failure::Unexpected(message)) => {
                    json!({"id": task_id, "error": {"code": "outcome_unknown", "message": truncate(&message)}})
                }
            };
            let saved = service.db().execute(
                "UPDATE receipts SET response=?1 WHERE id=?2",
                rusqlite::params![result.to_string(), task_id],
            );
            if saved.is_ok() {
                sender.send_replace(Some(result));
            }
            // A failed save keeps the receipt without an answer: an unknown outcome.
            service.inner().active.remove(&task_id);
        });
        match receiver.wait_for(Option::is_some).await {
            Ok(value) => Ok(value.clone().unwrap_or(Value::Null)),
            Err(_) => Err(HostExecError::new(
                "outcome_unknown",
                "The request has no proven result; inspect its operation",
            )
            .into()),
        }
    }

    async fn client(self: Arc<Self>, mut stream: tokio::net::UnixStream) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        {
            let mut inner = self.inner();
            if inner.clients >= MAX_CLIENTS {
                return;
            }
            inner.clients += 1;
        }
        let served = async {
            let mut line = Vec::new();
            let mut buffer = vec![0_u8; 64 * 1024];
            let read = tokio::time::timeout(std::time::Duration::from_secs(READ_SECONDS), async {
                loop {
                    let count = stream.read(&mut buffer).await.ok()?;
                    if count == 0 {
                        return Some(());
                    }
                    if let Some(end) = buffer[..count].iter().position(|byte| *byte == b'\n') {
                        line.extend_from_slice(&buffer[..=end]);
                        return Some(());
                    }
                    line.extend_from_slice(&buffer[..count]);
                    if line.len() > protocol::FRAME_LIMIT {
                        return None;
                    }
                }
            })
            .await
            .ok()
            .flatten();
            read?;
            if line.len() > protocol::FRAME_LIMIT {
                return None;
            }
            let response = self.request_frame(&line).await?;
            let mut data = response.to_string().into_bytes();
            data.push(b'\n');
            if data.len() > protocol::FRAME_LIMIT {
                return None;
            }
            tokio::time::timeout(
                std::time::Duration::from_secs(WRITE_SECONDS),
                stream.write_all(&data),
            )
            .await
            .ok()?
            .ok()
        };
        served.await.unwrap_or_default();
        drop(stream.shutdown().await);
        let retiring = {
            let mut inner = self.inner();
            inner.clients -= 1;
            inner.retiring
        };
        if retiring {
            // The service stops after the retire answer.
            self.stopped.notify_one();
        }
    }

    /// Stop every command process of this service.
    async fn stop_children(&self) {
        let children: Vec<ChildHandle> = self.inner().children.values().cloned().collect();
        for child in children {
            command::terminate(&child).await;
        }
    }
}

/// Serve `host-exec.sock` of the VM state folder `state` until a retire request.
pub(crate) async fn serve(service: Arc<HostService>) -> std::io::Result<()> {
    let listener = bind_listener(&service)?;
    serve_listener(service, listener).await
}

fn bind_listener(service: &HostService) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    let path = service.state.join(SOCKET_FILE);
    match fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let listener = tokio::net::UnixListener::bind(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    {
        use std::io::Write;
        let mut stdout = std::io::stdout().lock();
        writeln!(stdout, "{SERVICE_READY_LINE}")?;
        stdout.flush()?;
    }
    Ok(listener)
}

async fn serve_listener(
    service: Arc<HostService>,
    listener: tokio::net::UnixListener,
) -> std::io::Result<()> {
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    connections.spawn(Arc::clone(&service).client(stream));
                }
            }
            () = service.stopped.notified() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
        }
    }
    drop(listener);
    // Open connections finish their answers within their own write limit.
    drop(
        tokio::time::timeout(std::time::Duration::from_secs(WRITE_SECONDS), async {
            while connections.join_next().await.is_some() {}
        })
        .await,
    );
    service.stop_children().await;
    Ok(())
}

/// `studio-runtime-server host-exec-service <vm-state-dir> --build <id>
/// [--browser-host <folder>]`: run the service in its own session. Returns `None` for
/// other command lines.
pub fn main(args: impl IntoIterator<Item = std::ffi::OsString>) -> Option<i32> {
    let args: Vec<String> = args
        .into_iter()
        .skip(1)
        .filter_map(|arg| arg.into_string().ok())
        .collect();
    if args.first().map(String::as_str) != Some(SUBCOMMAND) {
        return None;
    }
    let browser_host = match (args.get(4).map(String::as_str), args.get(5), args.get(6)) {
        (None, None, None) => Some(None),
        (Some(BROWSER_HOST_ARGUMENT), Some(folder), None) => Some(Some(PathBuf::from(folder))),
        _ => None,
    };
    let (Some(state), Some("--build"), Some(build), Some(browser_host)) = (
        args.get(1),
        args.get(2).map(String::as_str),
        args.get(3),
        browser_host,
    ) else {
        eprintln!(
            "usage: studio-runtime-server {SUBCOMMAND} <vm-state-dir> --build <id> [{BROWSER_HOST_ARGUMENT} <folder>]"
        );
        return Some(2);
    };
    // A running Mac command survives a backend restart: the service leaves the
    // backend's session and process group.
    if let Err(error) = rustix::process::setsid() {
        eprintln!("host_exec service: setsid failed: {error}");
        return Some(1);
    }
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("host_exec service: {error}");
            return Some(1);
        }
    };
    let result = runtime.block_on(async {
        let service = Arc::new(
            HostService::open(Path::new(state), build.clone())?.with_browser_host(browser_host),
        );
        serve(service).await
    });
    match result {
        Ok(()) => Some(0),
        Err(error) => {
            eprintln!("host_exec service: {error}");
            Some(1)
        }
    }
}

#[cfg(test)]
mod tests;
