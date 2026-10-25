//! One provider task owns the command socket; its event pump owns a second
//! supervisor socket for cancellable event waits. Writes and durable ACKs stay
//! on the command socket. No automatic reconnect or write retry exists here.
use crate::{provider_seam::*, provider_shared::*, *};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::Arc;
use std::{collections::BTreeMap, future::Future, path::PathBuf, time::Duration};
use studio_runtime_providers::{
    DecodedJournalPayload, Frame, RequestId, ResponseBody, decode_journal_entry_borrowed,
};
use studio_runtime_store::supervisor_journal::launch_signature;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use tokio::sync::Semaphore;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{mpsc, oneshot, watch},
    time::Instant,
};

const RPC_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum wait for supervisor ownership release, in monotonic elapsed time.
/// Both sockets close concurrently, leaving reporting time inside the backend's
/// account-worker shutdown bound.
pub const SUPERVISOR_DETACH_TIMEOUT: Duration = Duration::from_secs(2);

pub struct SupervisorProviderTaskFactory {
    identity: BackendIdentity,
    endpoint: PathBuf,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    environment_reads: Arc<Semaphore>,
}
impl SupervisorProviderTaskFactory {
    pub fn new(identity: BackendIdentity) -> Self {
        let endpoint = identity.state_dir.join("supervisor.sock");
        Self {
            identity,
            endpoint,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            environment_reads: Arc::new(Semaphore::new(4)),
        }
    }
}

struct Activation {
    options: OpenOptions,
    shutdown: Shutdown,
    _previous: Shutdown,
}

/// A verified supervisor attachment paused before replay or provider writes.
/// The worker installs recovered response mappings when it consumes this factory.
pub struct PreparedProviderTask {
    generation: Generation,
    handle: ProviderHandle,
    pending: std::sync::Mutex<Option<(ProviderAttachment, oneshot::Sender<Activation>)>>,
}

impl PreparedProviderTask {
    pub fn generation(&self) -> Generation {
        self.generation
    }
}

impl ProviderTaskFactory for PreparedProviderTask {
    fn open(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: Shutdown,
    ) -> OpenFuture<'_> {
        Box::pin(async move {
            if handle != self.handle || options.expected_generation != Some(self.generation) {
                return Err(AdapterError::InvalidConfiguration);
            }
            if *shutdown.subscribe().borrow() {
                return Err(AdapterError::Cancelled);
            }
            let (mut attachment, activate) = self
                .pending
                .lock()
                .map_err(|_| AdapterError::TaskFailed)?
                .take()
                .ok_or(AdapterError::InvalidConfiguration)?;
            let previous = std::mem::replace(&mut attachment.shutdown, shutdown.clone());
            activate
                .send(Activation {
                    options,
                    shutdown,
                    _previous: previous,
                })
                .map_err(|_| AdapterError::TaskFailed)?;
            Ok(attachment)
        })
    }
}

impl SupervisorProviderTaskFactory {
    /// Resolve the actual generation through the existing identity/launch checks.
    /// No replay, initialization, ACK or provider write runs until `open` consumes it.
    pub async fn prepare(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: Shutdown,
    ) -> Result<PreparedProviderTask, AdapterError> {
        let (activate, activated) = oneshot::channel();
        let (ready, readiness) = oneshot::channel();
        let mut attachment = self
            .start(handle.clone(), options, shutdown, Some((ready, activated)))
            .await?;
        let generation = match tokio::time::timeout(RPC_TIMEOUT * 4, readiness).await {
            Ok(Ok(generation)) => generation,
            _ => {
                let exit = attachment.shutdown().await;
                let cause = exit.result.err().unwrap_or(AdapterError::Timeout);
                return Err(match exit.cleanup_error {
                    Some(cleanup_error) => AdapterError::PreparationCleanupUnconfirmed {
                        cause: Box::new(cause),
                        cleanup_error,
                    },
                    None => cause,
                });
            }
        };
        Ok(PreparedProviderTask {
            generation,
            handle,
            pending: std::sync::Mutex::new(Some((attachment, activate))),
        })
    }

