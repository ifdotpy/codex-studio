//! Supervisor-backed byte streams for the provider session.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use thiserror::Error;

use crate::{
    AckRequest, BackendIdentity, CursorRequest, EventCursor, EventKind, Generation, HandleRequest,
    OpenRequest, OperationId, OperationStatusRequest, SupervisorClient, SupervisorError,
    SupervisorEvent, SupervisorHandle, WriteRequest, WriteResult,
};

const OUTPUT_QUEUE_CAPACITY: usize = 64;
const MAX_PROVIDER_FRAME_BYTES: usize = 1024 * 1024;
const EMPTY_POLL_PAUSE: Duration = Duration::from_millis(50);

#[cfg(unix)]
type SupervisorStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type SupervisorStream = std::fs::File;

/// Child launch data and its stable supervisor-owned handle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderHandle {
    /// Stable handle, normally `account:<account-key>` for a native account.
    pub handle: String,
    /// Exact executable and arguments used to open or reattach the child.
    pub command: Vec<String>,
    /// Exact child environment.
    pub environment: BTreeMap<String, String>,
    /// Child working directory.
    pub cwd: PathBuf,
}

/// Byte stream and lifecycle contract mirrored by `runtime-services` until
/// the provider-session slice and this adapter are integrated.
pub trait ProviderProcess: Send {
    /// Split the child into its JSONL stdout and stdin byte streams.
    fn split(&mut self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)>;
    /// Detach this backend connection while leaving child ownership with the supervisor.
    fn stop(&mut self) -> io::Result<()>;
}

/// Factory contract consumed by the Rust turn driver.
pub trait ProviderProcessFactory: Send + Sync + 'static {
    /// Start a child under the supplied stable handle or reattach to it.
    fn start_or_reattach(
        &self,
        handle: &ProviderHandle,
    ) -> Result<Box<dyn ProviderProcess>, ProviderFactoryError>;
}

