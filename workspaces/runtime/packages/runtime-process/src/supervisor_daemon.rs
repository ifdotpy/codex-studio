//! Standalone protocol-v1 supervisor. One lease, bounded connections and owned children.
use super::supervisor_child::OwnedChild;
use super::supervisor_identity::{RETRY, free_space, start_time, stop_verified, valid_start};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use studio_runtime_providers::supervisor_v1::{
    Command, HANDLE_LIMIT, Handshake, Ownership, PROTOCOL, Request, WriteDecision,
};
use studio_runtime_store::supervisor_journal::{
    JournalError, StoredHandle, SupervisorJournal, launch_signature,
};

const MAX_REQUEST_BYTES: u64 = crate::transport::MAX_FRAME_BYTES as u64;
const MAX_CONNECTIONS: usize = 512;
const MAX_CHILDREN: usize = 128;
const OPERATOR_CLOSED: &str = "Closed by operator after verified test/unknown-client ownership";
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("Invalid supervisor request")]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Protocol(#[from] studio_runtime_providers::supervisor_v1::Refusal),
    #[error(
        "Supervisor handle {handle:?} is active with PID {pid} and different launch settings; wait for it to exit or use the operator CLI to close a verified test/unknown handle."
    )]
    ActiveChild { handle: String, pid: u32 },
    #[error("{0}")]
    Refused(&'static str),
}
pub type Result<T> = std::result::Result<T, Error>;
pub(super) fn lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    mutex
        .lock()
        .map_err(|_| Error::Refused("Supervisor lock poisoned"))
}
pub(super) fn now() -> Result<f64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Refused("Clock before Unix epoch"))?
        .as_secs_f64())
}

