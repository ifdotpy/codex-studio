//! Process-owned adapter for one JSONL provider session.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use studio_runtime_providers::{
    Action, Frame, Inbound, LineDecoder, Method, RequestId, RequestOutcome, RequestTicket,
    ResponseBody, SessionError, SessionState, encode_line,
};
use thiserror::Error;

const WRITER_QUEUE_CAPACITY: usize = 128;

/// Configuration for connecting a provider process supplied by its process
/// owner. Production callers use the runtime process supervisor; tests can
/// provide an isolated fake process.
pub struct ProviderSessionConfig {
    pub claude: bool,
    pub generation: u64,
    pub initialize_params: Value,
    pub request_timeout_ms: u64,
}

impl ProviderSessionConfig {
    #[must_use]
    pub fn new() -> Self {
        Self {
            claude: false,
            generation: 1,
            initialize_params: json!({
                "clientInfo": {"name": "codex_agents_canvas", "version": "1.0.0"},
                "capabilities": {"experimentalApi": true}
            }),
            request_timeout_ms: 60_000,
        }
    }
}

impl Default for ProviderSessionConfig {
    fn default() -> Self {
        Self::new()
    }
}

pub trait ProviderProcessReader: Send {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize>;
    fn take_cursors(&mut self) -> Vec<u64>;
}

/// Split byte streams and process lifecycle supplied by the process owner.
/// A production implementation must delegate ownership to `runtime-process`'s
/// supervisor; the session never launches or retries a process itself.
pub trait ProviderProcess: Send {
    fn split(&mut self) -> io::Result<(Box<dyn ProviderProcessReader>, Box<dyn Write + Send>)>;
    fn acknowledged_cursor(&self) -> u64;
    fn generation(&self) -> u64;
    fn acknowledge(&mut self, cursor: u64) -> io::Result<()>;
    fn stop(&mut self) -> io::Result<()>;
}

/// Correlated provider event delivered to the session caller.
#[derive(Clone, Debug, PartialEq)]
pub enum ProviderSessionEvent {
    Reply {
        ticket: RequestTicket,
        body: ResponseBody,
        late: bool,
    },
    UnknownOutcome {
        ticket: RequestTicket,
    },
    Inbound(Inbound),
    Closed {
        generation: u64,
    },
}

#[derive(Debug, Error)]
pub enum ProviderSessionError {
    #[error("provider process transport failed")]
    Process(#[source] io::Error),
    #[error("provider initialize request ended with an unknown outcome")]
    InitializeUnknown,
    #[error("provider initialize request was rejected")]
    InitializeRejected,
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("provider session is closed")]
    Closed,
}

enum ReadEvent {
    Frame(Frame, Option<u64>),
    WriteFailed(io::Error),
    End,
}

/// One provider process, initialized JSONL state machine, and pending events.
/// The session never retries a request; uncertain writes and replies are events.
pub struct ProviderSession {
    process: Option<Box<dyn ProviderProcess>>,
    state: SessionState,
    writer: Option<SyncSender<Vec<u8>>>,
    writer_thread: Option<JoinHandle<()>>,
    reader_events: Option<Receiver<ReadEvent>>,
    reader: Option<JoinHandle<()>>,
    deadlines: BTreeMap<RequestId, u64>,
    queued: VecDeque<(ProviderSessionEvent, Option<u64>)>,
    started: Instant,
    handled_cursor: u64,
    frame_cursor: Option<u64>,
    transport_failed: bool,
    reader_ended: bool,
}

impl ProviderSession {
    pub fn connect(
        process: Box<dyn ProviderProcess>,
        config: ProviderSessionConfig,
    ) -> Result<Self, ProviderSessionError> {
        Self::connect_recovering(process, config, Vec::new())
    }

