//! Process-owned adapter for one JSONL provider session.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Write};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
pub use studio_runtime_process::ProviderProcess;
use studio_runtime_process::ProviderProcessReader;
use studio_runtime_providers::{
    Action, Frame, Inbound, LineDecoder, Method, RequestId, RequestOutcome, RequestTicket,
    ResponseBody, SessionError, SessionState,
};
use thiserror::Error;

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
    #[error("provider cursor has not been delivered to the consumer")]
    CursorNotDelivered,
}

enum ReadEvent {
    Frame { frame: Frame, cursor: Option<u64> },
    End,
}

/// One provider process, initialized JSONL state machine, and pending events.
/// The session never retries a request; uncertain writes and replies are events.
pub struct ProviderSession {
    process: Option<Box<dyn ProviderProcess>>,
    state: SessionState,
    writer: Option<Box<dyn Write + Send>>,
    reader_events: Option<Receiver<ReadEvent>>,
    reader: Option<JoinHandle<()>>,
    deadlines: BTreeMap<RequestId, u64>,
    queued: VecDeque<(ProviderSessionEvent, Option<u64>)>,
    started: Instant,
    transport_failed: bool,
    reader_ended: bool,
    handled_cursor: u64,
    last_delivered_cursor: u64,
    generation: u64,
}