/// Failure to connect a provider session to its supervisor-owned child.
#[derive(Debug, Error)]
pub enum ProviderFactoryError {
    /// The supervisor rejected the connection or child launch.
    #[error(transparent)]
    Supervisor(#[from] SupervisorError),
    /// The adapter could not start its output reader.
    #[error("provider output reader: {0}")]
    Reader(#[from] io::Error),
}

/// Factory for provider app-servers owned by the runtime process supervisor.
#[derive(Clone, Debug)]
pub struct SupervisorProviderProcessFactory {
    identity: BackendIdentity,
}

impl SupervisorProviderProcessFactory {
    /// Bind new child sessions to this backend identity and state directory.
    #[must_use]
    pub fn new(identity: BackendIdentity) -> Self {
        Self { identity }
    }
}

impl ProviderHandle {
    /// Build the native app-server handle used by every agent on one account.
    #[must_use]
    pub fn native_account(
        account_key: impl Into<String>,
        command: Vec<String>,
        environment: BTreeMap<String, String>,
        cwd: PathBuf,
    ) -> Self {
        Self {
            handle: format!("account:{}", account_key.into()),
            command,
            environment,
            cwd,
        }
    }
}

impl ProviderProcessFactory for SupervisorProviderProcessFactory {
    fn start_or_reattach(
        &self,
        launch: &ProviderHandle,
    ) -> Result<Box<dyn ProviderProcess>, ProviderFactoryError> {
        let handle = SupervisorHandle(launch.handle.clone());
        let open = OpenRequest {
            handle: handle.clone(),
            command: launch.command.clone(),
            env: launch.environment.clone(),
            cwd: launch.cwd.to_string_lossy().into_owned(),
            extra: Default::default(),
        };
        let mut client =
            SupervisorClient::connect(&self.identity.state_dir, self.identity.clone())?;
        let opened = client.open(open.clone())?;
        let connection = SupervisorConnection {
            client,
            identity: self.identity.clone(),
            handle,
            generation: opened.generation,
            pid: opened.pid,
            acknowledged: opened.acknowledged,
        };
        let (output_sender, output_receiver) = mpsc::sync_channel(OUTPUT_QUEUE_CAPACITY);
        let shared = Arc::new(SharedProcess {
            connection: Mutex::new(connection),
            acknowledgements: Mutex::new(Acknowledgements {
                cursor: opened.acknowledged,
                consumed: BTreeSet::new(),
            }),
            output_sender: Mutex::new(Some(output_sender)),
            response_ids: Mutex::new(std::collections::BTreeMap::new()),
            stopped: AtomicBool::new(false),
            failure: Mutex::new(None),
        });
        let reader_worker = {
            let worker_shared = Arc::clone(&shared);
            match thread::Builder::new()
                .name("provider-supervisor-reader".into())
                .spawn(move || read_supervisor_events(worker_shared))
            {
                Ok(worker) => worker,
                Err(error) => {
                    if let Ok(mut connection) = shared.connection.lock() {
                        let _ignored = connection.detach();
                    }
                    return Err(error.into());
                }
            }
        };
        Ok(Box::new(SupervisorProviderProcess {
            shared,
            output_receiver: Some(output_receiver),
            reader_worker: Some(reader_worker),
            stopped: false,
            resumed_init_result: if opened.resumed {
                opened.init_result
            } else {
                None
            },
        }))
    }
}

struct SupervisorConnection {
    client: SupervisorClient<SupervisorStream>,
    identity: BackendIdentity,
    handle: SupervisorHandle,
    generation: Generation,
    pid: u32,
    acknowledged: u64,
}

impl SupervisorConnection {
    fn next(&mut self, cursor: EventCursor) -> Result<crate::NextResult, SupervisorError> {
        let request = CursorRequest {
            handle: self.handle.clone(),
            cursor: EventCursor(cursor.0.max(self.acknowledged)),
            extra: Default::default(),
        };
        match self.client.next(request.clone()) {
            Ok(result) => Ok(result),
            Err(error) if is_connection_error(&error) => {
                self.reconnect()?;
                let request = CursorRequest {
                    cursor: EventCursor(cursor.0.max(self.acknowledged)),
                    ..request
                };
                self.client.next(request)
            }
            Err(error) => Err(error),
        }
    }

    fn acknowledge(&mut self, sequence: u64) -> Result<(), SupervisorError> {
        if sequence <= self.acknowledged {
            return Ok(());
        }
        let request = AckRequest {
            handle: self.handle.clone(),
            sequence: EventCursor(sequence),
            extra: Default::default(),
        };
        match self.client.ack(request.clone()) {
            Ok(result) => {
                self.acknowledged = result.acknowledged;
                Ok(())
            }
            Err(error) if is_connection_error(&error) => {
                self.reconnect()?;
                if self.acknowledged >= sequence {
                    return Ok(());
                }
                let result = self.client.ack(request)?;
                self.acknowledged = result.acknowledged;
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    fn write_message(
        &mut self,
        operation_id: OperationId,
        message: Value,
    ) -> Result<Option<u64>, SupervisorError> {
        let request = WriteRequest {
            handle: self.handle.clone(),
            operation_id: operation_id.clone(),
            native_id: message.get("id").and_then(Value::as_u64),
            message,
            extra: Default::default(),
        };
        match self.client.write(request.clone()) {
            Ok(result) => accepted_write(result),
            Err(error) if is_connection_error(&error) => {
                self.reconnect()?;
                let status = self.client.operation_status(OperationStatusRequest {
                    handle: self.handle.clone(),
                    operation_id: operation_id.clone(),
                    extra: Default::default(),
                })?;
                if status.accepted {
                    return Ok(status.native_id);
                }
                // Reuse the same supervisor idempotency key after an explicit
                // not-accepted receipt; never create a new write identity.
                let result = self.client.write(request)?;
                accepted_write(result)
            }
            Err(error) => Err(error),
        }
    }

    fn detach(&mut self) -> Result<(), SupervisorError> {
        self.client
            .detach(HandleRequest {
                handle: self.handle.clone(),
                extra: Default::default(),
            })
            .map(|_| ())
    }

    fn reconnect(&mut self) -> Result<(), SupervisorError> {
        let mut client =
            SupervisorClient::connect(&self.identity.state_dir, self.identity.clone())?;
        let status = client.status(HandleRequest {
            handle: self.handle.clone(),
            extra: Default::default(),
        })?;
        if status.pid != self.pid {
            return Err(SupervisorError::Remote {
                action: crate::SupervisorAction::Status,
                message: "provider handle now identifies a different child".into(),
            });
        }
        self.acknowledged = status.acknowledged;
        self.client = client;
        Ok(())
    }
}

fn ensure_write_accepted(accepted: bool) -> Result<(), SupervisorError> {
    if accepted {
        Ok(())
    } else {
        Err(SupervisorError::Remote {
            action: crate::SupervisorAction::Write,
            message: "provider input was not accepted".into(),
        })
    }
}

fn accepted_write(result: WriteResult) -> Result<Option<u64>, SupervisorError> {
    ensure_write_accepted(result.accepted)?;
    Ok(result.remote_id)
}

fn is_connection_error(error: &SupervisorError) -> bool {
    matches!(
        error,
        SupervisorError::Io(_)
            | SupervisorError::ConnectionLost
            | SupervisorError::UnknownOutcome { .. }
    )
}

struct Acknowledgements {
    cursor: u64,
    consumed: BTreeSet<u64>,
}

struct SharedProcess {
    connection: Mutex<SupervisorConnection>,
    acknowledgements: Mutex<Acknowledgements>,
    output_sender: Mutex<Option<SyncSender<OutputItem>>>,
    response_ids: Mutex<BTreeMap<u64, u64>>,
    stopped: AtomicBool,
    failure: Mutex<Option<String>>,
}

struct OutputItem {
    sequence: Option<u64>,
    bytes: Vec<u8>,
}

struct CloseOutputSenderOnExit(Arc<SharedProcess>);

impl Drop for CloseOutputSenderOnExit {
    fn drop(&mut self) {
        if let Ok(mut sender) = self.0.output_sender.lock() {
            sender.take();
        }
    }
}

fn read_supervisor_events(shared: Arc<SharedProcess>) {
    let _close_sender = CloseOutputSenderOnExit(Arc::clone(&shared));
    let mut cursor = match shared.connection.lock() {
        Ok(connection) => connection.acknowledged,
        Err(_) => {
            fail_reader(&shared, "provider supervisor connection lock poisoned");
            return;
        }
    };
    while !shared.stopped.load(Ordering::Acquire) {
        if let Err(error) = flush_acknowledgements(&shared) {
            fail_reader(&shared, &error.to_string());
            return;
        }
        let result = match shared.connection.lock() {
            Ok(mut connection) => {
                let acknowledged = connection.acknowledged;
                cursor = cursor.max(acknowledged);
                connection.next(EventCursor(cursor))
            }
            Err(_) => {
                fail_reader(&shared, "provider supervisor connection lock poisoned");
                return;
            }
        };
        let result = match result {
            Ok(result) => result,
            Err(error) => {
                fail_reader(&shared, &error.to_string());
                return;
            }
        };
        if let Some(event) = result.event {
            if event.sequence.0 <= cursor {
                fail_reader(&shared, "supervisor returned a non-advancing event cursor");
                return;
            }
            cursor = event.sequence.0;
            if let Err(error) = deliver_event(&shared, event) {
                if !shared.stopped.load(Ordering::Acquire) {
                    fail_reader(&shared, &error.to_string());
                }
                return;
            }
        } else if result.return_code.is_some()
            && !result.stdout_reader_alive
            && !result.stderr_reader_alive
        {
            return;
        } else {
            thread::sleep(EMPTY_POLL_PAUSE);
        }
    }
}

fn deliver_event(shared: &SharedProcess, mut event: SupervisorEvent) -> io::Result<()> {
    let generation = shared
        .connection
        .lock()
        .map_err(|_| io::Error::other("provider supervisor connection lock poisoned"))?
        .generation;
    if event.generation != generation || event.kind != EventKind::Stdout {
        return mark_consumed(shared, event.sequence.0);
    }
    if event.payload.get("method").is_none()
        && let Some(remote_id) = event.payload.get("id").and_then(Value::as_u64)
    {
        let local_id = shared
            .response_ids
            .lock()
            .map_err(|_| io::Error::other("provider response ID lock poisoned"))?
            .remove(&remote_id);
        let Some(payload) = event.payload.as_object_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "provider response is not an object",
            ));
        };
        payload.insert(
            "id".into(),
            local_id.map_or_else(
                || Value::String(format!("supervisor-orphan-{remote_id}")),
                Value::from,
            ),
        );
    }
    let mut bytes = serde_json::to_vec(&event.payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    bytes.push(b'\n');
    send_output(
        shared,
        OutputItem {
            sequence: Some(event.sequence.0),
            bytes,
        },
    )
}

fn send_output(shared: &SharedProcess, mut item: OutputItem) -> io::Result<()> {
    loop {
        if shared.stopped.load(Ordering::Acquire) {
            return Ok(());
        }
        let sender = shared
            .output_sender
            .lock()
            .map_err(|_| io::Error::other("provider output sender lock poisoned"))?;
        let Some(channel) = sender.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "provider output reader closed",
            ));
        };
        match channel.try_send(item) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Full(returned)) => {
                item = returned;
                drop(sender);
                thread::sleep(EMPTY_POLL_PAUSE);
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "provider output reader closed",
                ));
            }
        }
    }
}

