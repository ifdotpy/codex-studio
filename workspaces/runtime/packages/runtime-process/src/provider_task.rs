//! One Tokio task owns one supervisor socket. No automatic reconnect, replay
//! batch, acknowledgment, or write retry exists at this boundary.
use crate::{provider_seam::*, provider_shared::*, *};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, future::Future, path::PathBuf, time::Duration};
#[cfg(target_os = "linux")]
use std::{io, sync::Arc};
use studio_runtime_providers::{
    DecodedJournalPayload, decode_journal_entry, validate_initialize_params,
};
#[cfg(target_os = "linux")]
use tokio::sync::Semaphore;
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::UnixStream,
    sync::{mpsc, watch},
    time::Instant,
};

/// Supervisor v1 `next` is nonblocking. This is the only polling delay.
const NEXT_QUERY_PAUSE: Duration = Duration::from_millis(50);
const RPC_TIMEOUT: Duration = Duration::from_secs(10);

pub struct SupervisorProviderTaskFactory {
    identity: BackendIdentity,
    endpoint: PathBuf,
    #[cfg(target_os = "linux")]
    environment_reads: Arc<Semaphore>,
}
impl SupervisorProviderTaskFactory {
    pub fn new(identity: BackendIdentity) -> Self {
        let endpoint = identity.state_dir.join("supervisor.sock");
        Self {
            identity,
            endpoint,
            #[cfg(target_os = "linux")]
            environment_reads: Arc::new(Semaphore::new(4)),
        }
    }
}

impl ProviderTaskFactory for SupervisorProviderTaskFactory {
    fn open(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: Shutdown,
    ) -> OpenFuture<'_> {
        Box::pin(async move {
            validate_initialize_params(&options.initialize_params)
                .map_err(|_| AdapterError::InvalidConfiguration)?;
            // Provider codecs allow 16 MiB, while supervisor v1 allows 1 MiB.
            // Reserve envelope/identity space before opening a child.
            if serde_json::to_vec(&options.initialize_params)
                .map_err(|_| AdapterError::InvalidConfiguration)?
                .len()
                + 2048
                > crate::transport::MAX_FRAME_BYTES
            {
                return Err(AdapterError::InvalidConfiguration);
            }
            if handle.as_str().len() > 180
                || options.launch.command.is_empty()
                || options.launch.command.iter().any(|arg| arg.contains('\0'))
                || options.launch.retained_response_ids.len() > MAX_PENDING_RESPONSES
                || !options.launch.cwd.is_absolute()
            {
                return Err(AdapterError::InvalidConfiguration);
            }
            let open = OpenRequest {
                handle: SupervisorHandle(handle.as_str().into()),
                command: options.launch.command,
                env: clean_environment(&options.launch.environment),
                cwd: options.launch.cwd.to_string_lossy().into_owned(),
                extra: Default::default(),
            };
            // Both validation and full wire-size validation precede all I/O.
            serialize_frame(&request_value(SupervisorAction::Open, u64::MAX, &open)?)?;
            let (commands, inbox) = mpsc::channel(LANE_CAPACITY);
            let (events, journal) = mpsc::channel(LANE_CAPACITY);
            let (state, status) = watch::channel(AttachmentStatus {
                info: None,
                initialization: InitializationStatus::AwaitingReply,
            });
            let actor = AdapterTask {
                clock: options.clock,
                handle,
                open,
                identity: self.identity.clone(),
                endpoint: self.endpoint.clone(),
                #[cfg(target_os = "linux")]
                environment_reads: self.environment_reads.clone(),
                expected_generation: options.expected_generation,
                handled_cursor: options.launch.handled_cursor,
                response_ids: options
                    .launch
                    .retained_response_ids
                    .into_iter()
                    .map(|(remote, local)| (remote, Value::from(local)))
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
            };
            let task = tokio::spawn(actor.run());
            Ok(ProviderAttachment::from_task(
                commands, journal, status, shutdown, task,
            ))
        })
    }
}

pub(crate) async fn cancelled(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() {
            return;
        }
        if stop.changed().await.is_err() {
            return;
        }
    }
}

async fn interruptible<T>(
    stop: &mut watch::Receiver<bool>,
    future: impl Future<Output = Result<T, AdapterError>>,
) -> Result<T, AdapterError> {
    tokio::select! {
        biased;
        _ = cancelled(stop) => Err(AdapterError::Cancelled),
        result = tokio::time::timeout(RPC_TIMEOUT, future) => result.unwrap_or(Err(AdapterError::Timeout)),
    }
}

