//! Supervisor-backed byte streams for the provider session.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex};
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
/// Protocol v1 `next` is a one-shot query (`codex_process_supervisor.py:835-859`);
/// keep this sole transport poll cancellable until a blocking supervisor action exists.
const NEXT_QUERY_PAUSE: Duration = Duration::from_millis(50);
const READER_JOIN_LIMIT: Duration = Duration::from_secs(2);
type Cancellation = Arc<dyn Fn() + Send + Sync>;
type Cancellations = Arc<Mutex<Option<Cancellation>>>;

struct StopSignal {
    mutex: Mutex<()>,
    changed: Condvar,
}

impl StopSignal {
    fn new() -> Self {
        Self {
            mutex: Mutex::new(()),
            changed: Condvar::new(),
        }
    }

    fn notify(&self) {
        self.changed.notify_all();
    }

    fn wait(&self, timeout: Duration) {
        if let Ok(guard) = self.mutex.lock() {
            let _ignored = self.changed.wait_timeout(guard, timeout);
        }
    }
}

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
) -> Result<(BTreeMap<String, String>, bool), ProviderFactoryError> {
    let mut clean = requested.clone();
    for key in BACKEND_ONLY_ENVIRONMENT {
        clean.remove(*key);
    }
    let Ok(mut client) = SupervisorClient::connect(&identity.state_dir, identity.clone()) else {
        return Ok((clean, false));
    };
    let Ok(status) = client.status(HandleRequest {
        handle: SupervisorHandle(handle.as_str().to_owned()),
        extra: Default::default(),
    }) else {
        return Ok((clean, false));
    };
    if status.return_code.is_some() {
        return Ok((clean, true));
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
        Ok((original, true))
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
        let (environment, status_verified) =
            normalize_environment(&launch.environment, &self.identity, handle)?;
        let open = OpenRequest {
            handle: supervisor_handle.clone(),
            command: launch.command.clone(),
            env: environment,
            cwd: launch.cwd.to_string_lossy().into_owned(),
            extra: Default::default(),
        };
        let mut client =
            SupervisorClient::connect(&self.identity.state_dir, self.identity.clone())?;
        let opened = client.open(open)?;
        if opened.resumed && (!status_verified || opened.init_result.is_none()) {
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
        let cancellations = Arc::new(Mutex::new(Some(Arc::clone(&cancellation))));
        let stopped = Arc::new(AtomicBool::new(false));
        let connection = SupervisorConnection {
            client: Some(client),
            identity: self.identity.clone(),
            handle: supervisor_handle,
            generation: opened.generation,
            pid: opened.pid,
            acknowledged,
            cancellation,
            cancellations: Arc::clone(&cancellations),
            stopped: Arc::clone(&stopped),
        };
        let output = Arc::new(OutputQueue::new());
        let shared = Arc::new(SharedProcess {
            connection: Mutex::new(connection),
            acknowledged_cursor: AtomicU64::new(acknowledged),
            output: Arc::clone(&output),
            cancellations,
            response_ids: Mutex::new(
                launch
                    .retained_response_ids
                    .into_iter()
                    .map(|(remote, local)| (remote, Value::from(local)))
                    .collect(),
            ),
            stopped,
            stop_signal: Arc::new(StopSignal::new()),
            failure: Mutex::new(None),
        });
        let reader_worker = {
            let worker_shared = Arc::clone(&shared);
            let (finished_sender, finished) = mpsc::channel();
            match thread::Builder::new()
                .name("provider-supervisor-reader".into())
                .spawn(move || {
                    let _notify = WorkerFinished(Some(finished_sender));
                    read_supervisor_events(worker_shared);
                }) {
                Ok(join) => ReaderWorker { join, finished },
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
            output,
            reader_worker: Some(reader_worker),
            split: false,
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
    stopped: Arc<AtomicBool>,
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
            Err(error) if is_connection_error(&error) && !self.stopped.load(Ordering::Acquire) => {
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
            Err(error) if is_connection_error(&error) && !self.stopped.load(Ordering::Acquire) => {
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
            native_id: provider_native_id(&message).or_else(|| {
                (message.get("method").and_then(Value::as_str) == Some("initialize")).then_some(1)
            }),
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
        if self.stopped.load(Ordering::Acquire) {
            return Err(SupervisorError::ConnectionLost);
        }
        let mut client = reconnect_client(&self.identity, &self.cancellations, &self.stopped)?;
        let cancellation = self
            .cancellations
            .lock()
            .map_err(|_| SupervisorError::ConnectionLost)?
            .clone()
            .ok_or(SupervisorError::ConnectionLost)?;
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
        if self.stopped.load(Ordering::Acquire) {
            cancellation();
            return Err(SupervisorError::ConnectionLost);
        }
        self.acknowledged = status.acknowledged;
        self.client = Some(client);
        self.cancellation = Arc::clone(&cancellation);
        Ok(())
    }
}

#[cfg(unix)]
fn reconnect_client(
    identity: &BackendIdentity,
    cancellations: &Cancellations,
    stopped: &AtomicBool,
) -> Result<SupervisorClient<SupervisorStream>, SupervisorError> {
    use std::os::unix::net::UnixStream;

    let path = &identity.state_dir;
    let endpoint = if path.is_dir() {
        path.join("supervisor.sock")
    } else {
        path.to_path_buf()
    };
    let stream = UnixStream::connect(endpoint)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let cancellation = cancellation_handle_for_stream(&stream)?;
    let mut registered = cancellations
        .lock()
        .map_err(|_| SupervisorError::ConnectionLost)?;
    if stopped.load(Ordering::Acquire) {
        cancellation();
        return Err(SupervisorError::ConnectionLost);
    }
    *registered = Some(cancellation);
    drop(registered);
    let client = SupervisorClient::from_transport(stream, identity.clone())?;
    if stopped.load(Ordering::Acquire) {
        if let Ok(cancellation) = cancellations.lock()
            && let Some(cancellation) = cancellation.as_ref()
        {
            cancellation();
        }
        return Err(SupervisorError::ConnectionLost);
    }
    Ok(client)
}

#[cfg(not(unix))]
fn reconnect_client(
    identity: &BackendIdentity,
    _cancellations: &Cancellations,
    _stopped: &AtomicBool,
) -> Result<SupervisorClient<SupervisorStream>, SupervisorError> {
    SupervisorClient::connect(&identity.state_dir, identity.clone())
}

#[cfg(unix)]
fn cancellation_handle_for_stream(stream: &SupervisorStream) -> io::Result<Cancellation> {
    use std::net::Shutdown;
    let stream = stream.try_clone()?;
    Ok(Arc::new(move || {
        let _ignored = stream.shutdown(Shutdown::Both);
    }))
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

struct SharedProcess {
    connection: Mutex<SupervisorConnection>,
    acknowledged_cursor: AtomicU64,
    output: Arc<OutputQueue>,
    cancellations: Cancellations,
    response_ids: Mutex<BTreeMap<u64, Value>>,
    stopped: Arc<AtomicBool>,
    stop_signal: Arc<StopSignal>,
    failure: Mutex<Option<String>>,
}

struct OutputItem {
    sequence: Option<u64>,
    bytes: Vec<u8>,
}

struct OutputQueueState {
    priority: Option<OutputItem>,
    ordinary: VecDeque<OutputItem>,
    closed: bool,
}

struct OutputQueue {
    state: Mutex<OutputQueueState>,
    available: Condvar,
    capacity: Condvar,
}

impl OutputQueue {
    fn new() -> Self {
        Self {
            state: Mutex::new(OutputQueueState {
                priority: None,
                ordinary: VecDeque::new(),
                closed: false,
            }),
            available: Condvar::new(),
            capacity: Condvar::new(),
        }
    }

    fn send(&self, item: OutputItem, priority: bool) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("provider output queue lock poisoned"))?;
        loop {
            if state.closed {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "provider output reader closed",
                ));
            }
            if priority {
                if state.priority.is_none() {
                    state.priority = Some(item);
                    self.available.notify_one();
                    return Ok(());
                }
            } else if state.ordinary.len() < OUTPUT_QUEUE_CAPACITY {
                state.ordinary.push_back(item);
                self.available.notify_one();
                return Ok(());
            }
            state = self
                .capacity
                .wait(state)
                .map_err(|_| io::Error::other("provider output queue lock poisoned"))?;
        }
    }

    fn receive(&self) -> io::Result<Option<OutputItem>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("provider output queue lock poisoned"))?;
        loop {
            if let Some(item) = state.priority.take() {
                self.capacity.notify_all();
                return Ok(Some(item));
            }
            if let Some(item) = state.ordinary.pop_front() {
                self.capacity.notify_one();
                return Ok(Some(item));
            }
            if state.closed {
                return Ok(None);
            }
            state = self
                .available
                .wait(state)
                .map_err(|_| io::Error::other("provider output queue lock poisoned"))?;
        }
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            self.available.notify_all();
            self.capacity.notify_all();
        }
    }
}