    fn start(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: Shutdown,
        activation: Option<(oneshot::Sender<Generation>, oneshot::Receiver<Activation>)>,
    ) -> OpenFuture<'_> {
        Box::pin(async move {
            if handle.as_str().len() > 180
                || options.launch.command.is_empty()
                || options.launch.command.iter().any(|arg| arg.contains('\0'))
                || options
                    .launch
                    .cwd
                    .as_ref()
                    .is_some_and(|cwd| !cwd.is_absolute())
            {
                return Err(AdapterError::InvalidConfiguration);
            }
            let open = OpenRequest {
                handle: SupervisorHandle(handle.as_str().into()),
                command: options.launch.command,
                env: clean_environment(&options.launch.environment),
                cwd: options
                    .launch
                    .cwd
                    .as_ref()
                    .map(|cwd| cwd.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                extra: Default::default(),
            };
            // Both validation and full wire-size validation precede all I/O.
            request_frame(
                SupervisorAction::Open,
                u64::MAX,
                open_wire_request(&open, options.launch.cwd.is_none()),
            )?;
            let (commands, inbox) = mpsc::channel(LANE_CAPACITY);
            let (events, journal) = mpsc::channel(LANE_CAPACITY);
            let (state, status) = watch::channel(AttachmentStatus {
                handle: handle.clone(),
                open_attempted: false,
                info: None,
                initialization: InitializationStatus::AwaitingReply,
            });
            let actor = AdapterTask {
                activation,
                inherit_cwd: options.launch.cwd.is_none(),
                clock: options.clock,
                handle,
                open,
                reattach_commands: options.launch.reattach_commands,
                retained: options.launch.retained,
                identity: self.identity.clone(),
                endpoint: self.endpoint.clone(),
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                environment_reads: self.environment_reads.clone(),
                expected_generation: options.expected_generation,
                handled_cursor: options.launch.handled_cursor,
                response_ids: options
                    .launch
                    .retained_response_ids
                    .into_iter()
                    .map(|(remote, local)| (remote, RequestId::Number(local)))
                    .collect(),
                stop: shutdown.subscribe(),
                commands: inbox,
                events,
                status: state,
                info: None,
                initialization: InitializationStatus::AwaitingReply,
                initialize_id: None,
                cached_initialize: None,
                open_attempted: false,
                provider_exited: false,
                cleanup_error: None,
            };
            let task = tokio::spawn(actor.run());
            Ok(
                ProviderAttachment::from_task(commands, journal, status, shutdown, task)
                    .with_supervisor_write_deduplication(),
            )
        })
    }
}

impl ProviderTaskFactory for SupervisorProviderTaskFactory {
    fn open(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: Shutdown,
    ) -> OpenFuture<'_> {
        self.start(handle, options, shutdown, None)
    }
}

#[derive(Serialize)]
struct OpenWireRequest<'a> {
    handle: &'a SupervisorHandle,
    command: &'a [String],
    env: &'a BTreeMap<String, String>,
    cwd: Option<&'a str>,
    #[serde(flatten)]
    extra: &'a BTreeMap<String, Value>,
}

impl SupervisorRequest for OpenWireRequest<'_> {
    fn has_handle(&self) -> bool {
        true
    }
}

fn open_wire_request(open: &OpenRequest, inherit_cwd: bool) -> OpenWireRequest<'_> {
    OpenWireRequest {
        handle: &open.handle,
        command: &open.command,
        env: &open.env,
        cwd: (!inherit_cwd).then_some(open.cwd.as_str()),
        extra: &open.extra,
    }
}

async fn interruptible<T>(
    stop: &mut watch::Receiver<bool>,
    future: impl Future<Output = Result<T, AdapterError>>,
) -> Result<T, AdapterError> {
    tokio::select! {
        biased;
        _ = Shutdown::cancelled(stop) => Err(AdapterError::Cancelled),
        result = tokio::time::timeout(RPC_TIMEOUT, future) => result.unwrap_or(Err(AdapterError::Timeout)),
    }
}

/// The retained future owns framing state. Only terminal cancellation or timeout
/// drops it; either case closes the socket before this task completes.
struct Rpc<S> {
    stream: BufReader<S>,
    sequence: u64,
    started: bool,
    received_at: Instant,
}
impl<S: AsyncRead + AsyncWrite + Unpin> Rpc<S> {
    /// A supervisor EOF follows its ownership detach, whereas dropping our
    /// socket only schedules that work. Bound cleanup even for a silent peer.
    async fn close(&mut self) -> Result<(), AdapterError> {
        tokio::time::timeout(SUPERVISOR_DETACH_TIMEOUT, async {
            self.stream
                .get_mut()
                .shutdown()
                .await
                .map_err(SupervisorError::from)?;
            tokio::io::copy(&mut self.stream, &mut tokio::io::sink())
                .await
                .map_err(SupervisorError::from)?;
            Ok(())
        })
        .await
        .unwrap_or(Err(AdapterError::Timeout))
    }

    async fn send_frame(&mut self, bytes: &[u8]) -> Result<(), AdapterError> {
        self.started = true;
        self.stream
            .get_mut()
            .write_all(bytes)
            .await
            .map_err(SupervisorError::from)?;
        self.stream
            .get_mut()
            .flush()
            .await
            .map_err(SupervisorError::from)?;
        #[cfg(test)]
        tests::panic_on_open_write(bytes);
        Ok(())
    }

    async fn read_frame(&mut self) -> Result<String, AdapterError> {
        let mut frame = Vec::new();
        loop {
            let available = self
                .stream
                .fill_buf()
                .await
                .map_err(SupervisorError::from)?;
            if available.is_empty() {
                return Err(SupervisorError::ConnectionLost.into());
            }
            let count = available
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(available.len(), |end| end + 1);
            if frame.len() + count > crate::transport::MAX_FRAME_BYTES {
                return Err(SupervisorError::InvalidFrame("response exceeds 1 MiB".into()).into());
            }
            let complete = available[count - 1] == b'\n';
            frame.extend_from_slice(&available[..count]);
            self.stream.consume(count);
            if complete {
                self.received_at = Instant::now();
                return String::from_utf8(frame)
                    .map_err(|_| SupervisorError::InvalidFrame("invalid UTF-8".into()).into());
            }
        }
    }

    async fn exchange(&mut self, bytes: &[u8]) -> Result<String, AdapterError> {
        self.send_frame(bytes).await?;
        self.read_frame().await
    }