impl ProviderSession {
    pub fn connect(
        mut process: Box<dyn ProviderProcess>,
        config: ProviderSessionConfig,
    ) -> Result<Self, ProviderSessionError> {
        let handled_cursor = process.acknowledged_cursor();
        let process_generation = process.generation();
        let generation = if process_generation == 0 {
            config.generation
        } else {
            process_generation
        };
        let (reader, writer) = match process.split() {
            Ok(streams) => streams,
            Err(error) => {
                let _ignored = process.stop();
                return Err(ProviderSessionError::Process(error));
            }
        };
        let (sender, reader_events) = mpsc::sync_channel(65_536);
        let reader = spawn_reader(reader, sender);
        let started = Instant::now();
        let (state, initial_actions) = match SessionState::new(
            generation,
            config.claude,
            0,
            config.request_timeout_ms,
            config.initialize_params,
        ) {
            Ok(session) => session,
            Err(error) => {
                let _ignored = process.stop();
                drop(reader_events);
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
            writer: Some(writer),
            reader_events: Some(reader_events),
            reader: Some(reader),
            deadlines: BTreeMap::from([(initialize_id, config.request_timeout_ms)]),
            queued: VecDeque::new(),
            started,
            transport_failed: false,
            reader_ended: false,
            handled_cursor,
            last_delivered_cursor: handled_cursor,
            generation,
        };
        session.apply_actions(initial_actions, None);
        let mut deferred = VecDeque::new();
        loop {
            let received = match session.receive_event() {
                Ok(received) => received,
                Err(error) => {
                    let _ignored = session.close();
                    return Err(error);
                }
            };
            match received {
                (
                    ProviderSessionEvent::Reply {
                        ticket,
                        body: ResponseBody::Result(_),
                        ..
                    },
                    cursor,
                ) if ticket.method == Method::Initialize => {
                    if let Err(error) = session.acknowledge_cursor(cursor) {
                        let _ignored = session.close();
                        return Err(error);
                    }
                    if session.transport_failed {
                        let _ignored = session.close();
                        return Err(ProviderSessionError::InitializeUnknown);
                    }
                    session.queued.extend(deferred);
                    return Ok(session);
                }
                (
                    ProviderSessionEvent::Reply {
                        ticket,
                        body: ResponseBody::Error(_),
                        ..
                    },
                    cursor,
                ) if ticket.method == Method::Initialize => {
                    if let Err(error) = session.acknowledge_cursor(cursor) {
                        let _ignored = session.close();
                        return Err(error);
                    }
                    let _ignored = session.close();
                    return Err(ProviderSessionError::InitializeRejected);
                }
                (ProviderSessionEvent::UnknownOutcome { ticket }, cursor)
                    if ticket.method == Method::Initialize =>
                {
                    if let Err(error) = session.acknowledge_cursor(cursor) {
                        let _ignored = session.close();
                        return Err(error);
                    }
                    let _ignored = session.close();
                    return Err(ProviderSessionError::InitializeUnknown);
                }
                _ if session.transport_failed || session.reader_ended => {
                    let _ignored = session.close();
                    return Err(ProviderSessionError::InitializeUnknown);
                }
                event => deferred.push_back(event),
            }
        }
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
        self.apply_actions(actions, None);
        Ok(ticket)
    }

    /// Wait for one reply, provider callback, unknown outcome, or connection
    /// close. Frames and inbound callbacks retain their wire order.
    pub fn receive_event(
        &mut self,
    ) -> Result<(ProviderSessionEvent, Option<u64>), ProviderSessionError> {
        loop {
            if let Some((event, cursor)) = self.queued.pop_front() {
                if let Some(cursor) = cursor {
                    self.last_delivered_cursor = self.last_delivered_cursor.max(cursor);
                }
                return Ok((event, cursor));
            }
            if self.reader_ended {
                return Err(ProviderSessionError::Closed);
            }
            let now = now_ms(self.started);
            let received = if let Some(deadline) = self.deadlines.values().min().copied() {
                let wait = Duration::from_millis(deadline.saturating_sub(now));
                match self
                    .reader_events
                    .as_ref()
                    .ok_or(ProviderSessionError::Closed)?
                    .recv_timeout(wait)
                {
                    Ok(event) => Some(event),
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        let (result, actions) = self.state.expire(now_ms(self.started));
                        self.deadlines.clear();
                        self.apply_actions(
                            actions
                                .into_iter()
                                .filter(|action| !matches!(action, Action::GenerationClosed { .. }))
                                .collect(),
                            None,
                        );
                        if result.is_err() {
                            continue;
                        }
                        None
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => Some(ReadEvent::End),
                }
            } else {
                self.reader_events
                    .as_ref()
                    .ok_or(ProviderSessionError::Closed)?
                    .recv()
                    .ok()
            };
            match received {
                Some(ReadEvent::Frame { frame, cursor }) => {
                    if cursor.is_some_and(|cursor| cursor <= self.handled_cursor) {
                        self.acknowledge_cursor(cursor)?;
                        continue;
                    }
                    let queued_before = self.queued.len();
                    let actions = self.state.receive(frame)?;
                    self.apply_actions(actions, cursor);
                    if self.queued.len() == queued_before {
                        self.acknowledge_cursor(cursor)?;
                    }
                }
                Some(ReadEvent::End) | None => {
                    self.reader_ended = true;
                    self.deadlines.clear();
                    let actions = self.state.transport_lost();
                    self.apply_actions(actions, None);
                }
            }
        }
    }

    fn apply_actions(&mut self, actions: Vec<Action>, cursor: Option<u64>) {
        for action in actions {
            match action {
                Action::Write(bytes) => {
                    let Some(writer) = self.writer.as_mut() else {
                        self.transport_lost();
                        return;
                    };
                    if writer
                        .write_all(&bytes)
                        .and_then(|()| writer.flush())
                        .is_err()
                    {
                        self.transport_lost();
                        return;
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
                        cursor,
                    ));
                }
                Action::Dispatch(frame) => {
                    self.queued
                        .push_back((ProviderSessionEvent::Inbound(to_inbound(frame)), cursor));
                }
                Action::GenerationClosed { generation } => self
                    .queued
                    .push_back((ProviderSessionEvent::Closed { generation }, cursor)),
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

    /// Mark a consumer event durable and then acknowledge its supervisor cursor.
    pub fn acknowledge_handled(&mut self, cursor: u64) -> Result<(), ProviderSessionError> {
        if cursor > self.last_delivered_cursor {
            return Err(ProviderSessionError::CursorNotDelivered);
        }
        self.acknowledge_cursor(Some(cursor))
    }

    fn acknowledge_cursor(&mut self, cursor: Option<u64>) -> Result<(), ProviderSessionError> {
        let Some(cursor) = cursor else {
            return Ok(());
        };
        if cursor <= self.handled_cursor {
            return Ok(());
        }
        let process = self.process.as_mut().ok_or(ProviderSessionError::Closed)?;
        process
            .acknowledge(cursor)
            .map_err(ProviderSessionError::Process)?;
        self.handled_cursor = process.acknowledged_cursor();
        Ok(())
    }

    /// Last supervisor journal cursor whose decoded frame was dispatched.
    /// Callers persist this with retained request bindings before reattach.
    #[must_use]
    pub const fn handled_cursor(&self) -> u64 {
        self.handled_cursor
    }

    /// Supervisor generation selected when the process was opened or reattached.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
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
                        let mut cursors = VecDeque::from(reader.take_cursors());
                        let cursor_start = frames.len().saturating_sub(cursors.len());
                        for (index, frame) in frames.into_iter().enumerate() {
                            let cursor = (index >= cursor_start)
                                .then(|| cursors.pop_front())
                                .flatten();
                            if sender.send(ReadEvent::Frame { frame, cursor }).is_err() {
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