fn mark_consumed(shared: &SharedProcess, sequence: u64) -> io::Result<()> {
    let mut acknowledgements = shared
        .acknowledgements
        .lock()
        .map_err(|_| io::Error::other("provider supervisor acknowledgement lock poisoned"))?;
    acknowledgements.consumed.insert(sequence);
    flush_acknowledgements_locked(shared, &mut acknowledgements)
}

fn flush_acknowledgements(shared: &SharedProcess) -> io::Result<()> {
    let mut acknowledgements = shared
        .acknowledgements
        .lock()
        .map_err(|_| io::Error::other("provider supervisor acknowledgement lock poisoned"))?;
    flush_acknowledgements_locked(shared, &mut acknowledgements)
}

fn flush_acknowledgements_locked(
    shared: &SharedProcess,
    acknowledgements: &mut Acknowledgements,
) -> io::Result<()> {
    let mut target = acknowledgements.cursor;
    while target
        .checked_add(1)
        .is_some_and(|next| acknowledgements.consumed.contains(&next))
    {
        target += 1;
    }
    if target == acknowledgements.cursor {
        return Ok(());
    }
    let mut connection = shared
        .connection
        .lock()
        .map_err(|_| io::Error::other("provider supervisor connection lock poisoned"))?;
    connection
        .acknowledge(target)
        .map_err(|error| io::Error::other(error.to_string()))?;
    acknowledgements
        .consumed
        .retain(|sequence| *sequence > target);
    acknowledgements.cursor = target;
    Ok(())
}