    async fn call<R: Serialize + SupervisorRequest, O: for<'de> Deserialize<'de>>(
        &mut self,
        action: SupervisorAction,
        request: R,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<O, AdapterError> {
        self.started = false;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(AdapterError::InvalidConfiguration)?;
        let request_id = self.sequence;
        let bytes = request_frame(action, request_id, request)?;
        let response = interruptible(stop, self.exchange(&bytes)).await?;
        response_value(action, request_id, &response).map_err(Into::into)
    }

    async fn call_wait<R: Serialize + SupervisorRequest, O: for<'de> Deserialize<'de>>(
        &mut self,
        action: SupervisorAction,
        request: R,
        stop: &mut watch::Receiver<bool>,
    ) -> Result<O, AdapterError> {
        self.started = false;
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or(AdapterError::InvalidConfiguration)?;
        let request_id = self.sequence;
        let bytes = request_frame(action, request_id, request)?;
        tokio::select! {
            biased;
            _ = Shutdown::cancelled(stop) => return Err(AdapterError::Cancelled),
            result = tokio::time::timeout(RPC_TIMEOUT, self.send_frame(&bytes)) => {
                result.unwrap_or(Err(AdapterError::Timeout))?;
            }
        }
        let response = tokio::select! {
            biased;
            _ = Shutdown::cancelled(stop) => return Err(AdapterError::Cancelled),
            result = self.read_frame() => result?,
        };
        response_value(action, request_id, &response).map_err(Into::into)
    }
}

struct AdapterTask {
    inherit_cwd: bool,
    activation: Option<(oneshot::Sender<Generation>, oneshot::Receiver<Activation>)>,
    clock: SessionClock,
    handle: ProviderHandle,
    open: OpenRequest,
    /// Earlier commands of this launch that a running child may keep.
    reattach_commands: Vec<Vec<String>>,
    /// Launch values that a running child of this provider may hold in another form.
    retained: RetainedLaunchValues,
    identity: BackendIdentity,
    endpoint: PathBuf,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    environment_reads: Arc<Semaphore>,
    expected_generation: Option<Generation>,
    handled_cursor: u64,
    response_ids: BTreeMap<u64, RequestId>,
    stop: watch::Receiver<bool>,
    commands: mpsc::Receiver<AdapterCommand>,
    events: mpsc::Sender<JournalEntry>,
    status: watch::Sender<AttachmentStatus>,
    info: Option<AttachmentInfo>,
    initialization: InitializationStatus,
    initialize_id: Option<RequestId>,
    cached_initialize: Option<Value>,
    open_attempted: bool,
    /// The supervisor reported the end of the attached child (`nextBatch`
    /// with its return code and closed output readers).
    provider_exited: bool,
    cleanup_error: Option<String>,
}

fn pump_channel_closed(shutdown: bool, terminal: bool) -> Option<AdapterError> {
    if terminal {
        None
    } else if shutdown {
        Some(AdapterError::Cancelled)
    } else {
        Some(AdapterError::TaskFailed)
    }
}

impl AdapterTask {
    async fn run(mut self) -> TaskExit {
        let result = self.connected().await;
        self.commands.close();
        while let Ok(command) = self.commands.try_recv() {
            command.reject();
        }
        self.publish();
        let recovery_needed = self.status.borrow().initialization_recovery();
        TaskExit {
            result,
            cleanup_error: self.cleanup_error,
            recovery_needed,
            // The supervisor owns the child: it outlives this attachment
            // unless the attachment saw it end.
            provider_retained: self.info.is_some() && !self.provider_exited,
        }
    }

    fn publish(&self) {
        self.status.send_replace(AttachmentStatus {
            handle: self.handle.clone(),
            open_attempted: self.open_attempted,
            info: self.info.clone(),
            initialization: self.initialization,
        });
    }

    async fn connected(&mut self) -> Result<(), AdapterError> {
        let socket = interruptible(&mut self.stop, async {
            UnixStream::connect(&self.endpoint)
                .await
                .map_err(|error| SupervisorError::Io(error).into())
        })
        .await?;
        let mut rpc = Rpc {
            stream: BufReader::new(socket),
            sequence: 0,
            started: false,
            received_at: Instant::now(),
        };
        let (result, event_task) = match self.attached(&mut rpc).await {
            Ok(pump) => {
                let result = self.serve(&mut rpc, pump.messages, pump.cursor).await;
                pump.shutdown.cancel();
                (result, Some(pump.task))
            }
            Err(error) => (Err(error), None),
        };
        // Poll both half-closes together. Waiting for the event EOF before
        // sending the control FIN would consume two detach budgets.
        let (control, events) = tokio::join!(rpc.close(), async {
            match event_task {
                Some(task) => task.await.unwrap_or(Err(AdapterError::TaskFailed)),
                None => Ok(()),
            }
        });
        self.record_cleanup(control);
        self.record_cleanup(events);
        result
    }

    fn record_cleanup(&mut self, result: Result<(), AdapterError>) {
        if let Err(error) = result {
            let message = format!("supervisor detach was not confirmed: {error}");
            match &mut self.cleanup_error {
                Some(previous) => {
                    previous.push_str("; ");
                    previous.push_str(&message);
                }
                None => self.cleanup_error = Some(message),
            }
        }
    }

