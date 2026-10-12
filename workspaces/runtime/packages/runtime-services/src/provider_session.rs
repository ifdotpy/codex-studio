//! Process-owned adapter for one JSONL provider session.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use studio_runtime_providers::{
    Action, Frame, Inbound, LineDecoder, Method, RequestId, RequestOutcome, RequestTicket,
    ResponseBody, SessionError, SessionState, encode_line,
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

/// Split byte streams and process lifecycle supplied by the process owner.
/// A production implementation must delegate ownership to `runtime-process`'s
/// supervisor; the session never launches or retries a process itself.
pub trait ProviderProcess: Send {
    fn split(&mut self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)>;
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
    Frame(Frame),
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
    queued: VecDeque<ProviderSessionEvent>,
    started: Instant,
    transport_failed: bool,
    reader_ended: bool,
}

impl ProviderSession {
    pub fn connect(
        mut process: Box<dyn ProviderProcess>,
        config: ProviderSessionConfig,
    ) -> Result<Self, ProviderSessionError> {
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
            config.generation,
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
        };
        session.apply_actions(initial_actions);
        loop {
            match session.receive_event()? {
                ProviderSessionEvent::Reply {
                    ticket,
                    body: ResponseBody::Result(_),
                    ..
                } if ticket.method == Method::Initialize => {
                    if session.transport_failed {
                        let _ignored = session.stop_process();
                        return Err(ProviderSessionError::InitializeUnknown);
                    }
                    return Ok(session);
                }
                ProviderSessionEvent::Reply {
                    ticket,
                    body: ResponseBody::Error(_),
                    ..
                } if ticket.method == Method::Initialize => {
                    let _ignored = session.stop_process();
                    return Err(ProviderSessionError::InitializeRejected);
                }
                ProviderSessionEvent::UnknownOutcome { ticket }
                    if ticket.method == Method::Initialize =>
                {
                    let _ignored = session.stop_process();
                    return Err(ProviderSessionError::InitializeUnknown);
                }
                _ if session.transport_failed || session.reader_ended => {
                    let _ignored = session.stop_process();
                    return Err(ProviderSessionError::InitializeUnknown);
                }
                _ => {}
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
        let Some(writer) = self.writer.as_mut() else {
            return Err(ProviderSessionError::Closed);
        };
        if let Err(error) = writer.write_all(&bytes).and_then(|()| writer.flush()) {
            self.transport_lost();
            return Err(ProviderSessionError::Process(error));
        }
        Ok(())
    }

    /// Wait for one reply, provider callback, unknown outcome, or connection
    /// close. Frames and inbound callbacks retain their wire order.
    pub fn receive_event(&mut self) -> Result<ProviderSessionEvent, ProviderSessionError> {
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
    ) -> Result<Option<ProviderSessionEvent>, ProviderSessionError> {
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
                            self.deadlines.clear();
                            self.apply_actions(
                                actions
                                    .into_iter()
                                    .filter(|action| {
                                        !matches!(action, Action::GenerationClosed { .. })
                                    })
                                    .collect(),
                            );
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
                Some(ReadEvent::Frame(frame)) => {
                    let actions = self.state.receive(frame)?;
                    self.apply_actions(actions);
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
                    self.queued.push_back(match outcome {
                        RequestOutcome::Reply { body, late } => {
                            ProviderSessionEvent::Reply { ticket, body, late }
                        }
                        RequestOutcome::Unknown => ProviderSessionEvent::UnknownOutcome { ticket },
                    });
                }
                Action::Dispatch(frame) => {
                    self.queued
                        .push_back(ProviderSessionEvent::Inbound(to_inbound(frame)));
                }
                Action::GenerationClosed { generation } => self
                    .queued
                    .push_back(ProviderSessionEvent::Closed { generation }),
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
                    self.queued.push_back(match outcome {
                        RequestOutcome::Reply { body, late } => {
                            ProviderSessionEvent::Reply { ticket, body, late }
                        }
                        RequestOutcome::Unknown => ProviderSessionEvent::UnknownOutcome { ticket },
                    });
                }
                Action::GenerationClosed { generation } => self
                    .queued
                    .push_back(ProviderSessionEvent::Closed { generation }),
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
}

fn spawn_reader(mut reader: Box<dyn Read + Send>, sender: SyncSender<ReadEvent>) -> JoinHandle<()> {
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
                        for frame in frames {
                            if sender.send(ReadEvent::Frame(frame)).is_err() {
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