fn fail_reader(shared: &SharedProcess, message: &str) {
    if let Ok(mut failure) = shared.failure.lock() {
        *failure = Some(message.to_owned());
    }
}

/// One supervisor-backed provider app-server connection.
pub struct SupervisorProviderProcess {
    shared: Arc<SharedProcess>,
    output_receiver: Option<Receiver<OutputItem>>,
    reader_worker: Option<JoinHandle<()>>,
    stopped: bool,
    resumed_init_result: Option<Value>,
}

impl ProviderProcess for SupervisorProviderProcess {
    fn split(&mut self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        let receiver = self.output_receiver.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "provider process already split",
            )
        })?;
        let reader = ProviderOutputReader {
            shared: Arc::clone(&self.shared),
            receiver,
            current: None,
            offset: 0,
        };
        let writer = ProviderInputWriter {
            shared: Arc::clone(&self.shared),
            buffer: Vec::new(),
            pending_operation_id: None,
            operation_prefix: operation_prefix(),
            operation_sequence: 0,
            resumed_init_result: self.resumed_init_result.take(),
            replayed_initialize: false,
        };
        Ok((Box::new(reader), Box::new(writer)))
    }

    fn stop(&mut self) -> io::Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        self.shared.stopped.store(true, Ordering::Release);
        self.output_receiver.take();
        if let Some(worker) = self.reader_worker.take() {
            worker
                .join()
                .map_err(|_| io::Error::other("provider supervisor reader panicked"))?;
        }
        self.shared
            .output_sender
            .lock()
            .map_err(|_| io::Error::other("provider output sender lock poisoned"))?
            .take();
        let mut connection = self
            .shared
            .connection
            .lock()
            .map_err(|_| io::Error::other("provider supervisor connection lock poisoned"))?;
        connection
            .detach()
            .map_err(|error| io::Error::other(error.to_string()))?;
        Ok(())
    }
}

impl Drop for SupervisorProviderProcess {
    fn drop(&mut self) {
        let _ignored = self.stop();
    }
}

struct ProviderOutputReader {
    shared: Arc<SharedProcess>,
    receiver: Receiver<OutputItem>,
    current: Option<OutputItem>,
    offset: usize,
}