    async fn attached(&mut self, rpc: &mut Rpc<UnixStream>) -> Result<EventPump, AdapterError> {
        let handshake = serialize_frame(&handshake_request_v2(&self.identity))?;
        let reply = interruptible(&mut self.stop, rpc.exchange(&handshake)).await?;
        validate_handshake_v2(&reply)?;
        let status: Result<StatusResult, _> = rpc
            .call(
                SupervisorAction::Status,
                HandleRequest {
                    handle: self.open.handle.clone(),
                    extra: Default::default(),
                },
                &mut self.stop,
            )
            .await;
        let prior = match status {
            Ok(status) => Some(status),
            Err(AdapterError::Supervisor(SupervisorError::Remote {
                action: SupervisorAction::Status,
                message,
            })) if message == "Unknown supervisor handle" => None,
            Err(error) => return Err(error),
        };
        if let Some(prior) = &prior {
            // Refuse unsupported/unverifiable reattach before `open` can mutate.
            self.open.env = self
                .retained_environment(prior.pid, prior.return_code.is_some())
                .await?;
            if prior.return_code.is_none()
                && (!self.reattach_commands.is_empty() || self.retained.executable_program)
            {
                self.adopt_reattach_command(rpc, prior.pid).await?;
            }
            if self.handled_cursor > prior.sequence {
                return Err(AdapterError::InvalidConfiguration);
            }
        } else if self.handled_cursor != 0 || self.expected_generation.is_some() {
            return Err(AdapterError::UnverifiableChild);
        }
        self.open_attempted = true;
        self.publish();
        let open = open_wire_request(&self.open, self.inherit_cwd);
        let opened: OpenResult = rpc
            .call(SupervisorAction::Open, open, &mut self.stop)
            .await?;
        // Any uncertain response or identity change preserves a recovery result;
        // it never reconnects or guesses that a second open is safe.
        self.cached_initialize = if opened.resumed {
            opened.init_result
        } else {
            None
        };
        if self.cached_initialize.is_some() {
            self.initialization = InitializationStatus::InitializedWritePending;
        }
        self.info = Some(AttachmentInfo {
            handle: self.handle.clone(),
            generation: opened.generation,
            pid: opened.pid,
            acknowledged: EventCursor(opened.acknowledged),
            resumed: opened.resumed,
        });
        if opened.resumed && prior.as_ref().is_none_or(|prior| prior.pid != opened.pid)
            || self
                .expected_generation
                .is_some_and(|generation| generation != opened.generation)
            || opened.acknowledged > opened.sequence
            || self.handled_cursor > opened.sequence
        {
            return Err(AdapterError::UnverifiableChild);
        }
        self.publish();
        if let Some((ready, activation)) = self.activation.take() {
            ready
                .send(opened.generation)
                .map_err(|_| AdapterError::Cancelled)?;
            let activated = interruptible(&mut self.stop, async {
                activation.await.map_err(|_| AdapterError::Cancelled)
            })
            .await?;
            self.stop = activated.shutdown.subscribe();
            let options = activated.options;
            if options.expected_generation != Some(opened.generation)
                || options.launch.handled_cursor > opened.sequence
            {
                return Err(AdapterError::InvalidConfiguration);
            }
            self.handled_cursor = options.launch.handled_cursor;
            self.response_ids = options
                .launch
                .retained_response_ids
                .into_iter()
                .map(|(remote, local)| (remote, RequestId::Number(local)))
                .collect();
        }
        let event_socket = interruptible(&mut self.stop, async {
            UnixStream::connect(&self.endpoint)
                .await
                .map_err(|error| SupervisorError::Io(error).into())
        })
        .await?;
        let mut event_rpc = Rpc {
            stream: BufReader::new(event_socket),
            sequence: 0,
            started: false,
            received_at: Instant::now(),
        };
        let handshake = serialize_frame(&handshake_request_v2(&self.identity))?;
        let (pump_sender, pump_receiver) = mpsc::channel(1);
        let mut pump_stop = self.stop.clone();
        let event_shutdown = Shutdown::default();
        let mut event_stop = event_shutdown.subscribe();
        let handle = self.open.handle.clone();
        let generation = opened.generation;
        let cursor = EventCursor(opened.acknowledged);
        let event_pump = tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                _ = Shutdown::cancelled(&mut event_stop) => Err(AdapterError::Cancelled),
                result = async {
                    let reply = interruptible(&mut pump_stop, event_rpc.exchange(&handshake)).await?;
                    validate_handshake_v2(&reply)?;
                    event_pump(
                        &mut event_rpc, handle, generation, cursor,
                        pump_sender.clone(), pump_stop.clone(),
                    ).await
                } => result,
            };
            if let Err(error) = result {
                tokio::select! {
                    biased;
                    _ = Shutdown::cancelled(&mut event_stop) => {},
                    _ = send_pump_message(&pump_sender, &mut pump_stop, Err(error)) => {},
                }
            }
            event_rpc.close().await
        });
        Ok(EventPump {
            messages: pump_receiver,
            task: event_pump,
            shutdown: event_shutdown,
            cursor,
        })
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn retained_environment(
        &mut self,
        pid: u32,
        exited: bool,
    ) -> Result<BTreeMap<String, String>, AdapterError> {
        if exited {
            return Ok(self.open.env.clone());
        }
        #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
        let mut original = settled_environment(
            &mut self.stop,
            &self.environment_reads,
            &self.open.env,
            &self.retained,
            RETAINED_ENVIRONMENT_SETTLE,
            move || read_process_environment(pid),
        )
        .await?;
        // A macOS Python launcher adds its marker after the script's exec, so
        // the live environment has it although the accepted launch did not,
        // as the Python supervisor's native_launch_environment handles.
        #[cfg(target_os = "macos")]
        if !self.open.env.contains_key(PYTHON_LAUNCHER_MARKER) {
            original.remove(PYTHON_LAUNCHER_MARKER);
        }
        Ok(original)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    async fn retained_environment(
        &mut self,
        _pid: u32,
        _exited: bool,
    ) -> Result<BTreeMap<String, String>, AdapterError> {
        Err(AdapterError::UnverifiableChild)
    }

    /// Open a running child started with one of the launch's earlier commands
    /// with that command, so the supervisor reattaches it instead of refusing
    /// different launch settings. Health reports each active handle's launch
    /// signature; the supervisor holds at most `MAX_CHILDREN` of them. Only
    /// `id` and `signature` are read, so a supervisor of an earlier release
    /// that outlives a backend install is served the same way. A replaced
    /// program has no earlier command to list: the child's own command is read
    /// from its launch record and adopted when `retained_command` accepts it.
    async fn adopt_reattach_command<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        rpc: &mut Rpc<S>,
        #[cfg_attr(
            not(any(target_os = "linux", target_os = "macos")),
            allow(unused_variables)
        )]
        pid: u32,
    ) -> Result<(), AdapterError> {
        #[derive(Deserialize)]
        struct Health {
            handles: Vec<Handle>,
        }
        #[derive(Deserialize)]
        struct Handle {
            id: String,
            signature: String,
        }
        let health: Health = rpc
            .call(
                SupervisorAction::Health,
                HealthRequest::default(),
                &mut self.stop,
            )
            .await?;
        let Some(running) = health
            .handles
            .into_iter()
            .find(|handle| handle.id == self.open.handle.0)
        else {
            return Ok(());
        };
        let cwd = (!self.inherit_cwd).then_some(self.open.cwd.as_str());
        let signed = |command: &[String]| launch_signature(command, &self.open.env, cwd);
        let mut candidates = self.reattach_commands.clone();
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if self.retained.executable_program
            && signed(&self.open.command) != running.signature
            && let Ok(original) = bounded_read(&mut self.stop, &self.environment_reads, move || {
                read_process_command(pid)
            })
            .await?
            && retained_command(
                &original,
                &self.open.command,
                &self.reattach_commands,
                &self.retained,
            )
        {
            candidates.push(original);
        }
        if let Some(command) = candidates
            .into_iter()
            .find(|command| signed(command) == running.signature)
        {
            self.open.command = command;
        }
        Ok(())
    }

    async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        rpc: &mut Rpc<S>,
        mut pump: mpsc::Receiver<Result<PumpMessage, AdapterError>>,
        mut cursor: EventCursor,
    ) -> Result<(), AdapterError> {
        let mut terminal = false;
        let mut pump_open = true;
        let mut pending_event: Option<(JournalEntry, oneshot::Sender<()>)> = None;
        loop {
            if terminal
                && self
                    .info
                    .as_ref()
                    .is_some_and(|info| info.acknowledged >= cursor)
            {
                break Ok(());
            }
            if *self.stop.borrow() {
                break Err(AdapterError::Cancelled);
            }
            if self.events.is_closed() {
                break Err(AdapterError::ConsumerClosed);
            }
            tokio::select! {
                _ = Shutdown::cancelled(&mut self.stop) => break Err(AdapterError::Cancelled),
                slot = self.events.clone().reserve_owned(), if pending_event.is_some() => {
                    let slot = match slot {
                        Ok(slot) => slot,
                        Err(_) => break Err(AdapterError::ConsumerClosed),
                    };
                    let Some((entry, accepted)) = pending_event.take() else {
                        break Err(AdapterError::TaskFailed);
                    };
                    let delivered = entry.cursor;
                    slot.send(entry);
                    cursor = delivered;
                    if accepted.send(()).is_err() {
                        break Err(AdapterError::Cancelled);
                    }
                }
                command = self.commands.recv() => {
                    let Some(command) = command else { break Ok(()); };
                    if terminal && matches!(command, AdapterCommand::Write { .. }) { command.reject(); }
                    else if let Err(error) = self.command(rpc, command, cursor).await { break Err(error); }
                }
                message = pump.recv(), if pump_open => {
                    match message {
                        Some(Ok(PumpMessage::Event { event, received_at, accepted })) => {
                            let received_at_ms = self.clock.millis_at(received_at);
                            let entry = match self.journal_entry_v2(event, received_at_ms) {
                                Ok(entry) => entry,
                                Err(error) => break Err(error),
                            };
                            if pending_event.replace((entry, accepted)).is_some() {
                                break Err(AdapterError::TaskFailed);
                            }
                        }
                        Some(Ok(PumpMessage::Terminal)) => {
                            terminal = true;
                            self.provider_exited = true;
                        }
                        Some(Err(error)) => break Err(error),
                        None => {
                            pump_open = false;
                            if let Some(error) =
                                pump_channel_closed(*self.stop.borrow(), terminal)
                            {
                                break Err(error);
                            }
                        }
                    }
                }
            }
        }
    }

    async fn command<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        rpc: &mut Rpc<S>,
        command: AdapterCommand,
        delivered: EventCursor,
    ) -> Result<(), AdapterError> {
        match command {
            AdapterCommand::Write {
                write,
                reply,
                observed,
            } => match self.write(rpc, write).await {
                Ok(receipt) => {
                    let needs_observation = receipt.requires_observation();
                    if reply.send(receipt).is_err() {
                        return Err(SupervisorError::ConnectionLost.into());
                    }
                    if !needs_observation {
                        return Ok(());
                    }
                    // Buffering is not observation. Abandonment of the receipt
                    // or its receive future fences this attachment before more I/O.
                    tokio::select! {
                        biased;
                        _ = Shutdown::cancelled(&mut self.stop) => return Err(AdapterError::Cancelled),
                        result = observed => if result.is_err() { return Err(SupervisorError::ConnectionLost.into()); },
                    }
                }
                Err(error) => {
                    let _ignored = reply.send(CommandReceipt::Unknown);
                    return Err(error);
                }
            },
            AdapterCommand::Ack { cursor, reply } => {
                let acknowledged = self
                    .info
                    .as_ref()
                    .map_or(EventCursor(0), |info| info.acknowledged);
                if cursor <= acknowledged {
                    let _ignored = reply.send(CommandReceipt::Acknowledged(acknowledged));
                    return Ok(());
                }
                if cursor > delivered {
                    let _ignored = reply.send(CommandReceipt::NotSent);
                    return Ok(());
                }
                let result: Result<AckResult, _> = rpc
                    .call(
                        SupervisorAction::Ack,
                        AckRequest {
                            handle: self.open.handle.clone(),
                            sequence: cursor,
                            extra: Default::default(),
                        },
                        &mut self.stop,
                    )
                    .await;
                match result {
                    Ok(result) if result.acknowledged == cursor.0 => {
                        if let Some(info) = &mut self.info {
                            info.acknowledged = cursor;
                        }
                        self.publish();
                        let _ignored = reply.send(CommandReceipt::Acknowledged(cursor));
                    }
                    _ => {
                        let _ignored = reply.send(CommandReceipt::Unknown);
                        return Err(SupervisorError::ConnectionLost.into());
                    }
                }
            }
        }
        Ok(())
    }

    async fn write<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        rpc: &mut Rpc<S>,
        write: PreparedWrite,
    ) -> Result<CommandReceipt, AdapterError> {
        if *self.stop.borrow() {
            return Ok(CommandReceipt::NotSent);
        }
        let method = write.message.get("method").and_then(Value::as_str);
        let initialize = method == Some("initialize");
        let initialized = method == Some("initialized");
        if initialized
            && (write.operation_id
                != initialized_operation_id(
                    &self.open.handle,
                    self.info
                        .as_ref()
                        .ok_or(AdapterError::InvalidConfiguration)?
                        .generation,
                )
                || self.initialization != InitializationStatus::InitializedWritePending
                    && self.initialization != InitializationStatus::Ready)
        {
            return Ok(CommandReceipt::NotSent);
        }
        if initialize {
            if let Some(cached) = self.cached_initialize.take() {
                self.initialize_id = local_request_id(&write.message);
                return Ok(CommandReceipt::CachedInitialize(cached));
            }
            if self.initialize_id.is_some() {
                return Ok(CommandReceipt::NotSent);
            }
        }
        let native_id = provider_native_id(&write.message)
            .or_else(|| initialize.then_some(NativeId::Integer(1)));
        let rpc_request = native_id.is_some();
        let local_id = method.and_then(|_| local_request_id(&write.message));
        if rpc_request && local_id.is_none() {
            return Ok(CommandReceipt::NotSent);
        }
        let request = WriteRequest {
            handle: self.open.handle.clone(),
            operation_id: write.operation_id,
            native_id,
            message: write.message,
            extra: Default::default(),
        };
        // Serialization failures happen before send, so they are definitely unsent.
        if request_frame(SupervisorAction::Write, u64::MAX, &request).is_err() {
            return Ok(CommandReceipt::NotSent);
        }
        let result: WriteResult = match rpc
            .call(SupervisorAction::Write, request, &mut self.stop)
            .await
        {
            Ok(result) => result,
            Err(_) if !rpc.started => return Ok(CommandReceipt::NotSent),
            Err(error) => return Err(error),
        };
        if !result.accepted {
            return Ok(CommandReceipt::NotSent);
        }
        if rpc_request {
            let remote = rpc_remote_id(result.remote_id.as_ref()).ok_or(
                SupervisorError::InvalidFrame("accepted request has no integer remote ID".into()),
            )?;
            if let Some(local) = local_id {
                if initialize {
                    self.initialize_id = Some(local.clone());
                }
                self.response_ids.insert(remote, local);
            }
        }
        if initialized {
            self.initialization = InitializationStatus::Ready;
            self.publish();
        }
        Ok(CommandReceipt::Written {
            // A reply can find a Python-written receipt with a text id; it is no RPC identity.
            remote_id: rpc_remote_id(result.remote_id.as_ref()),
            duplicate: result.duplicate,
        })
    }

    /// Decode one journal event. A stdout payload goes straight from its raw
    /// bytes into the typed frame decoder; a reply then takes its local ID.
    fn journal_entry_v2(
        &mut self,
        event: SupervisorEventV2,
        received_at_ms: u64,
    ) -> Result<JournalEntry, AdapterError> {
        let raw = event.payload.get();
        let payload = match event.kind {
            EventKind::Stdout => match decode_stdout(raw) {
                DecodedJournalPayload::Frame(mut frame) => {
                    if let Frame::Response { id, body, .. } = &mut frame {
                        self.settle_reply(id, body, event.generation);
                    }
                    JournalPayload::Frame(frame)
                }
                DecodedJournalPayload::RawStdout { bytes, error } => {
                    JournalPayload::RawStdout { bytes, error }
                }
            },
            EventKind::Stderr => JournalPayload::Stderr(parse_payload(raw)?),
            EventKind::Exit => JournalPayload::Exit(parse_payload(raw)?),
            EventKind::Unknown(kind) => JournalPayload::Other {
                kind,
                payload: parse_payload(raw)?,
            },
        };
        Ok(JournalEntry {
            received_at_ms,
            cursor: event.sequence,
            generation: event.generation,
            payload,
        })
    }

    /// Map a supervisor-assigned integer reply ID back to the local request ID
    /// and settle initialization when this reply answers `initialize`.
    fn settle_reply(&mut self, id: &mut RequestId, body: &ResponseBody, generation: Generation) {
        let current = self
            .info
            .as_ref()
            .is_some_and(|info| info.generation == generation);
        if let RequestId::Number(remote) = *id {
            let local = if current {
                self.response_ids.remove(&remote)
            } else {
                None
            };
            *id = local.unwrap_or_else(|| RequestId::String(format!("supervisor-orphan-{remote}")));
        }
        if current && self.initialize_id.as_ref() == Some(id) {
            self.initialization = match body {
                ResponseBody::Error(_) => InitializationStatus::Refused,
                ResponseBody::Result(_) => InitializationStatus::InitializedWritePending,
            };
            self.publish();
        }
    }
}