/// The retained future owns framing state. Only terminal cancellation or timeout
/// drops it; either case closes the socket before this task completes.
struct Rpc<S> {
    stream: BufReader<S>,
    sequence: u64,
    started: bool,
}
impl<S: AsyncRead + AsyncWrite + Unpin> Rpc<S> {
    async fn exchange(&mut self, bytes: &[u8]) -> Result<String, AdapterError> {
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
                return String::from_utf8(frame)
                    .map_err(|_| SupervisorError::InvalidFrame("invalid UTF-8".into()).into());
            }
        }
    }

    async fn call<R: Serialize, O: for<'de> Deserialize<'de>>(
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
        let bytes = serialize_frame(&request_value(action, request_id, request)?)?;
        let response = interruptible(stop, self.exchange(&bytes)).await?;
        response_value(action, request_id, &response).map_err(Into::into)
    }
}

struct AdapterTask {
    clock: SessionClock,
    handle: ProviderHandle,
    open: OpenRequest,
    identity: BackendIdentity,
    endpoint: PathBuf,
    #[cfg(target_os = "linux")]
    environment_reads: Arc<Semaphore>,
    expected_generation: Option<Generation>,
    handled_cursor: u64,
    response_ids: BTreeMap<u64, Value>,
    stop: watch::Receiver<bool>,
    commands: mpsc::Receiver<AdapterCommand>,
    events: mpsc::Sender<JournalEntry>,
    status: watch::Sender<AttachmentStatus>,
    info: Option<AttachmentInfo>,
    initialization: InitializationStatus,
    initialize_id: Option<Value>,
    cached_initialize: Option<Value>,
    open_attempted: bool,
}

impl AdapterTask {
    async fn run(mut self) -> TaskExit {
        let result = self.connected().await;
        self.commands.close();
        while let Ok(command) = self.commands.try_recv() {
            command.reject();
        }
        let recovery_needed = (self.open_attempted
            && self.initialization != InitializationStatus::Ready)
            .then(|| InitializationRecovery {
                handle: self.handle.clone(),
                generation: self.info.as_ref().map(|info| info.generation),
                phase: self.initialization,
            });
        TaskExit {
            result,
            recovery_needed,
        }
    }

    fn publish(&self) {
        self.status.send_replace(AttachmentStatus {
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
        };
        let handshake = serialize_frame(&handshake_request(&self.identity, false))?;
        let reply = interruptible(&mut self.stop, rpc.exchange(&handshake)).await?;
        validate_handshake(&reply)?;
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
            if self.handled_cursor > prior.sequence {
                return Err(AdapterError::InvalidConfiguration);
            }
        } else if self.handled_cursor != 0 || self.expected_generation.is_some() {
            return Err(AdapterError::UnverifiableChild);
        }
        self.open_attempted = true;
        let opened: OpenResult = rpc
            .call(SupervisorAction::Open, &self.open, &mut self.stop)
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
            initialization_recovery: opened.resumed.then(|| InitializationRecovery {
                handle: self.handle.clone(),
                generation: Some(opened.generation),
                phase: self.initialization,
            }),
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
        self.serve(&mut rpc, EventCursor(opened.acknowledged)).await
    }