impl Read for ProviderOutputReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        loop {
            if self.current.is_none() {
                match self.receiver.recv() {
                    Ok(item) => {
                        self.current = Some(item);
                        self.offset = 0;
                    }
                    Err(_) => {
                        let message = self
                            .shared
                            .failure
                            .lock()
                            .map_err(|_| io::Error::other("provider reader failure lock poisoned"))?
                            .clone();
                        return match message {
                            Some(message) => Err(io::Error::other(message)),
                            None => Ok(0),
                        };
                    }
                }
            }
            let item = self.current.as_ref().ok_or_else(|| {
                io::Error::other("provider output channel returned no current item")
            })?;
            if self.offset == item.bytes.len() {
                let sequence = item.sequence;
                self.current = None;
                if let Some(sequence) = sequence {
                    let _acknowledgement = mark_consumed(&self.shared, sequence);
                }
                continue;
            }
            let count = (item.bytes.len() - self.offset).min(output.len());
            output[..count].copy_from_slice(&item.bytes[self.offset..self.offset + count]);
            self.offset += count;
            if self.offset == item.bytes.len() {
                let sequence = item.sequence;
                self.current = None;
                if let Some(sequence) = sequence {
                    let _acknowledgement = mark_consumed(&self.shared, sequence);
                }
            }
            return Ok(count);
        }
    }
}

struct ProviderInputWriter {
    shared: Arc<SharedProcess>,
    buffer: Vec<u8>,
    pending_operation_id: Option<OperationId>,
    operation_prefix: String,
    operation_sequence: u64,
    resumed_init_result: Option<Value>,
    replayed_initialize: bool,
}

impl Write for ProviderInputWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.shared.stopped.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "provider process is stopped",
            ));
        }
        self.buffer.extend_from_slice(bytes);
        let mut frame_start = 0;
        for (index, byte) in self.buffer.iter().enumerate() {
            if *byte == b'\n' {
                if index - frame_start > MAX_PROVIDER_FRAME_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "provider input frame exceeds the supervisor limit",
                    ));
                }
                frame_start = index + 1;
            }
        }
        if self.buffer.len() - frame_start > MAX_PROVIDER_FRAME_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "provider input frame exceeds the supervisor limit",
            ));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.shared.stopped.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "provider process is stopped",
            ));
        }
        while let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let message: Value = serde_json::from_slice(&self.buffer[..end])
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let is_initialize = message.get("method").and_then(Value::as_str) == Some("initialize");
            if is_initialize && !self.replayed_initialize {
                if let Some(result) = self.resumed_init_result.take() {
                    let id = message.get("id").cloned().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "initialize request has no id")
                    })?;
                    let mut bytes = serde_json::to_vec(&json!({"id": id, "result": result}))
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    bytes.push(b'\n');
                    send_output(
                        &self.shared,
                        OutputItem {
                            sequence: None,
                            bytes,
                        },
                    )?;
                    self.replayed_initialize = true;
                } else {
                    self.write_message(message)?;
                    self.replayed_initialize = true;
                }
            } else {
                self.write_message(message)?;
            }
            self.buffer.drain(..=end);
        }
        Ok(())
    }
}

impl ProviderInputWriter {
    fn write_message(&mut self, message: Value) -> io::Result<()> {
        let operation_id = match self.pending_operation_id.clone() {
            Some(operation_id) => operation_id,
            None => {
                self.operation_sequence = self
                    .operation_sequence
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("provider write identity space exhausted"))?;
                let operation_id = OperationId(format!(
                    "{}:{}",
                    self.operation_prefix, self.operation_sequence
                ));
                self.pending_operation_id = Some(operation_id.clone());
                operation_id
            }
        };
        let local_id = message.get("id").and_then(Value::as_u64);
        // The supervisor can emit a response as soon as it accepts the write.
        // Publish its returned remote ID before the event worker can translate it.
        let mut response_ids = self
            .shared
            .response_ids
            .lock()
            .map_err(|_| io::Error::other("provider response ID lock poisoned"))?;
        let remote_id = self
            .shared
            .connection
            .lock()
            .map_err(|_| io::Error::other("provider supervisor connection lock poisoned"))?
            .write_message(operation_id, message)
            .map_err(|error| io::Error::other(error.to_string()))?;
        if let Some(local_id) = local_id {
            let remote_id = remote_id.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "supervisor accepted a provider request without a remote ID",
                )
            })?;
            response_ids.insert(remote_id, local_id);
        }
        self.pending_operation_id = None;
        Ok(())
    }
}

fn operation_prefix() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("provider:{}:{timestamp}", std::process::id())
}