/// The supervisor journals a stdout line that is not JSON as this wrapper.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SupervisorRawStdout<'a> {
    #[serde(rename = "supervisorRaw", borrow)]
    raw: std::borrow::Cow<'a, str>,
}

fn decode_stdout(raw: &str) -> DecodedJournalPayload {
    let decoded = decode_journal_entry_borrowed(0, 0, raw.as_bytes()).payload;
    if let DecodedJournalPayload::RawStdout { .. } = decoded
        && let Ok(wrapped) = serde_json::from_str::<SupervisorRawStdout<'_>>(raw)
    {
        return decode_journal_entry_borrowed(0, 0, wrapped.raw.as_bytes()).payload;
    }
    decoded
}

fn parse_payload(raw: &str) -> Result<Value, AdapterError> {
    serde_json::from_str(raw).map_err(|error| SupervisorError::from(error).into())
}

/// The local ID of an outbound request, when it is a valid wire request ID.
fn local_request_id(message: &Value) -> Option<RequestId> {
    message
        .get("id")
        .and_then(|id| RequestId::deserialize(id).ok())
}

struct EventPump {
    messages: mpsc::Receiver<Result<PumpMessage, AdapterError>>,
    task: tokio::task::JoinHandle<Result<(), AdapterError>>,
    shutdown: Shutdown,
    cursor: EventCursor,
}