    pub fn connect_recovering(
        mut process: Box<dyn ProviderProcess>,
        config: ProviderSessionConfig,
        recovered: Vec<(RequestId, Method, Option<String>)>,
    ) -> Result<Self, ProviderSessionError> {
        let generation = process.generation();
        let acknowledged_cursor = process.acknowledged_cursor();
        let (reader, writer) = match process.split() {
            Ok(streams) => streams,
            Err(error) => {
                let _ignored = process.stop();
                return Err(ProviderSessionError::Process(error));
            }
        };
        let (sender, reader_events) = mpsc::sync_channel(65_536);
        let reader = spawn_reader(reader, sender.clone());
        let (writer_sender, writer_receiver) = mpsc::sync_channel(WRITER_QUEUE_CAPACITY);
        let writer_thread = spawn_writer(writer, writer_receiver, sender);
        let started = Instant::now();
        let (state, initial_actions) = match SessionState::new_with_recovered(
            generation,
            config.claude,
            0,
            config.request_timeout_ms,
            config.initialize_params,
            recovered,
        ) {
            Ok(session) => session,
            Err(error) => {
                let _ignored = process.stop();
                drop(reader_events);
                drop(writer_sender);
                let _ignored = writer_thread.join();
                let _ignored = reader.join();
                return Err(error.into());
            }
        };
        let initialize_id = if config.claude {
            RequestId::String("claude:00000000000000000000000000000001".into())
        } else {
            RequestId::Number(1)
        };
        let mut session = Self {
            process: Some(process),
            state,
            writer: Some(writer_sender),
            writer_thread: Some(writer_thread),
            reader_events: Some(reader_events),
            reader: Some(reader),
            deadlines: BTreeMap::from([(initialize_id, config.request_timeout_ms)]),
            queued: VecDeque::new(),
            started,
            handled_cursor: acknowledged_cursor,
            frame_cursor: None,
            transport_failed: false,
            reader_ended: false,
        };
        session.apply_actions(initial_actions);
        let mut deferred = VecDeque::new();
        loop {
            let (event, cursor) = session.receive_event()?;
            match event {
                ProviderSessionEvent::Reply {
                    ticket,
                    body: ResponseBody::Result(_),
                    ..
                } if ticket.method == Method::Initialize => {
                    if let Some(cursor) = cursor {
                        session.acknowledge_handled(cursor)?;
                    }
                    if session.transport_failed {
                        let _ignored = session.close();
                        return Err(ProviderSessionError::InitializeUnknown);
                    }
                    session.queued.extend(deferred);
                    return Ok(session);
                }
                ProviderSessionEvent::Reply {
                    ticket,
                    body: ResponseBody::Error(_),
                    ..
                } if ticket.method == Method::Initialize => {
                    if let Some(cursor) = cursor {
                        session.acknowledge_handled(cursor)?;
                    }
                    let _ignored = session.close();
                    return Err(ProviderSessionError::InitializeRejected);
                }
                ProviderSessionEvent::UnknownOutcome { ticket }
                    if ticket.method == Method::Initialize =>
                {
                    let _ignored = session.close();
                    return Err(ProviderSessionError::InitializeUnknown);
                }
                _ if session.transport_failed || session.reader_ended => {
                    let _ignored = session.stop_process();
                    return Err(ProviderSessionError::InitializeUnknown);
                }
                other => deferred.push_back((other, cursor)),
            }
        }
    }

    #[must_use]
    pub const fn handled_cursor(&self) -> u64 {
        self.handled_cursor
    }

    pub fn acknowledge_handled(&mut self, cursor: u64) -> Result<(), ProviderSessionError> {
        let process = self.process.as_mut().ok_or(ProviderSessionError::Closed)?;
        process
            .acknowledge(cursor)
            .map_err(ProviderSessionError::Process)?;
        self.handled_cursor = cursor;
        Ok(())
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.state.generation()
    }

    /// Submit exactly once. If the write fails after registration, the caller
    /// receives an unknown outcome and must not retry under a new identity.
    pub fn send_request(
        &mut self,
        method: Method,
        params: Value,
        thread_id: Option<String>,
        lightweight: bool,
        timeout_ms: u64,
    ) -> Result<RequestTicket, ProviderSessionError> {
        let now = now_ms(self.started);
        let (ticket, actions) =
            self.state
                .submit(method, params, now, timeout_ms, thread_id, lightweight)?;
        self.deadlines
            .insert(ticket.id.clone(), now.saturating_add(timeout_ms));
        self.apply_actions(actions);
        Ok(ticket)
    }

    pub fn send_request_with_id(
        &mut self,
        id: RequestId,
        method: Method,
        params: Value,
        thread_id: Option<String>,
        lightweight: bool,
        timeout_ms: u64,
    ) -> Result<RequestTicket, ProviderSessionError> {
        let now = now_ms(self.started);
        let (ticket, actions) = self.state.submit_with_id(
            id,
            method,
            params,
            now,
            timeout_ms,
            thread_id,
            lightweight,
        )?;
        self.deadlines
            .insert(ticket.id.clone(), now.saturating_add(timeout_ms));
        self.apply_actions(actions);
        Ok(ticket)
    }