struct Supervisor {
    root: PathBuf,
    journal: Arc<SupervisorJournal>,
    children: Mutex<BTreeMap<String, Arc<OwnedChild>>>,
    ownership: Mutex<Ownership>,
    recovery: Mutex<Value>,
    connections: AtomicUsize,
}
impl Supervisor {
    fn saved(&self, handle: &str) -> Result<StoredHandle> {
        self.journal
            .handle(handle)?
            .ok_or(Error::Refused("Unknown supervisor handle"))
    }
    fn handle(&self, command: Command, operator: bool) -> Result<Value> {
        command.validate()?;
        match command {
            Command::Health => {
                let children = lock(&self.children)?;
                let handles=self.journal.handles(true)?.into_iter().map(|row| {
                    let child=children.get(&row.id);
                    Ok(json!({"id":row.id,"pid":row.pid,"signature":row.signature,"startTime":row.start_time,"jobIdentity":null,"sequence":row.snapshot.sequence(),"acknowledged":row.snapshot.acknowledged(),"bufferedBytes":row.buffered,"backpressure":child.is_some_and(|c|c.paused.load(Ordering::Acquire)),"stdoutReaderAlive":child.is_some_and(|c|c.stdout_alive.load(Ordering::Acquire)),"stdoutReaderError":child.map(|c|lock(&c.stdout_error).map(|e|e.clone())).transpose()?.flatten(),"persistenceErrors":child.map(|c|lock(&c.errors).map(|e|e.clone())).transpose()?.unwrap_or_default()}))
                }).collect::<Result<Vec<_>>>()?;
                Ok(
                    json!({"protocol":PROTOCOL,"stateDir":self.root,"handles":handles,"journalLimitBytes":HANDLE_LIMIT,"outputLimitBytesPerHandle":HANDLE_LIMIT,"durability":"sqlite-full-sync-per-request","recovery":lock(&self.recovery)?.clone()}),
                )
            }
            Command::FinishFallback => {
                let mut recovery = lock(&self.recovery)?;
                if !recovery["blocked"].is_null() {
                    return Err(Error::Refused(
                        "Cannot finish supervisor fallback while child ownership is uncertain",
                    ));
                }
                let clean = json!({"degraded":false,"fallbackReady":false,"blocked":null});
                self.journal.save_recovery(&clean)?;
                *recovery = clean;
                Ok(json!({"finished":true}))
            }
            Command::Open {
                handle,
                command,
                env,
                cwd,
            } => {
                let signature = launch_signature(&command, &env, cwd.as_deref());
                let mut children = lock(&self.children)?;
                let mut stopped = false;
                if let Some(child) = children.get(&handle) {
                    if child.code()?.is_none() {
                        if child.signature != signature {
                            return Err(Error::ActiveChild {
                                handle,
                                pid: child.pid,
                            });
                        }
                        return self.open_result(&handle, child, true);
                    }
                    child.stopping.store(true, Ordering::Release);
                    self.journal.close(&handle, "Native child exited", now()?)?;
                    children.remove(&handle);
                    stopped = true;
                }
                if let Some(saved) = self.journal.handle(&handle)? {
                    if !lock(&self.recovery)?["blocked"].is_null()
                        || (!stopped && saved.closed_at.is_none())
                    {
                        return Err(Error::Refused(
                            "Supervisor handle is orphaned; native outcome remains unknown",
                        ));
                    }
                    if !stopped && saved.signature != signature {
                        let proof = saved.recovery_proof
                            || saved.closed_reason.as_deref() == Some(OPERATOR_CLOSED);
                        if !proof
                            || !saved.start_time.as_deref().is_some_and(valid_start)
                            || start_time(saved.pid, false)? == saved.start_time
                        {
                            return Err(Error::Refused(
                                "Supervisor handle is orphaned; native outcome remains unknown",
                            ));
                        }
                    }
                }
                if children.len() >= MAX_CHILDREN {
                    return Err(Error::Refused("Supervisor child capacity reached"));
                }
                free_space(&self.root, 1024 * 1024)?;
                self.journal.reserve(&handle, &signature, now()?)?;
                let child = OwnedChild::spawn(
                    self.journal.clone(),
                    &self.root,
                    &handle,
                    signature,
                    &command,
                    &env,
                    cwd.as_deref(),
                )?;
                children.insert(handle.clone(), child.clone());
                self.open_result(&handle, &child, false)
            }
            Command::AdminCloseHandle {
                handle,
                expected_pid,
                expected_start_time,
                expected_signature,
            } => {
                if !operator {
                    return Err(Error::Refused(
                        "adminCloseHandle is available only through the operator CLI",
                    ));
                }
                let mut children = lock(&self.children)?;
                let row = self.saved(&handle)?;
                if !children.contains_key(&handle) {
                    if row.closed_at.is_some()
                        || row.pid != expected_pid
                        || row.signature != expected_signature
                        || row.start_time.as_deref().unwrap_or("") != expected_start_time
                    {
                        return Err(Error::Refused(
                            "Operator close refused: handle identity changed; refresh status and verify ownership",
                        ));
                    }
                    if let Some(expected) = &row.start_time {
                        match start_time(row.pid, false)? {
                            None => {}
                            Some(actual) if actual == *expected => {
                                stop_verified(
                                    row.pid,
                                    row.pgid
                                        .ok_or(Error::Refused("Missing child process group"))?,
                                    expected,
                                )?;
                            }
                            Some(_) => {
                                return Err(Error::Refused(
                                    "Operator close refused: child identity changed",
                                ));
                            }
                        }
                    }
                    // Exact operator acknowledgement clears unknown ownership without
                    // inventing a process identity or resending any accepted input.
                    let unresolved = self
                        .journal
                        .handles(true)?
                        .iter()
                        .any(|h| h.id != handle && !children.contains_key(&h.id));
                    let mut recovery = lock(&self.recovery)?;
                    let mut clean = recovery.clone();
                    if !unresolved {
                        clean["blocked"] = Value::Null;
                        clean["fallbackReady"] = clean["degraded"].clone();
                    }
                    self.journal.close_unknown(&row, now()?, &clean)?;
                    *recovery = clean;
                    return Ok(json!({"closed":true,"handle":handle,"pid":expected_pid}));
                }
                let child = children
                    .get(&handle)
                    .ok_or(Error::Refused("Unknown supervisor handle"))?;
                let _write = lock(&child.write_lock)?;
                if row.closed_at.is_some()
                    || row.pid != expected_pid
                    || row.signature != expected_signature
                    || row.start_time.as_deref() != Some(&expected_start_time)
                    || start_time(expected_pid, false)?.as_deref() != Some(&expected_start_time)
                    || child.pid != expected_pid
                    || child.start != expected_start_time
                {
                    return Err(Error::Refused(
                        "Operator close refused: handle identity changed; refresh status and verify ownership",
                    ));
                }
                stop_verified(
                    expected_pid,
                    row.pgid
                        .ok_or(Error::Refused("Missing child process group"))?,
                    &expected_start_time,
                )?;
                child.stopping.store(true, Ordering::Release);
                child.reap()?;
                self.journal.close(&handle, OPERATOR_CLOSED, now()?)?;
                drop(_write);
                children.remove(&handle);
                Ok(json!({"closed":true,"handle":handle,"pid":expected_pid}))
            }
            other => self.child_action(other),
        }
    }
    fn open_result(&self, handle: &str, child: &OwnedChild, resumed: bool) -> Result<Value> {
        let row = self.saved(handle)?;
        Ok(
            json!({"resumed":resumed,"initResult":row.init_result,"acknowledged":row.snapshot.acknowledged(),"sequence":row.snapshot.sequence(),"generation":row.snapshot.generation(),"returnCode":child.code()?,"pid":child.pid}),
        )
    }
    fn child_action(&self, command: Command) -> Result<Value> {
        let handle = command
            .handle()
            .ok_or(Error::Refused("Unknown supervisor action"))?
            .to_owned();
        let mut children = lock(&self.children)?;
        let child = children
            .get(&handle)
            .cloned()
            .ok_or(Error::Refused("Unknown supervisor handle"))?;
        match command {
            Command::Write {
                operation_id,
                native_id,
                message,
                ..
            } => {
                let _write = lock(&child.write_lock)?;
                let started = Instant::now();
                free_space(&self.root, 1024 * 1024)?;
                let decision = self.journal.accept(
                    &handle,
                    &operation_id,
                    native_id,
                    &Value::Object(message),
                    now()?,
                )?;
                let (duplicate, remote_id) = match decision {
                    WriteDecision::Duplicate { remote_id } => (true, remote_id),
                    WriteDecision::Accept {
                        remote_id,
                        outbound,
                        ..
                    } => {
                        child.write(&outbound)?;
                        (false, remote_id)
                    }
                };
                Ok(
                    json!({"accepted":true,"duplicate":duplicate,"remoteId":remote_id,"durableMs":started.elapsed().as_secs_f64()*1000.0}),
                )
            }
            Command::OperationStatus { operation_id, .. } => {
                Ok(self.journal.operation_status(&handle, &operation_id)?)
            }
            Command::Ack { sequence, .. } => {
                self.journal.ack(&handle, sequence)?;
                Ok(json!({"acknowledged":sequence}))
            }
            Command::Next { cursor, .. } => {
                // Sample completion before querying events: a final committed tail cannot be hidden.
                let stdout = child.stdout_alive.load(Ordering::Acquire);
                let stderr = child.stderr_alive.load(Ordering::Acquire);
                let events = self.journal.events(&handle, cursor, true)?;
                Ok(
                    json!({"event":events["events"].get(0),"returnCode":child.code()?,"backpressure":child.paused.load(Ordering::Acquire),"stdoutReaderAlive":stdout,"stderrReaderAlive":stderr}),
                )
            }
            Command::Replay { cursor, .. } => {
                let mut result = self.journal.events(&handle, cursor, false)?;
                result["backpressure"] = json!(child.paused.load(Ordering::Acquire));
                Ok(result)
            }
            Command::Status { .. } => {
                let row = self.saved(&handle)?;
                Ok(
                    json!({"sequence":row.snapshot.sequence(),"acknowledged":row.snapshot.acknowledged(),"backpressure":child.paused.load(Ordering::Acquire),"pid":child.pid,"returnCode":child.code()?}),
                )
            }
            Command::Detach { .. } => Ok(json!({"detached":true})),
            Command::RetireCommand { generation, .. } => {
                let _write = lock(&child.write_lock)?;
                self.journal.retire(
                    &handle,
                    generation,
                    child.code()?.is_some(),
                    !child.stdout_alive.load(Ordering::Acquire)
                        && !child.stderr_alive.load(Ordering::Acquire),
                )?;
                children.remove(&handle);
                Ok(json!({"retired":true}))
            }
            _ => Err(Error::Refused("Unknown supervisor action")),
        }
    }
    fn recover(&self) -> Result<()> {
        let mut recovery = json!({"degraded":false,"fallbackReady":false,"blocked":null});
        for row in self.journal.handles(true)? {
            let (outcome, detail) =
                if let (Some(expected), Some(pgid)) = (&row.start_time, row.pgid) {
                    match start_time(row.pid, false)? {
                        None => (
                            "already-exited",
                            "Native child was no longer running when the supervisor recovered",
                        ),
                        Some(actual) if actual != *expected => (
                            "identity-mismatch",
                            "PID belongs to a different process start time",
                        ),
                        Some(_) => {
                            recovery["degraded"] = json!(true);
                            recovery["notice"] = json!(
                                "The process supervisor restarted; native work is using recovery."
                            );
                            match stop_verified(row.pid, pgid, expected) {
                                Ok(outcome) => (outcome, "verified process group terminated"),
                                Err(_) => {
                                    recovery["blocked"] =
                                        json!("Could not terminate verified child process group");
                                    (
                                        "termination-failed",
                                        "Could not terminate verified child process group",
                                    )
                                }
                            }
                        }
                    }
                } else {
                    recovery["blocked"] =
                        json!("A native child is missing its recorded PID and start time");
                    (
                        "missing-identity",
                        "Native child PID and start time were not recorded",
                    )
                };
            self.journal.recovered(&row, outcome, detail, now()?)?;
        }
        recovery["fallbackReady"] =
            json!(recovery["degraded"] == true && recovery["blocked"].is_null());
        self.journal.save_recovery(&recovery)?;
        *lock(&self.recovery)? = recovery;
        Ok(())
    }
}