    #[cfg(target_os = "linux")]
    async fn retained_environment(
        &mut self,
        pid: u32,
        exited: bool,
    ) -> Result<BTreeMap<String, String>, AdapterError> {
        if exited {
            return Ok(self.open.env.clone());
        }
        let slots = self.environment_reads.clone();
        let permit = interruptible(&mut self.stop, async move {
            slots
                .acquire_owned()
                .await
                .map_err(|_| AdapterError::TaskFailed)
        })
        .await?;
        let job = tokio::task::spawn_blocking(move || {
            use std::io::Read;
            let _permit = permit;
            let mut bytes = Vec::new();
            std::fs::File::open(format!("/proc/{pid}/environ"))?
                .take(4 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 4 * 1024 * 1024 {
                return Err(io::Error::other("environment exceeds limit"));
            }
            parse_process_environment(&bytes)
        });
        // Reap the bounded filesystem job; never detach a started blocking job.
        let original = job
            .await
            .map_err(|_| AdapterError::TaskFailed)?
            .map_err(|_| AdapterError::UnverifiableChild)?;
        verify_environment(&original, &self.open.env)
            .map_err(|_| AdapterError::UnverifiableChild)?;
        Ok(original)
    }

    #[cfg(not(target_os = "linux"))]
    async fn retained_environment(
        &mut self,
        _pid: u32,
        _exited: bool,
    ) -> Result<BTreeMap<String, String>, AdapterError> {
        Err(AdapterError::UnverifiableChild)
    }

    async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
        &mut self,
        rpc: &mut Rpc<S>,
        mut cursor: EventCursor,
    ) -> Result<(), AdapterError> {
        let mut poll_at = Instant::now();
        let mut terminal = false;
        loop {
            if terminal
                && self
                    .info
                    .as_ref()
                    .is_some_and(|info| info.acknowledged >= cursor)
            {
                return Ok(());
            }
            if *self.stop.borrow() {
                return Err(AdapterError::Cancelled);
            }
            tokio::select! {
                _ = cancelled(&mut self.stop) => return Err(AdapterError::Cancelled),
                _ = self.events.closed() => return Err(AdapterError::ConsumerClosed),
                command = self.commands.recv() => {
                    let Some(command) = command else { return Ok(()); };
                    if terminal && matches!(command, AdapterCommand::Write { .. }) { command.reject(); }
                    else { self.command(rpc, command, cursor).await?; }
                }
                slot = async {
                    tokio::time::sleep_until(poll_at).await;
                    self.events.clone().reserve_owned().await
                }, if !terminal => {
                    let slot = slot.map_err(|_| AdapterError::ConsumerClosed)?;
                    let result: NextResult = rpc.call(SupervisorAction::Next, CursorRequest {
                        handle: self.open.handle.clone(), cursor, extra: Default::default(),
                    }, &mut self.stop).await?;
                    let received_at_ms = self.clock.now_ms();
                    if let Some(event) = result.event {
                        if event.sequence <= cursor {
                            // A stale replay cannot be delivered twice or create a hot loop.
                            return Err(SupervisorError::InvalidFrame("non-advancing journal cursor".into()).into());
                        }
                        cursor = event.sequence;
                        slot.send(self.journal_entry(event, received_at_ms)?);
                        poll_at = Instant::now();
                    } else if result.return_code.is_some() && !result.stdout_reader_alive && !result.stderr_reader_alive {
                        terminal = true;
                    } else {
                        poll_at = Instant::now() + NEXT_QUERY_PAUSE;
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
                    let needs_observation = matches!(
                        receipt,
                        CommandReceipt::Written { .. } | CommandReceipt::CachedInitialize(_)
                    );
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
                        _ = cancelled(&mut self.stop) => return Err(AdapterError::Cancelled),
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
                self.initialize_id = write.message.get("id").cloned();
                return Ok(CommandReceipt::CachedInitialize(cached));
            }
            if self.initialize_id.is_some() {
                return Ok(CommandReceipt::NotSent);
            }
        }
        let native_id = provider_native_id(&write.message).or_else(|| initialize.then_some(1));
        let local_id = method.and_then(|_| write.message.get("id")).cloned();
        if native_id.is_some()
            && (local_id.is_none() || self.response_ids.len() >= MAX_PENDING_RESPONSES)
        {
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
        if serialize_frame(&request_value(SupervisorAction::Write, u64::MAX, &request)?).is_err() {
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
        if native_id.is_some() {
            let remote = result.remote_id.ok_or(SupervisorError::InvalidFrame(
                "accepted request has no remote ID".into(),
            ))?;
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
            remote_id: result.remote_id,
            duplicate: result.duplicate,
        })
    }

    fn journal_entry(
        &mut self,
        mut event: SupervisorEvent,
        received_at_ms: u64,
    ) -> Result<JournalEntry, AdapterError> {
        let payload = match event.kind {
            EventKind::Stdout => {
                let current = self
                    .info
                    .as_ref()
                    .is_some_and(|info| info.generation == event.generation);
                if event.payload.get("method").is_none()
                    && let Some(remote) = event.payload.get("id").and_then(Value::as_u64)
                {
                    let local = if current {
                        self.response_ids.remove(&remote)
                    } else {
                        None
                    };
                    assign_reply_id(&mut event.payload, remote, local)
                        .map_err(SupervisorError::from)?;
                }
                if current
                    && self
                        .initialize_id
                        .as_ref()
                        .is_some_and(|id| event.payload.get("id") == Some(id))
                    && event.payload.get("method").is_none()
                {
                    self.initialization = if event.payload.get("error").is_some() {
                        InitializationStatus::Refused
                    } else {
                        InitializationStatus::InitializedWritePending
                    };
                    self.publish();
                }
                let bytes = match event.payload.get("supervisorRaw").and_then(Value::as_str) {
                    Some(raw) => raw.as_bytes().to_vec(),
                    None => serde_json::to_vec(&event.payload).map_err(SupervisorError::from)?,
                };
                match decode_journal_entry(event.sequence.0, event.generation.0, bytes).payload {
                    DecodedJournalPayload::Frame(frame) => JournalPayload::Frame(frame),
                    DecodedJournalPayload::RawStdout { bytes, error } => {
                        JournalPayload::RawStdout { bytes, error }
                    }
                }
            }
            EventKind::Stderr => JournalPayload::Stderr(event.payload),
            EventKind::Exit => JournalPayload::Exit(event.payload),
            EventKind::Unknown(kind) => JournalPayload::Other {
                kind,
                payload: event.payload,
            },
        };
        Ok(JournalEntry {
            received_at_ms,
            cursor: event.sequence,
            generation: event.generation,
            payload,
        })
    }
}

#[cfg(test)]
#[path = "provider_task_tests.rs"]
mod tests;