    /// Refuse an inbound provider callback that this runtime does not implement.
    pub fn reject_request(
        &mut self,
        id: RequestId,
        message: &str,
    ) -> Result<(), ProviderSessionError> {
        let bytes = encode_line(&Frame::Response {
            id,
            body: ResponseBody::Error(json!({
                "code": -32601,
                "message": message
            })),
            fields: Default::default(),
        })
        .map_err(|error| ProviderSessionError::Process(io::Error::other(error)))?;
        let Some(writer) = self.writer.as_ref() else {
            return Err(ProviderSessionError::Closed);
        };
        match writer.try_send(bytes) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                self.transport_lost();
                Err(ProviderSessionError::Process(io::Error::other(
                    "provider write queue is full; write outcome is unknown",
                )))
            }
            Err(TrySendError::Disconnected(_)) => Err(ProviderSessionError::Closed),
        }
    }

    /// Wait for one reply, provider callback, unknown outcome, or connection
    /// close. Frames and inbound callbacks retain their wire order.
    pub fn receive_event(
        &mut self,
    ) -> Result<(ProviderSessionEvent, Option<u64>), ProviderSessionError> {
        loop {
            if let Some(event) = self.receive_event_timeout(Duration::from_secs(1))? {
                return Ok(event);
            }
        }
    }

    /// Wait for one event, returning `None` when the caller's deadline expires.
    /// Provider request deadlines still resolve to `UnknownOutcome` events.
    pub fn receive_event_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<(ProviderSessionEvent, Option<u64>)>, ProviderSessionError> {
        let caller_deadline = Instant::now().checked_add(timeout);
        loop {
            if let Some(event) = self.queued.pop_front() {
                return Ok(Some(event));
            }
            if self.reader_ended {
                return Err(ProviderSessionError::Closed);
            }
            let now = now_ms(self.started);
            let request_deadline = self.deadlines.values().min().copied();
            let mut wait = request_deadline
                .map(|deadline| Duration::from_millis(deadline.saturating_sub(now)));
            if let Some(caller_deadline) = caller_deadline {
                let remaining = caller_deadline.saturating_duration_since(Instant::now());
                wait = Some(wait.map_or(remaining, |request_wait| request_wait.min(remaining)));
            }
            let receiver = self
                .reader_events
                .as_ref()
                .ok_or(ProviderSessionError::Closed)?;
            let received = match wait {
                Some(wait) => match receiver.recv_timeout(wait) {
                    Ok(event) => Some(event),
                    Err(mpsc::RecvTimeoutError::Disconnected) => Some(ReadEvent::End),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let now = now_ms(self.started);
                        if request_deadline.is_some_and(|deadline| now >= deadline) {
                            let (result, actions) = self.state.expire(now);
                            self.apply_actions(actions);
                            if result.is_err() {
                                continue;
                            }
                            None
                        } else {
                            return Ok(None);
                        }
                    }
                },
                None => receiver.recv().ok(),
            };
            match received {
                Some(ReadEvent::Frame(frame, cursor)) => {
                    self.frame_cursor = cursor;
                    let queued_before = self.queued.len();
                    let actions = self.state.receive(frame)?;
                    self.apply_actions(actions);
                    self.frame_cursor = None;
                    if self.queued.len() == queued_before
                        && let Some(cursor) = cursor
                    {
                        self.acknowledge_handled(cursor)?;
                    }
                }
                Some(ReadEvent::WriteFailed(error)) => {
                    eprintln!("provider session write failed: {error}");
                    self.transport_lost();
                    self.reader_ended = true;
                    self.queued.push_back((
                        ProviderSessionEvent::Closed {
                            generation: self.state.generation(),
                        },
                        None,
                    ));
                    continue;
                }
                Some(ReadEvent::End) | None => {
                    self.reader_ended = true;
                    self.deadlines.clear();
                    let actions = self.state.transport_lost();
                    self.apply_actions(actions);
                }
            }
        }
    }

    fn apply_actions(&mut self, actions: Vec<Action>) {
        for action in actions {
            match action {
                Action::Write(bytes) => {
                    let Some(writer) = self.writer.as_ref() else {
                        self.transport_lost();
                        return;
                    };
                    match writer.try_send(bytes) {
                        Ok(()) => {}
                        Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                            self.transport_lost();
                            return;
                        }
                    }
                }
                Action::Resolve { ticket, outcome } => {
                    self.deadlines.remove(&ticket.id);
                    self.queued.push_back((
                        match outcome {
                            RequestOutcome::Reply { body, late } => {
                                ProviderSessionEvent::Reply { ticket, body, late }
                            }
                            RequestOutcome::Unknown => {
                                ProviderSessionEvent::UnknownOutcome { ticket }
                            }
                        },
                        self.frame_cursor,
                    ));
                }
                Action::Dispatch(frame) => {
                    self.queued.push_back((
                        ProviderSessionEvent::Inbound(to_inbound(frame)),
                        self.frame_cursor,
                    ));
                }
                Action::GenerationClosed { generation } => self.queued.push_back((
                    ProviderSessionEvent::Closed { generation },
                    self.frame_cursor,
                )),
                Action::ThreadReleased { .. } => {}
            }
        }
    }

    fn transport_lost(&mut self) {
        self.transport_failed = true;
        self.deadlines.clear();
        let actions = self.state.transport_lost();
        for action in actions {
            match action {
                Action::Resolve { ticket, outcome } => {
                    self.queued.push_back((
                        match outcome {
                            RequestOutcome::Reply { body, late } => {
                                ProviderSessionEvent::Reply { ticket, body, late }
                            }
                            RequestOutcome::Unknown => {
                                ProviderSessionEvent::UnknownOutcome { ticket }
                            }
                        },
                        None,
                    ));
                }
                Action::GenerationClosed { generation } => self
                    .queued
                    .push_back((ProviderSessionEvent::Closed { generation }, None)),
                _ => {}
            }
        }
    }

    /// Stop the process before joining the reader, so a blocked pipe read can
    /// finish cleanly and no provider remains detached.
    pub fn close(mut self) -> Result<(), ProviderSessionError> {
        let process_result = self.stop_process();
        self.reader_events.take();
        if let Some(writer) = self.writer_thread.take() {
            let _ignored = writer.join();
        }
        if let Some(reader) = self.reader.take() {
            let _ignored = reader.join();
        }
        process_result.map_err(ProviderSessionError::Process)
    }

    fn stop_process(&mut self) -> io::Result<()> {
        self.writer.take();
        self.process
            .as_mut()
            .map(|process| process.stop())
            .unwrap_or(Ok(()))
    }
}

