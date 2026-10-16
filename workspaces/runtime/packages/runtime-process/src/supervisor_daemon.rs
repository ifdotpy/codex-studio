//! Standalone supervisor. One lease, bounded connections and owned children.
use super::supervisor_child::OwnedChild;
use super::supervisor_identity::{
    RETRY, free_space, inspect_start, process_gone_error, start_time, stop_verified, valid_start,
    verify_group,
};
use crate::{
    NextBatchRequestEnvelope, NextBatchResult, SUPERVISOR_NEXT_BATCH_MAX_EVENTS,
    SUPERVISOR_PROTOCOL_V2, SupervisorEventV2, SupervisorHandshakeV2,
};
use serde::Serialize;
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
    Command, HANDLE_LIMIT, Handshake, Ownership, PROTOCOL, Request, StartMatch, WriteDecision,
};
use studio_runtime_store::supervisor_journal::{
    JournalError, StoredHandle, SupervisorJournal, launch_signature,
};

const MAX_REQUEST_BYTES: u64 = crate::transport::MAX_FRAME_BYTES as u64;
const MAX_CONNECTIONS: usize = 512;
const MAX_CHILDREN: usize = 128;
const FRAME_BUDGET: Duration = Duration::from_secs(10);
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
    children: Mutex<BTreeMap<String, Arc<ChildSlot>>>,
    active_children: AtomicUsize,
    ownership: Mutex<Ownership>,
    recovery: Mutex<Value>,
    connections: AtomicUsize,
}
/// The map is only an index. This lock serializes lifecycle and requests for
/// one handle while keeping that handle's blocking journal/process work away
/// from unrelated handles.
struct ChildSlot {
    operation: Mutex<()>,
    child: Mutex<Option<Arc<OwnedChild>>>,
    notify: Arc<tokio::sync::Notify>,
    openings: AtomicUsize,
    opening_generation: AtomicUsize,
}

struct OpeningGuard(Arc<ChildSlot>);

