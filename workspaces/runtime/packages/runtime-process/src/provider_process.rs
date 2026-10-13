//! Supervisor-backed byte streams for the provider session.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use thiserror::Error;

use crate::{
    AckRequest, BackendIdentity, CursorRequest, EventCursor, EventKind, Generation, HandleRequest,
    OpenRequest, OperationId, ProviderProcess, ProviderProcessReader, SupervisorClient,
    SupervisorError, SupervisorEvent, SupervisorHandle, WriteRequest, WriteResult,
};

const OUTPUT_QUEUE_CAPACITY: usize = 64;
const MAX_PROVIDER_FRAME_BYTES: usize = 1024 * 1024;
const EMPTY_POLL_PAUSE: Duration = Duration::from_millis(50);
const READER_JOIN_LIMIT: Duration = Duration::from_secs(2);
type Cancellation = Arc<dyn Fn() + Send + Sync>;
type Cancellations = Arc<Mutex<Vec<Cancellation>>>;

#[cfg(unix)]
type SupervisorStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type SupervisorStream = std::fs::File;

/// Stable identity for a supervisor-owned child.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProviderHandle(String);

impl ProviderHandle {
    #[must_use]
    pub fn for_account(account_key: &str) -> Self {
        Self(format!("account:{account_key}"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Child launch values resolved by the owning server from account/runtime state.
/// This intentionally has no `Debug` implementation because it contains secrets.
#[derive(Clone)]
pub struct ProviderLaunchSpec {
    pub command: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub cwd: PathBuf,
    /// Recovered native-to-session request IDs installed before replay starts.
    pub retained_response_ids: BTreeMap<u64, u64>,
    /// Cursor already handled by runtime state, used to make an uncertain ACK idempotent.
    pub handled_cursor: u64,
}

const BACKEND_ONLY_ENVIRONMENT: &[&str] = &[
    "CODEX_AGENTS_BACKEND_ID",
    "CODEX_RUNTIME_LOCK_METRICS",
    "CODEX_AGENTS_PROVIDER_CAPTURE",
    "CODEX_AGENTS_PROVIDER_CAPTURE_FILE",
    "CODEX_NODE",
];
const RETAINED_LAUNCHER_ENVIRONMENT: &[&str] = &[
    "PATH",
    "LANG",
    "LC_CTYPE",
    "LC_ALL",
    "__PYVENV_LAUNCHER__",
    "COMMAND_MODE",
    "MallocNanoZone",
    "XPC_SERVICE_NAME",
    "__CFBundleIdentifier",
];

fn normalize_environment(
    requested: &BTreeMap<String, String>,
    identity: &BackendIdentity,
    handle: &ProviderHandle,
) -> Result<BTreeMap<String, String>, ProviderFactoryError> {
    let mut clean = requested.clone();
    for key in BACKEND_ONLY_ENVIRONMENT {
        clean.remove(*key);
    }
    let Ok(mut client) = SupervisorClient::connect(&identity.state_dir, identity.clone()) else {
        return Ok(clean);
    };
    let Ok(status) = client.status(HandleRequest {
        handle: SupervisorHandle(handle.as_str().to_owned()),
        extra: Default::default(),
    }) else {
        return Ok(clean);
    };
    if status.return_code.is_some() {
        return Ok(clean);
    }
    #[cfg(target_os = "linux")]
    {
        let original = process_environment(status.pid).map_err(|_| ProviderFactoryError::Launch)?;
        let without_launcher = |values: &BTreeMap<String, String>| {
            values
                .iter()
                .filter(|(key, _)| {
                    !BACKEND_ONLY_ENVIRONMENT.contains(&key.as_str())
                        && !RETAINED_LAUNCHER_ENVIRONMENT.contains(&key.as_str())
                })
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect::<BTreeMap<_, _>>()
        };
        if without_launcher(&original) != without_launcher(&clean) {
            return Err(ProviderFactoryError::Launch);
        }
        Ok(original)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = status;
        Err(ProviderFactoryError::Launch)
    }
}

#[cfg(target_os = "linux")]
fn process_environment(pid: u32) -> io::Result<BTreeMap<String, String>> {
    let raw = std::fs::read(format!("/proc/{pid}/environ"))?;
    if raw.len() > 4 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "native process environment exceeds its limit",
        ));
    }
    parse_process_environment(&raw)
}

#[cfg(target_os = "linux")]
fn parse_process_environment(raw: &[u8]) -> io::Result<BTreeMap<String, String>> {
    let mut environment = BTreeMap::new();
    for entry in raw
        .split(|byte| *byte == 0)
        .take_while(|entry| !entry.is_empty())
    {
        let Some(separator) = entry.iter().position(|byte| *byte == b'=') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid process environment",
            ));
        };
        let key = std::str::from_utf8(&entry[..separator])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let value = std::str::from_utf8(&entry[separator + 1..])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        environment.insert(key.to_owned(), value.to_owned());
    }
    Ok(environment)
}