fn spawn_reader(
    mut reader: Box<dyn ProviderProcessReader>,
    sender: SyncSender<ReadEvent>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut decoder = LineDecoder::new();
        let mut bytes = [0_u8; 8192];
        loop {
            match reader.read(&mut bytes) {
                Ok(0) => {
                    let _ignored = sender.send(ReadEvent::End);
                    return;
                }
                Ok(count) => match decoder.push(&bytes[..count]) {
                    Ok(frames) => {
                        let cursors = reader.take_cursors();
                        let cursors = match cursors.len() {
                            0 => vec![None; frames.len()],
                            count if count == frames.len() => {
                                cursors.into_iter().map(Some).collect()
                            }
                            _ => {
                                let _ignored = sender.send(ReadEvent::End);
                                return;
                            }
                        };
                        for (frame, cursor) in frames.into_iter().zip(cursors) {
                            if sender.send(ReadEvent::Frame(frame, cursor)).is_err() {
                                return;
                            }
                        }
                    }
                    Err(_) => {
                        let _ignored = sender.send(ReadEvent::End);
                        return;
                    }
                },
                Err(_) => {
                    let _ignored = sender.send(ReadEvent::End);
                    return;
                }
            }
        }
    })
}

fn spawn_writer(
    mut writer: Box<dyn Write + Send>,
    receiver: Receiver<Vec<u8>>,
    events: SyncSender<ReadEvent>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while let Ok(bytes) = receiver.recv() {
            if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
                let _ignored = events.send(ReadEvent::WriteFailed(error));
                return;
            }
        }
    })
}

fn to_inbound(frame: Frame) -> Inbound {
    match frame {
        Frame::Request(request) => Inbound::Request(request),
        Frame::Notification(notification) => {
            Inbound::Notification(Frame::Notification(notification))
        }
        other => Inbound::Notification(other),
    }
}

fn now_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