enum PumpMessage {
    Event {
        event: SupervisorEventV2,
        received_at: Instant,
        accepted: oneshot::Sender<()>,
    },
    Terminal,
}

async fn send_pump_message(
    sender: &mpsc::Sender<Result<PumpMessage, AdapterError>>,
    stop: &mut watch::Receiver<bool>,
    message: Result<PumpMessage, AdapterError>,
) -> Result<(), AdapterError> {
    tokio::select! {
        biased;
        _ = Shutdown::cancelled(stop) => Err(AdapterError::Cancelled),
        sent = sender.send(message) => sent.map_err(|_| AdapterError::Cancelled),
    }
}

async fn event_pump(
    rpc: &mut Rpc<UnixStream>,
    handle: SupervisorHandle,
    generation: Generation,
    mut cursor: EventCursor,
    sender: mpsc::Sender<Result<PumpMessage, AdapterError>>,
    mut stop: watch::Receiver<bool>,
) -> Result<(), AdapterError> {
    loop {
        let result: NextBatchResult = match rpc
            .call_wait(
                SupervisorAction::NextBatch,
                NextBatchRequest {
                    handle: handle.clone(),
                    cursor,
                    generation,
                    limit: SUPERVISOR_NEXT_BATCH_MAX_EVENTS,
                },
                &mut stop,
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                let _ignored = send_pump_message(&sender, &mut stop, Err(error)).await;
                return Ok(());
            }
        };
        let received_at = rpc.received_at;
        let terminal = result.return_code.is_some()
            && !result.stdout_reader_alive
            && !result.stderr_reader_alive;
        if result.events.len() > SUPERVISOR_NEXT_BATCH_MAX_EVENTS {
            let error = SupervisorError::InvalidFrame(
                "nextBatch exceeded the requested event limit".into(),
            )
            .into();
            let _ignored = send_pump_message(&sender, &mut stop, Err(error)).await;
            return Ok(());
        }
        if result.events.is_empty() && !terminal {
            let error = SupervisorError::InvalidFrame(
                "nextBatch returned empty before end of stream".into(),
            )
            .into();
            let _ignored = send_pump_message(&sender, &mut stop, Err(error)).await;
            return Ok(());
        }
        for event in result.events {
            if event.sequence <= cursor {
                let error = SupervisorError::InvalidFrame(
                    "nextBatch returned a non-advancing cursor".into(),
                )
                .into();
                let _ignored = send_pump_message(&sender, &mut stop, Err(error)).await;
                return Ok(());
            }
            let accepted_cursor = event.sequence;
            let (accepted, acceptance) = oneshot::channel();
            if send_pump_message(
                &sender,
                &mut stop,
                Ok(PumpMessage::Event {
                    event,
                    received_at,
                    accepted,
                }),
            )
            .await
            .is_err()
            {
                return Err(AdapterError::Cancelled);
            }
            tokio::select! {
                biased;
                _ = Shutdown::cancelled(&mut stop) => return Err(AdapterError::Cancelled),
                result = acceptance => {
                    if result.is_err() { return Err(AdapterError::Cancelled); }
                }
            }
            // A cursor only moves after the adapter accepted the event into its
            // bounded journal channel. The durable ACK remains a separate command.
            cursor = accepted_cursor;
        }
        if terminal {
            let _ignored = send_pump_message(&sender, &mut stop, Ok(PumpMessage::Terminal)).await;
            return Ok(());
        }
    }
}