impl ProviderLaunchSpec {
    #[must_use]
    pub fn new(command: Vec<String>, environment: BTreeMap<String, String>, cwd: PathBuf) -> Self {
        Self {
            command,
            environment,
            cwd,
            retained_response_ids: BTreeMap::new(),
            handled_cursor: 0,
        }
    }
}

/// Resolves the account-scoped child command and environment for a stable handle.
pub trait ProviderLaunchResolver: Send + Sync + 'static {
    fn resolve(&self, handle: &ProviderHandle) -> Result<ProviderLaunchSpec, String>;
}

impl<F> ProviderLaunchResolver for F
where
    F: Fn(&ProviderHandle) -> Result<ProviderLaunchSpec, String> + Send + Sync + 'static,
{
    fn resolve(&self, handle: &ProviderHandle) -> Result<ProviderLaunchSpec, String> {
        self(handle)
    }
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
    /// The server could not resolve the provider command/configuration.
    #[error("provider launch configuration could not be resolved or verified")]
    Launch,
}

/// Factory for provider app-servers owned by the runtime process supervisor.
#[derive(Clone)]
pub struct SupervisorProviderProcessFactory {
    identity: BackendIdentity,
    resolver: Arc<dyn ProviderLaunchResolver>,
}

impl SupervisorProviderProcessFactory {
    /// Bind new child sessions to this backend identity and state directory.
    #[must_use]
    pub fn new(identity: BackendIdentity, resolver: impl ProviderLaunchResolver) -> Self {
        Self {
            identity,
            resolver: Arc::new(resolver),
        }
    }
}

impl ProviderProcessFactory for SupervisorProviderProcessFactory {
    fn start_or_reattach(
        &self,
        handle: &ProviderHandle,
    ) -> Result<Box<dyn ProviderProcess>, ProviderFactoryError> {
        let launch = self
            .resolver
            .resolve(handle)
            .map_err(|_| ProviderFactoryError::Launch)?;
        let supervisor_handle = SupervisorHandle(handle.as_str().to_owned());
        let open = OpenRequest {
            handle: supervisor_handle.clone(),
            command: launch.command.clone(),
            env: normalize_environment(&launch.environment, &self.identity, handle)?,
            cwd: launch.cwd.to_string_lossy().into_owned(),
            extra: Default::default(),
        };
        let mut client =
            SupervisorClient::connect(&self.identity.state_dir, self.identity.clone())?;
        let opened = client.open(open)?;
        if opened.resumed && opened.init_result.is_none() {
            let _ignored = client.detach(HandleRequest {
                handle: supervisor_handle.clone(),
                extra: Default::default(),
            });
            return Err(ProviderFactoryError::Launch);
        }
        let mut acknowledged = opened.acknowledged;
        if launch.handled_cursor > acknowledged {
            if launch.handled_cursor > opened.sequence {
                return Err(ProviderFactoryError::Launch);
            }
            acknowledged = client
                .ack(AckRequest {
                    handle: supervisor_handle.clone(),
                    sequence: EventCursor(launch.handled_cursor),
                    extra: Default::default(),
                })?
                .acknowledged;
        }
        let cancellation = cancellation_handle(&client)?;
        let cancellations = Arc::new(Mutex::new(vec![Arc::clone(&cancellation)]));
        let connection = SupervisorConnection {
            client: Some(client),
            identity: self.identity.clone(),
            handle: supervisor_handle,
            generation: opened.generation,
            pid: opened.pid,
            acknowledged,
            cancellation,
            cancellations: Arc::clone(&cancellations),
        };
        let (output_sender, output_receiver) = mpsc::sync_channel(OUTPUT_QUEUE_CAPACITY);
        let (priority_sender, priority_receiver) = mpsc::sync_channel(1);
        let shared = Arc::new(SharedProcess {
            connection: Mutex::new(connection),
            acknowledgements: Mutex::new(Acknowledgements {
                cursor: acknowledged,
                consumed: BTreeSet::new(),
            }),
            output_sender: Mutex::new(Some(output_sender)),
            priority_sender: Mutex::new(Some(priority_sender)),
            cancellations,
            response_ids: Mutex::new(launch.retained_response_ids),
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
            priority_receiver: Some(priority_receiver),
            reader_worker: Some(reader_worker),
            resumed: opened.resumed,
            resumed_init_result: if opened.resumed {
                opened.init_result
            } else {
                None
            },
        }))
    }
}