struct CloseOutputSenderOnExit(Arc<SharedProcess>);

impl Drop for CloseOutputSenderOnExit {
    fn drop(&mut self) {
        self.0.output.close();
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
        let result = match shared.connection.lock() {
            Ok(mut connection) => {
                let acknowledged = connection.acknowledged;
                cursor = cursor.max(acknowledged);
                let result = connection.next(EventCursor(cursor));
                shared
                    .acknowledged_cursor
                    .store(connection.acknowledged, Ordering::Release);
                result
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
            shared.stop_signal.wait(NEXT_QUERY_PAUSE);
        }
    }
}

fn deliver_event(shared: &SharedProcess, event: SupervisorEvent) -> io::Result<()> {
    let generation = shared
        .connection
        .lock()
        .map_err(|_| io::Error::other("provider supervisor connection lock poisoned"))?
        .generation;
    let sequence = event.sequence.0;
    let payload = translate_event(event, generation, &shared.response_ids)?;
    let mut bytes = serde_json::to_vec(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    bytes.push(b'\n');
    send_output(
        shared,
        OutputItem {
            sequence: Some(sequence),
            bytes,
        },
    )
}

fn translate_event(
    mut event: SupervisorEvent,
    generation: Generation,
    response_ids: &Mutex<BTreeMap<u64, Value>>,
) -> io::Result<Value> {
    Ok(match event.kind {
        EventKind::Stdout => {
            if event.generation == generation {
                remap_provider_reply(&mut event.payload, response_ids)?;
            } else if event.payload.get("method").is_none()
                && let Some(remote_id) = event.payload.get("id").and_then(Value::as_u64)
                && let Some(object) = event.payload.as_object_mut()
            {
                object.insert(
                    "id".into(),
                    Value::String(format!("supervisor-orphan-{remote_id}")),
                );
            }
            event.payload
        }
        kind => json!({
            "method": "codex/supervisorEvent",
            "params": {
                "kind": kind,
                "payload": event.payload,
                "generation": event.generation.0,
            }
        }),
    })
}

fn remap_provider_reply(
    payload: &mut Value,
    response_ids: &Mutex<BTreeMap<u64, Value>>,
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
        local_id.unwrap_or_else(|| Value::String(format!("supervisor-orphan-{remote_id}"))),
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

fn send_output(shared: &SharedProcess, item: OutputItem) -> io::Result<()> {
    shared.output.send(item, false)
}

fn send_priority_output(shared: &SharedProcess, item: OutputItem) -> io::Result<()> {
    shared.output.send(item, true)
}

fn acknowledge_prefix(shared: &SharedProcess, cursor: u64) -> io::Result<()> {
    let mut connection = shared
        .connection
        .lock()
        .map_err(|_| io::Error::other("provider supervisor connection lock poisoned"))?;
    if cursor <= connection.acknowledged {
        return Ok(());
    }
    connection
        .acknowledge(cursor)
        .map_err(|error| io::Error::other(error.to_string()))?;
    shared
        .acknowledged_cursor
        .store(connection.acknowledged, Ordering::Release);
    Ok(())
}

fn fail_reader(shared: &SharedProcess, message: &str) {
    if let Ok(mut failure) = shared.failure.lock() {
        *failure = Some(message.to_owned());
    }
    shared.output.close();
}

/// One supervisor-backed provider app-server connection.
pub struct SupervisorProviderProcess {
    shared: Arc<SharedProcess>,
    output: Arc<OutputQueue>,
    reader_worker: Option<ReaderWorker>,
    split: bool,
    resumed: bool,
    resumed_init_result: Option<Value>,
}

impl ProviderProcess for SupervisorProviderProcess {
    fn split(&mut self) -> io::Result<(Box<dyn ProviderProcessReader>, Box<dyn Write + Send>)> {
        if self.split {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "provider process already split",
            ));
        }
        self.split = true;
        let reader = ProviderOutputReader {
            shared: Arc::clone(&self.shared),
            output: Arc::clone(&self.output),
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
            poisoned: false,
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
        match self.shared.connection.lock() {
            Ok(mut connection) => {
                connection.client.take();
                connection.cancellation = Arc::new(|| {});
                if let Ok(mut cancellation) = connection.cancellations.lock() {
                    *cancellation = None;
                }
            }
            Err(_) => {
                stop_error.get_or_insert_with(|| {
                    io::Error::other("provider supervisor connection lock poisoned")
                });
            }
        }
        stop_error.map_or(Ok(()), Err)
    }

    fn cancel(&mut self) -> io::Result<()> {
        self.shared.stopped.store(true, Ordering::Release);
        self.shared.stop_signal.notify();
        self.output.close();
        let mut cancel_error = None;
        match self.shared.cancellations.lock() {
            Ok(cancellation) => {
                if let Some(cancel) = cancellation.as_ref() {
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
        self.shared.acknowledged_cursor.load(Ordering::Acquire)
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

struct ReaderWorker {
    join: JoinHandle<()>,
    finished: Receiver<()>,
}

struct WorkerFinished(Option<mpsc::Sender<()>>);

impl Drop for WorkerFinished {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ignored = sender.send(());
        }
    }
}

fn join_reader_bounded(worker: ReaderWorker) -> io::Result<()> {
    if worker.finished.recv_timeout(READER_JOIN_LIMIT).is_ok() {
        worker
            .join
            .join()
            .map_err(|_| io::Error::other("provider supervisor reader panicked"))
    } else {
        drop(worker.join);
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
    output: Arc<OutputQueue>,
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
            self.current = match self.output.receive()? {
                Some(item) => Some(item),
                None => {
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
            };
            self.offset = 0;
        }
        let item = self
            .current
            .as_ref()
            .ok_or_else(|| io::Error::other("provider output queue returned no current item"))?;
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
    poisoned: bool,
}

impl Write for ProviderInputWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.shared.stopped.load(Ordering::Acquire) || self.poisoned {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "provider process writer is stopped or poisoned",
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
        if self.shared.stopped.load(Ordering::Acquire) || self.poisoned {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "provider process writer is stopped or poisoned",
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
                    if let Err(error) = self.write_message(message) {
                        self.poisoned = true;
                        self.buffer.clear();
                        return Err(error);
                    }
                    self.replayed_initialize = true;
                }
            } else {
                if let Err(error) = self.write_message(message) {
                    self.poisoned = true;
                    self.buffer.clear();
                    return Err(error);
                }
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
            OperationId(format!("initialized:{}", connection.handle.0))
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
        let has_remote_identity = provider_native_id(&message).is_some()
            || message.get("method").and_then(Value::as_str) == Some("initialize");
        let local_id = message
            .get("method")
            .and_then(|_| message.get("id"))
            .cloned();
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
        if has_remote_identity && let Some(local_id) = local_id {
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
        let response_ids = Mutex::new(BTreeMap::from([(42, Value::from(7))]));
        let mut callback = json!({"id":42,"method":"provider/callback","params":{}});
        assert_eq!(provider_native_id(&callback), Some(42));
        remap_provider_reply(&mut callback, &response_ids).unwrap();
        assert_eq!(callback["id"], 42);
        assert_eq!(response_ids.lock().unwrap().get(&42), Some(&Value::from(7)));

        let callback_reply = json!({"id":42,"result":{}});
        assert_eq!(provider_native_id(&callback_reply), None);
        let mut reply = json!({"id":42,"result":{}});
        remap_provider_reply(&mut reply, &response_ids).unwrap();
        assert_eq!(reply["id"], 7);
        assert!(response_ids.lock().unwrap().is_empty());
    }

    #[test]
    fn prior_generation_notification_without_id_is_retained() {
        let response_ids = Mutex::new(BTreeMap::new());
        let event = |generation, payload| SupervisorEvent {
            sequence: EventCursor(1),
            kind: EventKind::Stdout,
            payload,
            generation: Generation(generation),
            extra: BTreeMap::new(),
        };
        assert_eq!(
            translate_event(
                event(1, json!({"method":"provider/notice"})),
                Generation(2),
                &response_ids,
            )
            .unwrap(),
            json!({"method":"provider/notice"})
        );
    }

    #[test]
    fn stderr_and_exit_keep_their_supervisor_cursor_as_notifications() {
        let response_ids = Mutex::new(BTreeMap::new());
        for (kind, payload, name) in [
            (EventKind::Stderr, json!({"data":"diagnostic"}), "stderr"),
            (EventKind::Exit, json!({"returnCode":7}), "exit"),
        ] {
            let translated = translate_event(
                SupervisorEvent {
                    sequence: EventCursor(4),
                    kind,
                    payload: payload.clone(),
                    generation: Generation(3),
                    extra: BTreeMap::new(),
                },
                Generation(3),
                &response_ids,
            )
            .unwrap();
            assert_eq!(translated["method"], "codex/supervisorEvent");
            assert_eq!(translated["params"]["generation"], 3);
            assert_eq!(translated["params"]["kind"], name);
            assert_eq!(translated["params"]["payload"], payload);
        }
    }
}