impl Drop for OpeningGuard {
    fn drop(&mut self) {
        self.0.openings.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Supervisor {
    fn reserve_child_slot(&self) -> bool {
        let mut active = self.active_children.load(Ordering::Acquire);
        loop {
            if active >= MAX_CHILDREN {
                return false;
            }
            match self.active_children.compare_exchange_weak(
                active,
                active + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => active = observed,
            }
        }
    }

    fn child_slot(&self, handle: &str, create: bool) -> Result<Option<Arc<ChildSlot>>> {
        let mut children = lock(&self.children)?;
        if let Some(slot) = children.get(handle) {
            if create {
                slot.openings.fetch_add(1, Ordering::AcqRel);
                slot.opening_generation.fetch_add(1, Ordering::AcqRel);
            }
            return Ok(Some(slot.clone()));
        }
        if !create {
            return Ok(None);
        }
        let slot = Arc::new(ChildSlot {
            operation: Mutex::new(()),
            child: Mutex::new(None),
            notify: Arc::new(tokio::sync::Notify::new()),
            openings: AtomicUsize::new(usize::from(create)),
            opening_generation: AtomicUsize::new(usize::from(create)),
        });
        children.insert(handle.to_owned(), slot.clone());
        Ok(Some(slot))
    }

    fn saved(&self, handle: &str) -> Result<StoredHandle> {
        self.journal
            .handle(handle)?
            .ok_or(Error::Refused("Unknown supervisor handle"))
    }
    fn handle(&self, command: Command, operator: bool) -> Result<Value> {
        command.validate()?;
        match command {
            Command::Health => {
                let mut handles = Vec::new();
                // Open inserts its slot before reserving the durable row. Take
                // this snapshot first so every row reserved by an overlapping
                // Open has a slot here, then use `openings` to hide it until
                // child installation finishes.
                let rows = self.journal.handles(true)?;
                let children = lock(&self.children)?.clone();
                for row in rows {
                    let slot = children.get(&row.id);
                    let opening_generation =
                        slot.map(|slot| slot.opening_generation.load(Ordering::Acquire));
                    // Open reserves the durable row before the child is installed.
                    // Hide that transient row until all overlapping Open calls finish.
                    if slot.is_some_and(|slot| slot.openings.load(Ordering::Acquire) != 0) {
                        continue;
                    }
                    let child = slot
                        .map(|slot| lock(&slot.child).map(|child| child.clone()))
                        .transpose()?
                        .flatten();
                    // The generation also catches an Open that starts and
                    // finishes between the two in-flight checks.
                    if slot.is_some_and(|slot| {
                        slot.openings.load(Ordering::Acquire) != 0
                            || Some(slot.opening_generation.load(Ordering::Acquire))
                                != opening_generation
                    }) {
                        continue;
                    }
                    handles.push(json!({"id":row.id,"pid":row.pid,"signature":row.signature,"startTime":row.start_time,"jobIdentity":null,"sequence":row.snapshot.sequence(),"acknowledged":row.snapshot.acknowledged(),"bufferedBytes":row.buffered,"backpressure":child.as_ref().is_some_and(|c|c.paused.load(Ordering::Acquire)),"stdoutReaderAlive":child.as_ref().is_some_and(|c|c.stdout_alive.load(Ordering::Acquire)),"stdoutReaderError":child.as_ref().map(|c|lock(&c.stdout_error).map(|e|e.clone())).transpose()?.flatten(),"persistenceErrors":child.as_ref().map(|c|lock(&c.errors).map(|e|e.clone())).transpose()?.unwrap_or_default()}));
                }
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
            request @ Command::Open { .. } => {
                let Command::Open {
                    handle,
                    command,
                    env,
                    cwd,
                } = &request
                else {
                    return Err(Error::Refused("Invalid supervisor command"));
                };
                let signature = launch_signature(command, env, cwd.as_deref());
                let slot = self
                    .child_slot(handle, true)?
                    .ok_or(Error::Refused("Missing child slot"))?;
                let _opening = OpeningGuard(slot.clone());
                let _operation = lock(&slot.operation)?;
                let mut stopped = false;
                let existing = lock(&slot.child)?.clone();
                if let Some(child) = existing {
                    if child.code()?.is_none() {
                        if child.signature != signature {
                            return Err(Error::ActiveChild {
                                handle: handle.to_owned(),
                                pid: child.pid,
                            });
                        }
                        return self.open_result(handle, &child, true);
                    }
                    if child.durable_exit_sequence()?.is_none() {
                        return Err(Error::Refused(
                            "Native child exit event is not yet durably recorded",
                        ));
                    }
                    child.stopping.store(true, Ordering::Release);
                    self.journal.close(handle, "Native child exited", now()?)?;
                    *lock(&slot.child)? = None;
                    self.active_children.fetch_sub(1, Ordering::AcqRel);
                    stopped = true;
                }
                if let Some(saved) = self.journal.handle(handle)? {
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
                            || matches!(
                                inspect_start(
                                    saved.pid,
                                    saved.start_time.as_deref().unwrap_or("")
                                )?
                                .0,
                                StartMatch::Exact | StartMatch::Legacy
                            )
                        {
                            return Err(Error::Refused(
                                "Supervisor handle is orphaned; native outcome remains unknown",
                            ));
                        }
                    }
                }
                if !self.reserve_child_slot() {
                    return Err(Error::Refused("Supervisor child capacity reached"));
                }
                let reserved = (|| {
                    free_space(&self.root, 1024 * 1024)?;
                    self.journal.reserve(handle, &signature, now()?)?;
                    OwnedChild::spawn(
                        self.journal.clone(),
                        &self.root,
                        &request,
                        slot.notify.clone(),
                        signature,
                    )
                })();
                let child = match reserved {
                    Ok(child) => child,
                    Err(error) => {
                        self.active_children.fetch_sub(1, Ordering::AcqRel);
                        return Err(error);
                    }
                };
                *lock(&slot.child)? = Some(child.clone());
                self.open_result(handle, &child, false)
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
                let slot = self.child_slot(&handle, false)?;
                let _operation = slot
                    .as_ref()
                    .map(|slot| lock(&slot.operation))
                    .transpose()?;
                let active_child = slot
                    .as_ref()
                    .map(|slot| lock(&slot.child).map(|child| child.clone()))
                    .transpose()?
                    .flatten();
                let row = self.saved(&handle)?;
                if active_child.is_none() {
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
                        match inspect_start(row.pid, expected)?.0 {
                            StartMatch::Gone => {}
                            StartMatch::Exact | StartMatch::Legacy => {
                                if let Some(verified) = self.verified_start(&row)? {
                                    stop_verified(
                                        row.pid,
                                        row.pgid
                                            .ok_or(Error::Refused("Missing child process group"))?,
                                        &verified,
                                    )?;
                                }
                            }
                            StartMatch::Different => {
                                return Err(Error::Refused(
                                    "Operator close refused: child identity changed",
                                ));
                            }
                        }
                    }
                    // Exact operator acknowledgement clears unknown ownership without
                    // inventing a process identity or resending any accepted input.
                    let unresolved = self.journal.handles(true)?.iter().any(|h| {
                        if h.id == handle {
                            return false;
                        }
                        match self.child_slot(&h.id, false) {
                            Ok(Some(slot)) => {
                                lock(&slot.child).map_or(true, |child| child.is_none())
                            }
                            Ok(None) | Err(_) => true,
                        }
                    });
                    let mut recovery = lock(&self.recovery)?;
                    let mut clean = recovery.clone();
                    if !unresolved {
                        clean["blocked"] = Value::Null;
                        clean["fallbackReady"] = clean["degraded"].clone();
                    }
                    self.journal
                        .close_unknown(&self.saved(&row.id)?, now()?, &clean)?;
                    *recovery = clean;
                    return Ok(json!({"closed":true,"handle":handle,"pid":expected_pid}));
                }
                let child = active_child.ok_or(Error::Refused("Unknown supervisor handle"))?;
                let _write = lock(&child.write_lock)?;
                let actual_start = start_time(expected_pid, false)?;
                if row.closed_at.is_some()
                    || row.pid != expected_pid
                    || row.signature != expected_signature
                    || row.start_time.as_deref() != Some(&expected_start_time)
                    || actual_start
                        .as_deref()
                        .is_some_and(|actual| actual != expected_start_time)
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
                if let Some(slot) = slot.as_ref() {
                    *lock(&slot.child)? = None;
                    self.active_children.fetch_sub(1, Ordering::AcqRel);
                }
                Ok(json!({"closed":true,"handle":handle,"pid":expected_pid}))
            }
            other => self.child_action(other),
        }
    }
    fn next_batch_ready(
        &self,
        request: &crate::NextBatchRequest,
    ) -> Result<Option<(NextBatchResult, Option<u64>)>> {
        if request.limit == 0 {
            return Err(Error::Refused(
                "Supervisor event batch limit must be positive",
            ));
        }
        let handle = &request.handle.0;
        let slot = self
            .child_slot(handle, false)?
            .ok_or(Error::Refused("Unknown supervisor handle"))?;
        let child = lock(&slot.child)?
            .clone()
            .ok_or(Error::Refused("Unknown supervisor handle"))?;
        if child.generation != request.generation.0 {
            return Err(Error::Refused("Supervisor handle generation changed"));
        }
        let rows = self.journal.events_v2(
            handle,
            request.cursor.0,
            request.generation.0,
            request.limit.min(SUPERVISOR_NEXT_BATCH_MAX_EVENTS),
            crate::transport::MAX_FRAME_BYTES,
        )?;
        let events = rows
            .into_iter()
            .map(|row| {
                Ok(SupervisorEventV2 {
                    sequence: crate::EventCursor(row.sequence),
                    generation: crate::Generation(row.generation),
                    kind: serde_json::from_value(json!(row.kind))?,
                    payload: row.payload,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let exit_sequence = child.durable_exit_sequence()?;
        let terminal = exit_delivered(
            request.cursor.0,
            events.last().map(|event| event.sequence.0),
            exit_sequence,
        );
        let return_code = if terminal { child.code()? } else { None };
        let stdout_reader_alive = child.stdout_alive.load(Ordering::Acquire);
        let stderr_reader_alive = child.stderr_alive.load(Ordering::Acquire);
        if events.is_empty() && !terminal {
            return Ok(None);
        }
        Ok(Some((
            NextBatchResult {
                events,
                return_code,
                backpressure: child.paused.load(Ordering::Acquire),
                stdout_reader_alive,
                stderr_reader_alive,
            },
            exit_sequence,
        )))
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
        let slot = self
            .child_slot(&handle, false)?
            .ok_or(Error::Refused("Unknown supervisor handle"))?;
        let _operation = lock(&slot.operation)?;
        let child = lock(&slot.child)?
            .clone()
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
                // Sample readers before querying so the final committed tail cannot be hidden.
                let stdout = child.stdout_alive.load(Ordering::Acquire);
                let stderr = child.stderr_alive.load(Ordering::Acquire);
                let events = self.journal.events(&handle, cursor, true)?;
                let event = events["events"].get(0);
                let last_event_cursor = event
                    .and_then(|event| event.get("sequence"))
                    .and_then(Value::as_u64);
                let return_code =
                    if exit_delivered(cursor, last_event_cursor, child.durable_exit_sequence()?) {
                        child.code()?
                    } else {
                        None
                    };
                Ok(
                    json!({"event":events["events"].get(0),"returnCode":return_code,"backpressure":child.paused.load(Ordering::Acquire),"stdoutReaderAlive":stdout,"stderrReaderAlive":stderr}),
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
                let exit_event_durable = child.durable_exit_sequence()?.is_some();
                self.journal
                    .retire(&handle, generation, exit_event_durable, exit_event_durable)?;
                *lock(&slot.child)? = None;
                self.active_children.fetch_sub(1, Ordering::AcqRel);
                Ok(json!({"retired":true}))
            }
            _ => Err(Error::Refused("Unknown supervisor action")),
        }
    }
    fn verified_start(&self, row: &StoredHandle) -> Result<Option<String>> {
        let expected = row
            .start_time
            .as_deref()
            .ok_or(Error::Refused("Missing child start identity"))?;
        let pgid = row
            .pgid
            .ok_or(Error::Refused("Missing child process group"))?;
        let (kind, actual) = inspect_start(row.pid, expected)?;
        let Some(actual) = actual else {
            return Ok(None);
        };
        match kind {
            StartMatch::Exact => {}
            StartMatch::Legacy => {}
            StartMatch::Gone => return Ok(None),
            _ => return Err(Error::Refused("Child identity changed before signal")),
        }
        match verify_group(row.pid, pgid) {
            Ok(_) => {}
            Err(Error::Io(error))
                if process_gone_error(&error) && start_time(row.pid, false)?.is_none() =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        match start_time(row.pid, false)? {
            None => return Ok(None),
            Some(current) if current == actual => {}
            Some(_) => return Err(Error::Refused("Child identity changed before signal")),
        }
        if kind == StartMatch::Legacy {
            self.journal.upgrade_identity(row, &actual)?;
        }
        Ok(Some(actual))
    }
    fn recover(&self) -> Result<()> {
        let mut recovery = json!({"degraded":false,"fallbackReady":false,"blocked":null});
        for row in self.journal.handles(true)? {
            let (outcome, detail) = if let (Some(expected), Some(pgid)) =
                (&row.start_time, row.pgid)
            {
                match inspect_start(row.pid, expected)?.0 {
                    StartMatch::Gone => (
                        "already-exited",
                        "Native child was no longer running when the supervisor recovered",
                    ),
                    StartMatch::Different => (
                        "identity-mismatch",
                        "PID belongs to a different process start time",
                    ),
                    StartMatch::Exact | StartMatch::Legacy => {
                        recovery["degraded"] = json!(true);
                        recovery["notice"] = json!(
                            "The process supervisor restarted; native work is using recovery."
                        );
                        match self.verified_start(&row) {
                            Ok(None) => (
                                "already-exited",
                                "Native child exited while its identity was being verified",
                            ),
                            Ok(Some(verified)) => match stop_verified(row.pid, pgid, &verified) {
                                Ok(outcome) => (outcome, "verified process group terminated"),
                                Err(_) => {
                                    recovery["blocked"] =
                                        json!("Could not terminate verified child process group");
                                    (
                                        "termination-failed",
                                        "Could not terminate verified child process group",
                                    )
                                }
                            },
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
            self.journal
                .recovered(&self.saved(&row.id)?, outcome, detail, now()?)?;
        }
        recovery["fallbackReady"] =
            json!(recovery["degraded"] == true && recovery["blocked"].is_null());
        self.journal.save_recovery(&recovery)?;
        *lock(&self.recovery)? = recovery;
        Ok(())
    }
}

fn read_frame(
    reader: &mut BufReader<UnixStream>,
    mut deadline: Option<Instant>,
    budget: Duration,
) -> Result<Option<Value>> {
    let mut bytes = Vec::new();
    loop {
        let remaining = deadline
            .map(|at| {
                at.checked_duration_since(Instant::now())
                    .filter(|left| !left.is_zero())
                    .ok_or(Error::Refused(
                        "Supervisor handshake or frame deadline exceeded",
                    ))
            })
            .transpose()?;
        reader.get_ref().set_read_timeout(remaining)?;
        let chunk = match reader.fill_buf() {
            Ok(chunk) => chunk,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(Error::Refused(
                    "Supervisor handshake or frame deadline exceeded",
                ));
            }
            Err(e) => return Err(e.into()),
        };
        if deadline.is_some_and(|at| Instant::now() >= at) {
            return Err(Error::Refused(
                "Supervisor handshake or frame deadline exceeded",
            ));
        }
        if chunk.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(Error::Refused(
                    "Supervisor frame exceeds the size limit or is incomplete",
                ))
            };
        }
        // An established idle connection has no deadline. Once its first bytes arrive,
        // subsequent reads share one absolute deadline, so trickling cannot extend it.
        deadline.get_or_insert_with(|| Instant::now() + budget);
        let end = chunk.iter().position(|b| *b == b'\n').map(|i| i + 1);
        let count = end.unwrap_or(chunk.len());
        if (bytes.len() + count) as u64 > MAX_REQUEST_BYTES {
            return Err(Error::Refused(
                "Supervisor frame exceeds the size limit or is incomplete",
            ));
        }
        bytes.extend_from_slice(&chunk[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(Some(serde_json::from_slice(&bytes)?));
        }
    }
}
fn write_frame(writer: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}
fn connection(supervisor: Arc<Supervisor>, socket: UnixStream) {
    connection_with_budget(supervisor, socket, FRAME_BUDGET);
}
async fn read_v2_frame(
    reader: &mut tokio::io::BufReader<tokio::net::UnixStream>,
    budget: Duration,
) -> Result<Option<Value>> {
    use tokio::io::AsyncBufReadExt;

    let mut bytes = Vec::new();
    let mut deadline: Option<Instant> = None;
    loop {
        let available = if let Some(at) = deadline {
            let remaining = at
                .checked_duration_since(Instant::now())
                .filter(|left| !left.is_zero())
                .ok_or(Error::Refused("Supervisor frame deadline exceeded"))?;
            tokio::time::timeout(remaining, reader.fill_buf())
                .await
                .map_err(|_| Error::Refused("Supervisor frame deadline exceeded"))??
        } else {
            reader.fill_buf().await?
        };
        if available.is_empty() {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(Error::Refused(
                    "Supervisor frame exceeds the size limit or is incomplete",
                ))
            };
        }
        deadline.get_or_insert_with(|| Instant::now() + budget);
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|i| i + 1);
        let count = end.unwrap_or(available.len());
        if bytes.len() + count > MAX_REQUEST_BYTES as usize {
            return Err(Error::Refused(
                "Supervisor frame exceeds the size limit or is incomplete",
            ));
        }
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if end.is_some() {
            return Ok(Some(serde_json::from_slice(&bytes)?));
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct NextBatchResponse<'a> {
    request_id: Option<u64>,
    result: &'a NextBatchResult,
}

fn exit_delivered(
    request_cursor: u64,
    last_event_cursor: Option<u64>,
    exit_sequence: Option<u64>,
) -> bool {
    exit_sequence.is_some_and(|sequence| {
        last_event_cursor
            .unwrap_or(request_cursor)
            .max(request_cursor)
            >= sequence
    })
}

fn batch_response(
    request_id: Option<u64>,
    request_cursor: u64,
    exit_sequence: Option<u64>,
    mut result: NextBatchResult,
) -> Result<Vec<u8>> {
    let had_events = !result.events.is_empty();
    let child_return_code = result.return_code;
    loop {
        let last_event_cursor = result.events.last().map(|event| event.sequence.0);
        result.return_code = exit_delivered(request_cursor, last_event_cursor, exit_sequence)
            .then_some(child_return_code)
            .flatten();
        let bytes = serde_json::to_vec(&NextBatchResponse {
            request_id,
            result: &result,
        })?;
        if bytes.len() < crate::transport::MAX_FRAME_BYTES {
            return Ok(bytes);
        }
        if result.events.pop().is_none() || (result.events.is_empty() && had_events) {
            return Err(Error::Refused(
                "Supervisor event exceeds the frame size limit",
            ));
        }
    }
}

async fn v2_connection_loop(
    supervisor: Arc<Supervisor>,
    socket: tokio::net::UnixStream,
    operator: bool,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut reader = tokio::io::BufReader::new(socket);
    while let Some(raw) = read_v2_frame(&mut reader, FRAME_BUDGET).await? {
        let request_id = raw.get("requestId").cloned();
        let bytes = if raw.get("action").and_then(Value::as_str) == Some("nextBatch") {
            match serde_json::from_value::<NextBatchRequestEnvelope>(raw) {
                Ok(envelope) if envelope.action == crate::SupervisorAction::NextBatch => {
                    match supervisor.child_slot(&envelope.request.handle.0, false) {
                        Ok(Some(slot)) => {
                            let result = loop {
                                let notified = slot.notify.notified();
                                tokio::pin!(notified);
                                notified.as_mut().enable();
                                match supervisor.next_batch_ready(&envelope.request) {
                                    Ok(Some(result)) => break Ok(Some(result)),
                                    Err(error) => break Err(error),
                                    Ok(None) => {}
                                }
                                if !reader.buffer().is_empty() {
                                    break Err(Error::Refused(
                                        "Supervisor requests cannot be pipelined while waiting for events",
                                    ));
                                }
                                tokio::select! {
                                    _ = &mut notified => {},
                                    ready = reader.get_mut().readable() => {
                                        ready?;
                                        let mut byte = [0; 1];
                                        match reader.get_mut().try_read(&mut byte) {
                                            Ok(0) => break Ok(None),
                                            Ok(_) => break Err(Error::Refused(
                                                "Supervisor requests cannot be pipelined while waiting for events",
                                            )),
                                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {},
                                            Err(error) => break Err(error.into()),
                                        }
                                    }
                                }
                            };
                            match result {
                                Ok(Some((result, exit_sequence))) => {
                                    match batch_response(
                                        envelope.request_id,
                                        envelope.request.cursor.0,
                                        exit_sequence,
                                        result,
                                    ) {
                                        Ok(bytes) => bytes,
                                        Err(error) => serde_json::to_vec(
                                            &json!({"requestId":request_id,"error":error.to_string()}),
                                        )?,
                                    }
                                }
                                Ok(None) => return Ok(()),
                                Err(error) => serde_json::to_vec(
                                    &json!({"requestId":request_id,"error":error.to_string()}),
                                )?,
                            }
                        }
                        Ok(None) => serde_json::to_vec(
                            &json!({"requestId":request_id,"error":"Unknown supervisor handle"}),
                        )?,
                        Err(error) => serde_json::to_vec(
                            &json!({"requestId":request_id,"error":error.to_string()}),
                        )?,
                    }
                }
                Ok(_) => serde_json::to_vec(
                    &json!({"requestId":request_id,"error":"Invalid supervisor action"}),
                )?,
                Err(error) => {
                    serde_json::to_vec(&json!({"requestId":request_id,"error":error.to_string()}))?
                }
            }
        } else {
            let response = match serde_json::from_value::<Request>(raw)
                .map_err(Error::from)
                .and_then(|request| supervisor.handle(request.command, operator))
            {
                Ok(value) => json!({"requestId":request_id,"result":value}),
                Err(error) => {
                    json!({"requestId":request_id,"error":error.to_string().chars().take(500).collect::<String>()})
                }
            };
            serde_json::to_vec(&response)?
        };
        if bytes.len() + 1 > crate::transport::MAX_FRAME_BYTES {
            return Err(Error::Refused(
                "Supervisor response exceeds the frame size limit",
            ));
        }
        reader.get_mut().write_all(&bytes).await?;
        reader.get_mut().write_all(b"\n").await?;
        reader.get_mut().flush().await?;
    }
    Ok(())
}

fn connection_with_budget(supervisor: Arc<Supervisor>, mut socket: UnixStream, budget: Duration) {
    let mut admitted = None;
    let result = (|| -> Result<()> {
        socket.set_write_timeout(Some(Duration::from_secs(10)))?;
        let mut reader = BufReader::new(socket.try_clone()?);
        let first_value = read_frame(&mut reader, Some(Instant::now() + budget), budget)?
            .ok_or(Error::Refused("Missing supervisor handshake"))?;
        let protocol = first_value
            .get("protocol")
            .and_then(Value::as_u64)
            .ok_or(Error::Refused("Invalid supervisor protocol"))?;
        let (first, operator) = match protocol {
            PROTOCOL => {
                let first: Handshake = serde_json::from_value(first_value)?;
                let operator = first.operator;
                (first, operator)
            }
            SUPERVISOR_PROTOCOL_V2 => {
                let first: SupervisorHandshakeV2 = serde_json::from_value(first_value)?;
                if first.protocol != SUPERVISOR_PROTOCOL_V2 {
                    return Err(Error::Refused("Invalid supervisor protocol"));
                }
                let operator = first.operator;
                let state_dir = first
                    .state_dir
                    .to_str()
                    .ok_or(Error::Refused("Invalid supervisor state directory"))?
                    .to_owned();
                (
                    Handshake {
                        protocol: PROTOCOL,
                        state_dir,
                        backend_id: first.backend_id.map(|backend| backend.0),
                        probe: first.probe,
                        operator,
                    },
                    operator,
                )
            }
            _ => return Err(Error::Refused("Supervisor protocol mismatch")),
        };
        admitted =
            lock(&supervisor.ownership)?.attach(&first, &supervisor.root.to_string_lossy())?;
        if protocol == PROTOCOL {
            write_frame(&mut socket, &json!({"ok":true}))?;
            while let Some(raw) = read_frame(&mut reader, None, budget)? {
                let id = raw.get("requestId").cloned();
                let response = match serde_json::from_value::<Request>(raw)
                    .map_err(Error::from)
                    .and_then(|request| supervisor.handle(request.command, operator))
                {
                    Ok(value) => json!({"requestId":id,"result":value}),
                    Err(error) => {
                        json!({"requestId":id,"error":error.to_string().chars().take(500).collect::<String>()})
                    }
                };
                write_frame(&mut socket, &response)?;
            }
            return Ok(());
        }

        write_frame(
            &mut socket,
            &json!({"ok":true,"protocol":SUPERVISOR_PROTOCOL_V2}),
        )?;
        drop(reader);
        socket.set_read_timeout(None)?;
        socket.set_write_timeout(None)?;
        let async_socket = socket.try_clone()?;
        async_socket.set_nonblocking(true)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let runtime_supervisor = supervisor.clone();
        runtime.block_on(async move {
            let async_socket = tokio::net::UnixStream::from_std(async_socket)?;
            v2_connection_loop(runtime_supervisor, async_socket, operator).await
        })
    })();
    let _ignored = socket.set_nonblocking(false);
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
        if matches!(
            inspect_start(pid, expected)?.0,
            StartMatch::Exact | StartMatch::Legacy
        ) {
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
        active_children: AtomicUsize::new(0),
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

#[cfg(test)]
#[path = "../tests/supervisor/deadlines.rs"]
mod deadline_tests;