struct SupervisorConnection {
    client: Option<SupervisorClient<SupervisorStream>>,
    identity: BackendIdentity,
    handle: SupervisorHandle,
    generation: Generation,
    pid: u32,
    acknowledged: u64,
    cancellation: Cancellation,
    cancellations: Cancellations,
}

impl SupervisorConnection {
    fn next(&mut self, cursor: EventCursor) -> Result<crate::NextResult, SupervisorError> {
        let request = CursorRequest {
            handle: self.handle.clone(),
            cursor: EventCursor(cursor.0.max(self.acknowledged)),
            extra: Default::default(),
        };
        match self
            .client
            .as_mut()
            .ok_or(SupervisorError::ConnectionLost)?
            .next(request.clone())
        {
            Ok(result) => Ok(result),
            Err(error) if is_connection_error(&error) => {
                self.reconnect()?;
                let request = CursorRequest {
                    cursor: EventCursor(cursor.0.max(self.acknowledged)),
                    ..request
                };
                self.client
                    .as_mut()
                    .ok_or(SupervisorError::ConnectionLost)?
                    .next(request)
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
        match self
            .client
            .as_mut()
            .ok_or(SupervisorError::ConnectionLost)?
            .ack(request.clone())
        {
            Ok(result) => {
                self.acknowledged = result.acknowledged;
                Ok(())
            }
            Err(error) if is_connection_error(&error) => {
                self.reconnect()?;
                if self.acknowledged >= sequence {
                    return Ok(());
                }
                let result = self
                    .client
                    .as_mut()
                    .ok_or(SupervisorError::ConnectionLost)?
                    .ack(request)?;
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
            native_id: provider_native_id(&message),
            message,
            extra: Default::default(),
        };
        match self.client.as_mut().ok_or(SupervisorError::ConnectionLost)?.write(request) {
            Ok(result) => accepted_write(result),
            Err(error) if is_connection_error(&error) => Err(SupervisorError::Remote {
                action: crate::SupervisorAction::Write,
                message: "provider write outcome is unknown; operationStatus supports monitor writes only".into(),
            }),
            Err(error) => Err(error),
        }
    }

    fn detach(&mut self) -> Result<(), SupervisorError> {
        let result = self
            .client
            .as_mut()
            .ok_or(SupervisorError::ConnectionLost)?
            .detach(HandleRequest {
                handle: self.handle.clone(),
                extra: Default::default(),
            })
            .map(|_| ());
        self.client.take();
        result
    }

    fn reconnect(&mut self) -> Result<(), SupervisorError> {
        let mut client =
            SupervisorClient::connect(&self.identity.state_dir, self.identity.clone())?;
        let cancellation = cancellation_handle(&client)?;
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
        self.client = Some(client);
        self.cancellation = Arc::clone(&cancellation);
        self.cancellations
            .lock()
            .map_err(|_| SupervisorError::ConnectionLost)?
            .push(cancellation);
        Ok(())
    }
}

fn cancellation_handle(client: &SupervisorClient<SupervisorStream>) -> io::Result<Cancellation> {
    #[cfg(unix)]
    {
        client.cancellation_handle()
    }
    #[cfg(windows)]
    {
        let _ = client;
        Ok(Arc::new(|| {}))
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
    priority_sender: Mutex<Option<SyncSender<OutputItem>>>,
    cancellations: Cancellations,
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
    if discard_event(&event, generation) {
        return mark_consumed(shared, event.sequence.0);
    }
    remap_provider_reply(&mut event.payload, &shared.response_ids)?;
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

fn discard_event(event: &SupervisorEvent, generation: Generation) -> bool {
    if event.kind != EventKind::Stdout {
        return true;
    }
    if event.generation == generation {
        return false;
    }
    !(event.generation < generation && event.payload.get("id").is_none())
}

fn remap_provider_reply(
    payload: &mut Value,
    response_ids: &Mutex<BTreeMap<u64, u64>>,
) -> io::Result<()> {
    if payload.get("method").is_some() {
        return Ok(());
    }
    let Some(remote_id) = payload.get("id").and_then(Value::as_u64) else {
        return Ok(());
    };
    let local_id = response_ids
        .lock()
        .map_err(|_| io::Error::other("provider response ID lock poisoned"))?
        .remove(&remote_id);
    let Some(payload) = payload.as_object_mut() else {
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
    Ok(())
}

fn provider_native_id(message: &Value) -> Option<u64> {
    message
        .get("method")
        .and_then(Value::as_str)
        .and_then(|_| message.get("id"))
        .and_then(Value::as_u64)
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

fn send_priority_output(shared: &SharedProcess, item: OutputItem) -> io::Result<()> {
    let sender = shared
        .priority_sender
        .lock()
        .map_err(|_| io::Error::other("provider priority sender lock poisoned"))?;
    let Some(channel) = sender.as_ref() else {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "provider priority reader closed",
        ));
    };
    channel.try_send(item).map_err(|error| match error {
        mpsc::TrySendError::Full(_) => io::Error::new(
            io::ErrorKind::WouldBlock,
            "provider priority slot is occupied",
        ),
        mpsc::TrySendError::Disconnected(_) => {
            io::Error::new(io::ErrorKind::BrokenPipe, "provider priority reader closed")
        }
    })
}

fn mark_consumed(shared: &SharedProcess, sequence: u64) -> io::Result<()> {
    let mut acknowledgements = shared
        .acknowledgements
        .lock()
        .map_err(|_| io::Error::other("provider supervisor acknowledgement lock poisoned"))?;
    acknowledgements.consumed.insert(sequence);
    flush_acknowledgements_locked(shared, &mut acknowledgements)
}

fn acknowledge_prefix(shared: &SharedProcess, cursor: u64) -> io::Result<()> {
    let mut acknowledgements = shared
        .acknowledgements
        .lock()
        .map_err(|_| io::Error::other("provider supervisor acknowledgement lock poisoned"))?;
    if cursor <= acknowledgements.cursor {
        return Ok(());
    }
    let mut connection = shared
        .connection
        .lock()
        .map_err(|_| io::Error::other("provider supervisor connection lock poisoned"))?;
    connection
        .acknowledge(cursor)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let acknowledged = connection.acknowledged.max(cursor);
    acknowledgements.cursor = acknowledged;
    acknowledgements
        .consumed
        .retain(|sequence| *sequence > acknowledged);
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
    let acknowledged = connection.acknowledged.max(target);
    acknowledgements
        .consumed
        .retain(|sequence| *sequence > acknowledged);
    acknowledgements.cursor = acknowledged;
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
    priority_receiver: Option<Receiver<OutputItem>>,
    reader_worker: Option<JoinHandle<()>>,
    resumed: bool,
    resumed_init_result: Option<Value>,
}

impl ProviderProcess for SupervisorProviderProcess {
    fn split(&mut self) -> io::Result<(Box<dyn ProviderProcessReader>, Box<dyn Write + Send>)> {
        let receiver = self.output_receiver.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "provider process already split",
            )
        })?;
        let priority_receiver = self.priority_receiver.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "provider priority stream already split",
            )
        })?;
        let reader = ProviderOutputReader {
            shared: Arc::clone(&self.shared),
            receiver,
            priority_receiver,
            current: None,
            offset: 0,
            completed_cursors: VecDeque::new(),
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
        let mut stop_error = self.cancel().err();
        if let Some(worker) = self.reader_worker.take()
            && let Err(error) = join_reader_bounded(worker)
        {
            stop_error.get_or_insert(error);
        }
        match self.shared.connection.try_lock() {
            Ok(mut connection) => {
                connection.client.take();
                connection.cancellation = Arc::new(|| {});
                connection.cancellations = Arc::new(Mutex::new(Vec::new()));
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                stop_error.get_or_insert_with(|| {
                    io::Error::other("provider supervisor connection lock poisoned")
                });
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                stop_error.get_or_insert_with(|| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "provider supervisor connection remained busy after cancellation",
                    )
                });
            }
        }
        for sender in [&self.shared.output_sender, &self.shared.priority_sender] {
            match sender.lock() {
                Ok(mut sender) => {
                    sender.take();
                }
                Err(_) => {
                    stop_error.get_or_insert_with(|| {
                        io::Error::other("provider output sender lock poisoned")
                    });
                }
            }
        }
        stop_error.map_or(Ok(()), Err)
    }

    fn cancel(&mut self) -> io::Result<()> {
        self.shared.stopped.store(true, Ordering::Release);
        self.output_receiver.take();
        self.priority_receiver.take();
        let mut cancel_error = None;
        for sender in [&self.shared.output_sender, &self.shared.priority_sender] {
            match sender.lock() {
                Ok(mut sender) => {
                    sender.take();
                }
                Err(_) => {
                    cancel_error.get_or_insert_with(|| {
                        io::Error::other("provider output sender lock poisoned")
                    });
                }
            }
        }
        match self.shared.cancellations.lock() {
            Ok(cancellations) => {
                for cancel in cancellations.iter() {
                    cancel();
                }
            }
            Err(_) => {
                cancel_error
                    .get_or_insert_with(|| io::Error::other("provider cancellation lock poisoned"));
            }
        }
        cancel_error.map_or(Ok(()), Err)
    }

    fn acknowledged_cursor(&self) -> u64 {
        self.shared
            .acknowledgements
            .lock()
            .map(|acknowledgements| acknowledgements.cursor)
            .unwrap_or_default()
    }

    fn generation(&self) -> u64 {
        self.shared
            .connection
            .lock()
            .map(|connection| connection.generation.0)
            .unwrap_or_default()
    }

    fn resumed(&self) -> bool {
        self.resumed
    }

    fn acknowledge(&mut self, cursor: u64) -> io::Result<()> {
        acknowledge_prefix(&self.shared, cursor)
    }
}