/// Upper bound for re-reading a retained child's launch record until it
/// equals the accepted launch. A just-started child can show a record in
/// transition (see `read_process_environment`), which the next read corrects.
/// A child whose record still differs at the end is refused as unverifiable.
/// This and `RETAINED_ENVIRONMENT_RETRY` own the values that the Python
/// supervisor copies; `test_codex_process_supervisor_environment.py` checks them.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const RETAINED_ENVIRONMENT_SETTLE: Duration = Duration::from_secs(5);

/// Pause between two reads of a retained child's launch record.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const RETAINED_ENVIRONMENT_RETRY: Duration = Duration::from_millis(10);

/// Read a retained child's launch environment until it verifies against
/// `clean` or `settle` ends. Each read is a bounded blocking job that holds
/// one of the `slots`; stop cancels the wait between reads.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn settled_environment(
    stop: &mut watch::Receiver<bool>,
    slots: &Arc<Semaphore>,
    clean: &BTreeMap<String, String>,
    retained: &RetainedLaunchValues,
    settle: Duration,
    read: impl Fn() -> std::io::Result<BTreeMap<String, String>> + Clone + Send + 'static,
) -> Result<BTreeMap<String, String>, AdapterError> {
    let deadline = Instant::now() + settle;
    loop {
        if let Ok(original) = bounded_read(stop, slots, read.clone()).await?
            && verify_environment(&original, clean, retained).is_ok()
        {
            return Ok(original);
        }
        if Instant::now() >= deadline {
            return Err(AdapterError::UnverifiableChild);
        }
        tokio::select! {
            biased;
            _ = Shutdown::cancelled(stop) => return Err(AdapterError::Cancelled),
            _ = tokio::time::sleep(RETAINED_ENVIRONMENT_RETRY) => {}
        }
    }
}