fn read_frame(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut bytes = Vec::new();
    let n = reader
        .take(MAX_REQUEST_BYTES + 1)
        .read_until(b'\n', &mut bytes)?;
    if n == 0 {
        return Ok(None);
    }
    if n as u64 > MAX_REQUEST_BYTES || bytes.last() != Some(&b'\n') {
        return Err(Error::Refused(
            "Supervisor frame exceeds the size limit or is incomplete",
        ));
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}
fn write_frame(writer: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}
fn connection(supervisor: Arc<Supervisor>, mut socket: UnixStream) {
    let mut admitted = None;
    let result = (|| -> Result<()> {
        socket.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut reader = BufReader::new(socket.try_clone()?);
        let first: Handshake = serde_json::from_value(
            read_frame(&mut reader)?.ok_or(Error::Refused("Missing supervisor handshake"))?,
        )?;
        admitted =
            lock(&supervisor.ownership)?.attach(&first, &supervisor.root.to_string_lossy())?;
        write_frame(&mut socket, &json!({"ok":true}))?;
        while let Some(raw) = read_frame(&mut reader)? {
            let id = raw.get("requestId").cloned();
            let response = match serde_json::from_value::<Request>(raw)
                .map_err(Error::from)
                .and_then(|r| supervisor.handle(r.command, first.operator))
            {
                Ok(value) => json!({"requestId":id,"result":value}),
                Err(error) => {
                    json!({"requestId":id,"error":error.to_string().chars().take(500).collect::<String>()})
                }
            };
            write_frame(&mut socket, &response)?;
        }
        Ok(())
    })();
    if let Err(error) = result
        && write_frame(&mut socket, &json!({"error":error.to_string()})).is_err()
    {
        // Disconnected client; durable receipts remain.
    }
    if let Ok(mut ownership) = supervisor.ownership.lock() {
        ownership.detach(admitted.as_deref());
    }
    supervisor.connections.fetch_sub(1, Ordering::AcqRel);
}

/// Serve only the explicitly selected directory. Does not install or connect to another service.
pub fn serve(root: &Path, wait_for_lease: bool) -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Err(Error::Refused(
            "Not implemented: requires native supervisor process identity for this platform",
        ));
    }
    fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let mut lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(root.join("supervisor.lock"))?;
    let mut announced = false;
    loop {
        match lease.try_lock() {
            Ok(()) => break,
            Err(_) if !wait_for_lease => {
                return Err(Error::Refused(
                    "Another supervisor owns this state directory",
                ));
            }
            Err(_) => {
                if !announced {
                    let mut text = String::new();
                    lease.seek(SeekFrom::Start(0))?;
                    Read::by_ref(&mut lease)
                        .take(4096)
                        .read_to_string(&mut text)?;
                    let value: Value = serde_json::from_str(&text)?;
                    eprintln!("supervisor waiting for owner {}", value["pid"]);
                    announced = true;
                }
                thread::sleep(RETRY);
            }
        }
    }
    let mut previous = String::new();
    lease.seek(SeekFrom::Start(0))?;
    Read::by_ref(&mut lease)
        .take(4096)
        .read_to_string(&mut previous)?;
    if !previous.trim().is_empty() {
        let identity: Value = serde_json::from_str(&previous)?;
        let pid = identity["pid"]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .ok_or(Error::Refused(
                "The previous supervisor lease identity is invalid",
            ))?;
        let expected = identity["startTime"].as_str().ok_or(Error::Refused(
            "The previous supervisor lease identity is invalid",
        ))?;
        if start_time(pid, false)?.as_deref() == Some(expected) {
            return Err(Error::Refused(
                "The previous supervisor process is still alive",
            ));
        }
    }
    free_space(&root, HANDLE_LIMIT + 16 * 1024 * 1024)?;
    let journal = Arc::new(SupervisorJournal::open(&root)?);
    fs::set_permissions(
        root.join("supervisor.sqlite3"),
        fs::Permissions::from_mode(0o600),
    )?;
    let supervisor = Arc::new(Supervisor {
        root: root.clone(),
        journal,
        children: Mutex::new(BTreeMap::new()),
        ownership: Mutex::new(Ownership::default()),
        recovery: Mutex::new(json!({"degraded":false,"fallbackReady":false,"blocked":null})),
        connections: AtomicUsize::new(0),
    });
    supervisor.recover()?;
    let own_start = start_time(std::process::id(), false)?
        .ok_or(Error::Refused("Cannot prove supervisor identity"))?;
    lease.seek(SeekFrom::Start(0))?;
    lease.set_len(0)?;
    write_frame(
        &mut lease,
        &json!({"pid":std::process::id(),"startTime":own_start}),
    )?;
    lease.sync_all()?;
    let socket = root.join("supervisor.sock");
    match fs::remove_file(&socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    for socket in listener.incoming() {
        let mut socket = socket?;
        if supervisor.connections.fetch_add(1, Ordering::AcqRel) >= MAX_CONNECTIONS {
            supervisor.connections.fetch_sub(1, Ordering::AcqRel);
            socket.set_write_timeout(Some(Duration::from_secs(1)))?;
            if write_frame(
                &mut socket,
                &json!({"error":"Supervisor connection capacity reached"}),
            )
            .is_err()
            { /* The rejected connection may already have closed. */ }
            continue;
        }
        let copy = supervisor.clone();
        thread::spawn(move || connection(copy, socket));
    }
    drop(lease);
    Ok(())
}