fn join_reader_bounded(worker: JoinHandle<()>) -> io::Result<()> {
    let deadline = std::time::Instant::now() + READER_JOIN_LIMIT;
    while !worker.is_finished() && std::time::Instant::now() < deadline {
        thread::yield_now();
    }
    if worker.is_finished() {
        worker
            .join()
            .map_err(|_| io::Error::other("provider supervisor reader panicked"))
    } else {
        drop(worker);
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "provider supervisor reader did not stop after cancellation",
        ))
    }
}

impl Drop for SupervisorProviderProcess {
    fn drop(&mut self) {
        let _ignored = self.cancel();
        let _ignored = self.stop();
    }
}

struct ProviderOutputReader {
    shared: Arc<SharedProcess>,
    receiver: Receiver<OutputItem>,
    priority_receiver: Receiver<OutputItem>,
    current: Option<OutputItem>,
    offset: usize,
    completed_cursors: VecDeque<u64>,
}

impl Read for ProviderOutputReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.current.is_none() {
            loop {
                match self.priority_receiver.try_recv() {
                    Ok(item) => {
                        self.current = Some(item);
                        break;
                    }
                    Err(TryRecvError::Disconnected) => {}
                    Err(TryRecvError::Empty) => {}
                }
                match self.receiver.recv_timeout(EMPTY_POLL_PAUSE) {
                    Ok(item) => {
                        self.current = Some(item);
                        break;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if self.shared.stopped.load(Ordering::Acquire) {
                            return Ok(0);
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
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
            self.offset = 0;
        }
        let item = self
            .current
            .as_ref()
            .ok_or_else(|| io::Error::other("provider output channel returned no current item"))?;
        let count = (item.bytes.len() - self.offset).min(output.len());
        output[..count].copy_from_slice(&item.bytes[self.offset..self.offset + count]);
        self.offset += count;
        if self.offset == item.bytes.len() {
            let sequence = item.sequence;
            self.current = None;
            if let Some(sequence) = sequence {
                self.completed_cursors.push_back(sequence);
            }
        }
        Ok(count)
    }
}

impl ProviderProcessReader for ProviderOutputReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        Read::read(self, output)
    }