/// Run one read of a retained child's launch record as a bounded blocking job
/// that holds one of the `slots`; stop cancels the wait.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn bounded_read<T: Send + 'static>(
    stop: &mut watch::Receiver<bool>,
    slots: &Arc<Semaphore>,
    read: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<std::io::Result<T>, AdapterError> {
    let slots = slots.clone();
    let permit = interruptible(stop, async move {
        slots
            .acquire_owned()
            .await
            .map_err(|_| AdapterError::TaskFailed)
    })
    .await?;
    let job = crate::blocking_pool::spawn_blocking("runtime-process/provider_task", move || {
        let _permit = permit;
        read()
    });
    // Reap the bounded filesystem job; never detach a started blocking job.
    reap_environment_job(job, stop).await
}

/// Abort queued blocking work on stop. Once the OS read has started Tokio cannot
/// cancel it: shutdown retains the socket/permit and joins that bounded read.
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn reap_environment_job<T>(
    mut job: tokio::task::JoinHandle<T>,
    stop: &mut watch::Receiver<bool>,
) -> Result<T, AdapterError> {
    tokio::select! {
        biased;
        _ = Shutdown::cancelled(stop) => {
            job.abort();
            let _ignored = job.await;
            Err(AdapterError::Cancelled)
        }
        result = &mut job => result.map_err(|_| AdapterError::TaskFailed),
    }
}

#[cfg(test)]
#[path = "provider_task_tests.rs"]
mod tests;