    fn take_cursors(&mut self) -> Vec<u64> {
        self.completed_cursors.drain(..).collect()
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
                    send_priority_output(
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
        let initialized = message.get("method").and_then(Value::as_str) == Some("initialized");
        let operation_id = if initialized {
            let connection =
                self.shared.connection.lock().map_err(|_| {
                    io::Error::other("provider supervisor connection lock poisoned")
                })?;
            OperationId(if connection.generation.0 == 1 {
                format!("initialized:{}", connection.handle.0)
            } else {
                format!(
                    "initialized:{}:{}",
                    connection.handle.0, connection.generation.0
                )
            })
        } else {
            match self.pending_operation_id.clone() {
                Some(operation_id) => operation_id,
                None => {
                    self.operation_sequence =
                        self.operation_sequence.checked_add(1).ok_or_else(|| {
                            io::Error::other("provider write identity space exhausted")
                        })?;
                    let operation_id = OperationId(format!(
                        "{}:{}",
                        self.operation_prefix, self.operation_sequence
                    ));
                    self.pending_operation_id = Some(operation_id.clone());
                    operation_id
                }
            }
        };
        let local_id = provider_native_id(&message);
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
        if !initialized {
            self.pending_operation_id = None;
        }
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

#[cfg(test)]
mod provider_event_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn callback_reply_with_mapped_id_does_not_consume_request_mapping() {
        let response_ids = Mutex::new(BTreeMap::from([(42, 7)]));
        let mut callback = json!({"id":42,"method":"provider/callback","params":{}});
        assert_eq!(provider_native_id(&callback), Some(42));
        remap_provider_reply(&mut callback, &response_ids).unwrap();
        assert_eq!(callback["id"], 42);
        assert_eq!(response_ids.lock().unwrap().get(&42), Some(&7));

        let callback_reply = json!({"id":42,"result":{}});
        assert_eq!(provider_native_id(&callback_reply), None);
        let mut reply = json!({"id":42,"result":{}});
        remap_provider_reply(&mut reply, &response_ids).unwrap();
        assert_eq!(reply["id"], 7);
        assert!(response_ids.lock().unwrap().is_empty());
    }

    #[test]
    fn prior_generation_notification_without_id_is_retained() {
        let event = |generation, payload| SupervisorEvent {
            sequence: EventCursor(1),
            kind: EventKind::Stdout,
            payload,
            generation: Generation(generation),
            extra: BTreeMap::new(),
        };
        assert!(!discard_event(
            &event(1, json!({"method":"provider/notice"})),
            Generation(2)
        ));
        assert!(discard_event(
            &event(1, json!({"id":9,"result":{}})),
            Generation(2)
        ));
    }
}
