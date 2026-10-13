//! Supervisor-owned provider streams and account-actor session state.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_process::ProviderProcessReader;
pub use studio_runtime_process::{
    ProviderFactoryError, ProviderHandle, ProviderProcess, ProviderProcessFactory,
};
use studio_runtime_providers::{
    Action, Frame, Inbound, LineDecoder, Method, RequestId, RequestOutcome, RequestTicket,
    ResponseBody, SessionError, SessionState, encode_line,
};
use thiserror::Error;

const WRITER_QUEUE_CAPACITY: usize = 128;
const WORKER_JOIN_LIMIT: Duration = Duration::from_secs(2);
const LOCAL_INPUT_CAPACITY: usize = 64;
const COMPLETED_ID_LIMIT: usize = 4096;

/// Session input produced by the supervisor byte-reader and the write worker.
#[derive(Clone, Debug, PartialEq)]
pub enum ProviderSessionInput {
    Frame {
        frame: Frame,
        cursor: Option<u64>,
        received_at_ms: u64,
    },
    WriteCompleted {
        ticket: RequestTicket,
    },
    WriteFailed {
        ticket: Option<RequestTicket>,
        message: String,
    },
    End,
    /// Sent only after `quiesce` has stopped and joined both I/O producers.
    Quiesced,
}

/// A bounded account mailbox rejected this input or has closed.
#[derive(Debug)]
pub enum ProviderSessionInputSendError {
    /// The producer retains the input and waits until one queue slot is free.
    Full {
        input: ProviderSessionInput,
        capacity_available: Receiver<()>,
    },
    /// The mailbox is shutting down; the unacknowledged input remains replayable.
    Closed { input: ProviderSessionInput },
}

/// Nonblocking callback into the account actor's bounded input channel.
/// `Full` carries a one-shot notification so I/O workers can wait without
/// polling or buffering additional input.
pub type ProviderSessionInputSink = Arc<
    dyn Fn(ProviderSessionInput) -> Result<(), ProviderSessionInputSendError>
        + Send
        + Sync
        + 'static,
>;

fn send_input(sink: &ProviderSessionInputSink, mut input: ProviderSessionInput) -> io::Result<()> {
    loop {
        match sink(input) {
            Ok(()) => return Ok(()),
            Err(ProviderSessionInputSendError::Full {
                input: pending,
                capacity_available,
            }) => {
                capacity_available.recv().map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "provider input mailbox closed while waiting for capacity",
                    )
                })?;
                input = pending;
            }
            Err(ProviderSessionInputSendError::Closed { .. }) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "provider input mailbox is closed",
                ));
            }
        }
    }
}

struct LocalInputState {
    queue: VecDeque<ProviderSessionInput>,
    waiters: VecDeque<mpsc::Sender<()>>,
    closed: bool,
}

/// Bounded mailbox used by the synchronous convenience API and tests.
struct LocalInputMailbox {
    state: Mutex<LocalInputState>,
    ready: Condvar,
}

impl LocalInputMailbox {
    fn new() -> Self {
        Self {
            state: Mutex::new(LocalInputState {
                queue: VecDeque::new(),
                waiters: VecDeque::new(),
                closed: false,
            }),
            ready: Condvar::new(),
        }
    }

    #[allow(clippy::result_large_err)]
    fn send(&self, input: ProviderSessionInput) -> Result<(), ProviderSessionInputSendError> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(_) => return Err(ProviderSessionInputSendError::Closed { input }),
        };
        if state.closed && !matches!(input, ProviderSessionInput::Quiesced) {
            return Err(ProviderSessionInputSendError::Closed { input });
        }
        if state.queue.len() >= LOCAL_INPUT_CAPACITY
            && !matches!(input, ProviderSessionInput::Quiesced)
        {
            let (sender, capacity_available) = mpsc::channel();
            state.waiters.push_back(sender);
            return Err(ProviderSessionInputSendError::Full {
                input,
                capacity_available,
            });
        }
        state.queue.push_back(input);
        self.ready.notify_one();
        Ok(())
    }

    fn recv_timeout(
        &self,
        timeout: Duration,
    ) -> Result<ProviderSessionInput, mpsc::RecvTimeoutError> {
        let state = self
            .state
            .lock()
            .map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
        let (mut state, _wait_result) = self
            .ready
            .wait_timeout_while(state, timeout, |state| {
                state.queue.is_empty() && !state.closed
            })
            .map_err(|_| mpsc::RecvTimeoutError::Disconnected)?;
        if let Some(input) = state.queue.pop_front() {
            if let Some(waiter) = state.waiters.pop_front() {
                let _ignored = waiter.send(());
            }
            return Ok(input);
        }
        if state.closed {
            Err(mpsc::RecvTimeoutError::Disconnected)
        } else {
            Err(mpsc::RecvTimeoutError::Timeout)
        }
    }

    fn recv(&self) -> Result<ProviderSessionInput, ()> {
        let mut state = self.state.lock().map_err(|_| ())?;
        loop {
            if let Some(input) = state.queue.pop_front() {
                if let Some(waiter) = state.waiters.pop_front() {
                    let _ignored = waiter.send(());
                }
                return Ok(input);
            }
            if state.closed {
                return Err(());
            }
            state = self.ready.wait(state).map_err(|_| ())?;
        }
    }

    fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.closed = true;
            state.waiters.clear();
            self.ready.notify_all();
        }
    }
}

/// Configuration for one JSONL provider session.
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

/// Events delivered to the account actor. Consumer events carry a supervisor
/// cursor; the actor acknowledges it only after durable settlement.
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
    NotSent {
        ticket: RequestTicket,
    },
    OrphanReply {
        frame: Frame,
    },
    WriteCompleted {
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
    #[error("provider write was definitely not sent")]
    NotSent { ticket: RequestTicket },
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("provider session is closed")]
    Closed,
    #[error("provider cursor has not been delivered to the consumer")]
    CursorNotDelivered,
    #[error("provider session has no synchronous event receiver")]
    EventSourceUnavailable,
    #[error("provider I/O worker did not stop after cancellation")]
    CleanupTimedOut,
}

struct WriteCommand {
    bytes: Vec<u8>,
    ticket: Option<RequestTicket>,
}

/// One provider process and session state, owned by an account actor.
pub struct ProviderSession {
    process: Option<Box<dyn ProviderProcess>>,
    state: SessionState,
    writer: Option<SyncSender<WriteCommand>>,
    writer_thread: Option<WorkerHandle>,
    reader_thread: Option<WorkerHandle>,
    input_receiver: Option<Arc<LocalInputMailbox>>,
    input_sink: ProviderSessionInputSink,
    deadlines: BTreeMap<RequestId, u64>,
    queued: VecDeque<(ProviderSessionEvent, Option<u64>)>,
    tickets: BTreeMap<RequestId, RequestTicket>,
    ticket_cursors: BTreeMap<u64, RequestId>,
    completed_ids: BTreeSet<RequestId>,
    completed_order: VecDeque<RequestId>,
    seen_cursors: BTreeSet<u64>,
    delivered_cursors: BTreeSet<u64>,
    settled_cursors: BTreeSet<u64>,
    started: Instant,
    handled_cursor: u64,
    generation: u64,
    ready: bool,
    terminal: bool,
    quiesced: bool,
    cleanup_failed: bool,
}

impl ProviderSession {
    /// Validate recovered request identities and initialization encoding before
    /// opening a supervisor child. Constructors repeat this check defensively.
    pub fn validate_recovered(
        config: &ProviderSessionConfig,
        recovered: &[(RequestId, Method, Option<String>)],
    ) -> Result<(), ProviderSessionError> {
        SessionState::new_with_recovered_and_prefix(
            config.generation,
            config.claude,
            0,
            config.request_timeout_ms,
            config.initialize_params.clone(),
            recovered.to_vec(),
            None,
        )
        .map(|_| ())
        .map_err(ProviderSessionError::Session)
    }

    pub fn connect(
        process: Box<dyn ProviderProcess>,
        config: ProviderSessionConfig,
    ) -> Result<Self, ProviderSessionError> {
        Self::connect_recovering(process, config, Vec::new())
    }

    /// Synchronous convenience for tests. Production account actors should use
    /// `begin_connect_recovering` and route inputs through their account channel.
    pub fn connect_recovering(
        process: Box<dyn ProviderProcess>,
        config: ProviderSessionConfig,
        recovered: Vec<(RequestId, Method, Option<String>)>,
    ) -> Result<Self, ProviderSessionError> {
        let mailbox = Arc::new(LocalInputMailbox::new());
        let sink_mailbox = Arc::clone(&mailbox);
        #[allow(clippy::result_large_err)]
        let sink: ProviderSessionInputSink = Arc::new(move |input| sink_mailbox.send(input));
        let mut session = Self::begin_connect_recovering(process, config, recovered, sink)?;
        session.input_receiver = Some(mailbox);
        let mut deferred = VecDeque::new();
        let startup = (|| {
            while !session.ready {
                let Some(input) = session.recv_input_until_deadline()? else {
                    continue;
                };
                let events = session.process_input(input)?;
                if events
                    .iter()
                    .any(|(event, _)| matches!(event, ProviderSessionEvent::Closed { .. }))
                    || session.terminal
                {
                    return Err(ProviderSessionError::InitializeUnknown);
                }
                deferred.extend(events);
            }
            Ok(())
        })();
        if let Err(error) = startup {
            session.quiesce()?;
            return Err(error);
        }
        session.queued.extend(deferred);
        Ok(session)
    }

    /// Start reader/writer producers and queue initialize, returning immediately
    /// so the account actor can continue processing Wake, Stop and Shutdown.
    pub fn begin_connect_recovering(
        mut process: Box<dyn ProviderProcess>,
        config: ProviderSessionConfig,
        recovered: Vec<(RequestId, Method, Option<String>)>,
        input_sink: ProviderSessionInputSink,
    ) -> Result<Self, ProviderSessionError> {
        if let Err(error) = Self::validate_recovered(&config, &recovered) {
            let initialized = Self::connect_recovering(process, config, Vec::new())?;
            initialized.close()?;
            return Err(error);
        }
        let process_generation = process.generation();
        let generation = if process_generation == 0 {
            config.generation
        } else {
            process_generation
        };
        let handled_cursor = process.acknowledged_cursor();
        let namespace = process.resumed().then(|| request_id_namespace(generation));
        let (state, initial_actions) = match SessionState::new_with_recovered_and_prefix(
            generation,
            config.claude,
            0,
            config.request_timeout_ms,
            config.initialize_params,
            recovered.clone(),
            namespace,
        ) {
            Ok(state) => state,
            Err(error) => {
                let _ignored = process.cancel();
                let _ignored = process.stop();
                return Err(error.into());
            }
        };
        let Some(initialize_ticket) = initial_actions
            .iter()
            .find_map(|action| action_ticket(action, generation))
        else {
            let _ignored = process.cancel();
            let _ignored = process.stop();
            return Err(ProviderSessionError::Process(io::Error::other(
                "initialize request has no ticket",
            )));
        };
        let (reader, writer) = match process.split() {
            Ok(streams) => streams,
            Err(error) => {
                let _ignored = process.cancel();
                let _ignored = process.stop();
                return Err(ProviderSessionError::Process(error));
            }
        };
        let started = Instant::now();
        let reader_sink = Arc::clone(&input_sink);
        let reader_thread = match spawn_worker("provider-session-reader", move || {
            spawn_reader(reader, reader_sink, started);
        }) {
            Ok(thread) => thread,
            Err(error) => {
                let _ignored = process.cancel();
                let _ignored = process.stop();
                return Err(ProviderSessionError::Process(error));
            }
        };
        let (writer_sender, writer_receiver) = mpsc::sync_channel(WRITER_QUEUE_CAPACITY);
        let writer_sink = Arc::clone(&input_sink);
        let writer_thread = match spawn_worker("provider-session-writer", move || {
            spawn_writer(writer, writer_receiver, writer_sink);
        }) {
            Ok(thread) => thread,
            Err(error) => {
                let _ignored = process.cancel();
                let _ignored = process.stop();
                if !bounded_join(Some(reader_thread)) {
                    return Err(ProviderSessionError::CleanupTimedOut);
                }
                return Err(ProviderSessionError::Process(error));
            }
        };
        let mut tickets = BTreeMap::new();
        for (id, method, _) in recovered {
            tickets.insert(
                id.clone(),
                RequestTicket {
                    generation,
                    id,
                    method,
                },
            );
        }
        tickets.insert(initialize_ticket.id.clone(), initialize_ticket.clone());
        let mut session = Self {
            process: Some(process),
            state,
            writer: Some(writer_sender),
            writer_thread: Some(writer_thread),
            reader_thread: Some(reader_thread),
            input_receiver: None,
            input_sink,
            deadlines: BTreeMap::from([(
                initialize_ticket.id.clone(),
                now_ms(started).saturating_add(config.request_timeout_ms),
            )]),
            queued: VecDeque::new(),
            tickets,
            ticket_cursors: BTreeMap::new(),
            completed_ids: BTreeSet::new(),
            completed_order: VecDeque::new(),
            seen_cursors: BTreeSet::new(),
            delivered_cursors: BTreeSet::new(),
            settled_cursors: BTreeSet::new(),
            started,
            handled_cursor,
            generation,
            ready: false,
            terminal: false,
            quiesced: false,
            cleanup_failed: false,
        };
        let events = session.apply_actions(initial_actions, None);
        if session.terminal {
            session.quiesce()?;
            return Err(ProviderSessionError::InitializeUnknown);
        }
        session.queued.extend(events);
        Ok(session)
    }

    #[must_use]
    pub const fn is_ready(&self) -> bool {
        self.ready
    }

    /// Pre-encode a caller-allocated request before its durable Submitted marker.
    pub fn preflight_request_with_id(
        &self,
        id: RequestId,
        method: Method,
        params: Value,
        thread_id: Option<String>,
        lightweight: bool,
    ) -> Result<(), ProviderSessionError> {
        if !self.ready {
            return Err(SessionError::NotReady.into());
        }
        self.state
            .preflight_with_id(id, method, &params, thread_id.as_deref(), lightweight)
            .map_err(ProviderSessionError::Session)
    }

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
        self.register_ticket(ticket.clone(), timeout_ms);
        let events = self.apply_actions(actions, None);
        self.collect_async_events(events, Some(&ticket))
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
        self.register_ticket(ticket.clone(), timeout_ms);
        let events = self.apply_actions(actions, None);
        self.collect_async_events(events, Some(&ticket))
    }

    fn register_ticket(&mut self, ticket: RequestTicket, timeout_ms: u64) {
        self.deadlines.insert(
            ticket.id.clone(),
            now_ms(self.started).saturating_add(timeout_ms),
        );
        self.tickets.insert(ticket.id.clone(), ticket);
    }

    fn collect_async_events(
        &mut self,
        events: Vec<(ProviderSessionEvent, Option<u64>)>,
        submitted: Option<&RequestTicket>,
    ) -> Result<RequestTicket, ProviderSessionError> {
        let mut not_sent = None;
        for (event, cursor) in events {
            match event {
                ProviderSessionEvent::NotSent { ticket } if submitted == Some(&ticket) => {
                    not_sent = Some(ticket)
                }
                event => self.queued.push_back((event, cursor)),
            }
        }
        if let Some(ticket) = not_sent {
            Err(ProviderSessionError::NotSent { ticket })
        } else {
            submitted.cloned().ok_or(ProviderSessionError::Closed)
        }
    }

    pub fn reject_request(
        &mut self,
        id: RequestId,
        message: &str,
    ) -> Result<(), ProviderSessionError> {
        let bytes = encode_line(&Frame::Response {
            id,
            body: ResponseBody::Error(json!({"code": -32601, "message": message})),
            fields: Default::default(),
        })
        .map_err(|error| ProviderSessionError::Process(io::Error::other(error)))?;
        let events = self.enqueue(
            WriteCommand {
                bytes,
                ticket: None,
            },
            None,
        );
        self.queued.extend(events);
        if self.terminal {
            Err(ProviderSessionError::Closed)
        } else {
            Ok(())
        }
    }

    /// Process one item from the account actor's unified input channel.
    pub fn handle_input(
        &mut self,
        input: ProviderSessionInput,
    ) -> Result<Vec<(ProviderSessionEvent, Option<u64>)>, ProviderSessionError> {
        let events = self.process_input(input)?;
        self.deliver(events)
    }

    fn process_input(
        &mut self,
        input: ProviderSessionInput,
    ) -> Result<Vec<(ProviderSessionEvent, Option<u64>)>, ProviderSessionError> {
        match input {
            ProviderSessionInput::Frame {
                frame,
                cursor,
                received_at_ms,
            } => {
                // The account actor can be busy after the reader has received a
                // frame. Use its receive time so an expired request is marked
                // unknown before a queued reply is matched, independent of
                // when the actor gets around to handling that input.
                let (_, expired) = self.state.expire(received_at_ms);
                let mut events = self.apply_actions(expired, None);
                events.extend(self.process_frame(frame, cursor)?);
                Ok(events)
            }
            ProviderSessionInput::WriteCompleted { ticket }
                if ticket.method == Method::Initialize =>
            {
                Ok(Vec::new())
            }
            ProviderSessionInput::WriteCompleted { ticket } => Ok(vec![(
                ProviderSessionEvent::WriteCompleted { ticket },
                None,
            )]),
            ProviderSessionInput::WriteFailed { .. } | ProviderSessionInput::End => {
                Ok(self.fail_transport(None))
            }
            ProviderSessionInput::Quiesced => Ok(Vec::new()),
        }
    }

    fn process_frame(
        &mut self,
        frame: Frame,
        cursor: Option<u64>,
    ) -> Result<Vec<(ProviderSessionEvent, Option<u64>)>, ProviderSessionError> {
        if let Some(cursor) = cursor {
            if cursor <= self.handled_cursor {
                self.settle_internal(cursor)?;
                return Ok(Vec::new());
            }
            if !self.seen_cursors.insert(cursor) {
                return Ok(Vec::new());
            }
        }
        if let Frame::Response { id, .. } = &frame {
            if self.completed_ids.contains(id) {
                self.settle_internal_opt(cursor)?;
                return Ok(Vec::new());
            }
            if !self.tickets.contains_key(id) {
                return Ok(vec![(ProviderSessionEvent::OrphanReply { frame }, cursor)]);
            }
        }
        let actions = self.state.receive(frame)?;
        Ok(self.apply_actions(actions, cursor))
    }

    fn apply_actions(
        &mut self,
        actions: Vec<Action>,
        cursor: Option<u64>,
    ) -> Vec<(ProviderSessionEvent, Option<u64>)> {
        let mut events = Vec::new();
        for action in actions {
            match action {
                Action::Write(frame) => match encode_line(&frame) {
                    Ok(bytes) => {
                        let ticket =
                            request_id(&frame).and_then(|id| self.tickets.get(&id).cloned());
                        events.extend(self.enqueue(WriteCommand { bytes, ticket }, cursor));
                    }
                    Err(_) => events.extend(self.fail_transport(None)),
                },
                Action::Resolve { ticket, outcome } => {
                    self.deadlines.remove(&ticket.id);
                    if let Some(cursor) = cursor {
                        self.ticket_cursors.insert(cursor, ticket.id.clone());
                    }
                    match outcome {
                        RequestOutcome::Reply { body, late } => {
                            self.remember_completed(ticket.id.clone());
                            if ticket.method == Method::Initialize {
                                self.ready = matches!(body, ResponseBody::Result(_));
                                let initialized = self.settle_internal_opt(cursor);
                                if initialized.is_err() {
                                    events.extend(self.fail_transport(None));
                                }
                                if !self.ready {
                                    events.extend(self.fail_transport(None));
                                }
                            } else {
                                events.push((
                                    ProviderSessionEvent::Reply { ticket, body, late },
                                    cursor,
                                ));
                            }
                        }
                        RequestOutcome::Unknown => {
                            if ticket.method == Method::Initialize {
                                events.extend(self.fail_transport(None));
                            } else {
                                events.push((
                                    ProviderSessionEvent::UnknownOutcome { ticket },
                                    cursor,
                                ));
                            }
                        }
                    }
                }
                Action::Dispatch(frame) => {
                    events.push((ProviderSessionEvent::Inbound(to_inbound(frame)), cursor))
                }
                Action::GenerationClosed { generation } => {
                    if !self.terminal {
                        self.terminal = true;
                        events.push((ProviderSessionEvent::Closed { generation }, None));
                    }
                }
                Action::ThreadReleased { .. } => {}
            }
        }
        events
    }

    fn enqueue(
        &mut self,
        command: WriteCommand,
        cursor: Option<u64>,
    ) -> Vec<(ProviderSessionEvent, Option<u64>)> {
        let ticket = command.ticket.clone();
        let result = self.writer.as_ref().map(|writer| writer.try_send(command));
        match result {
            Some(Ok(())) => Vec::new(),
            Some(Err(TrySendError::Full(command)))
            | Some(Err(TrySendError::Disconnected(command))) => {
                let mut events = Vec::new();
                if let Some(ticket) = command.ticket {
                    events.push((
                        ProviderSessionEvent::NotSent {
                            ticket: ticket.clone(),
                        },
                        None,
                    ));
                    events.extend(self.fail_transport(Some(ticket.id)));
                } else {
                    events.extend(self.fail_transport(None));
                }
                events
            }
            None => {
                let mut events =
                    self.fail_transport(ticket.as_ref().map(|ticket| ticket.id.clone()));
                if let Some(ticket) = ticket {
                    events.insert(0, (ProviderSessionEvent::NotSent { ticket }, None));
                }
                let _ = cursor;
                events
            }
        }
    }

    fn remember_completed(&mut self, id: RequestId) {
        if self.completed_ids.insert(id.clone()) {
            self.completed_order.push_back(id);
        }
        while self.completed_order.len() > COMPLETED_ID_LIMIT {
            if let Some(oldest) = self.completed_order.pop_front() {
                self.completed_ids.remove(&oldest);
            }
        }
    }

    fn fail_transport(
        &mut self,
        not_sent: Option<RequestId>,
    ) -> Vec<(ProviderSessionEvent, Option<u64>)> {
        if self.terminal {
            return Vec::new();
        }
        self.terminal = true;
        self.deadlines.clear();
        let mut events = Vec::new();
        for action in self.state.transport_lost() {
            if let Action::Resolve {
                ticket,
                outcome: RequestOutcome::Unknown,
            } = action
                && not_sent.as_ref() != Some(&ticket.id)
                && ticket.method != Method::Initialize
            {
                events.push((ProviderSessionEvent::UnknownOutcome { ticket }, None));
            }
        }
        events.push((
            ProviderSessionEvent::Closed {
                generation: self.generation,
            },
            None,
        ));
        events
    }

    /// Mark a consumer event durable. Only the contiguous settled prefix is
    /// acknowledged; later settled cursors wait behind any earlier gap.
    pub fn acknowledge_handled(&mut self, cursor: u64) -> Result<(), ProviderSessionError> {
        if !self.delivered_cursors.contains(&cursor) {
            return Err(ProviderSessionError::CursorNotDelivered);
        }
        self.settled_cursors.insert(cursor);
        self.advance_acknowledged_prefix()
    }

    fn settle_internal_opt(&mut self, cursor: Option<u64>) -> Result<(), ProviderSessionError> {
        if let Some(cursor) = cursor {
            self.settle_internal(cursor)?;
        }
        Ok(())
    }

    fn settle_internal(&mut self, cursor: u64) -> Result<(), ProviderSessionError> {
        self.settled_cursors.insert(cursor);
        self.advance_acknowledged_prefix()
    }

    fn advance_acknowledged_prefix(&mut self) -> Result<(), ProviderSessionError> {
        let process = self.process.as_mut().ok_or(ProviderSessionError::Closed)?;
        self.handled_cursor = self.handled_cursor.max(process.acknowledged_cursor());
        self.settled_cursors
            .retain(|cursor| *cursor > self.handled_cursor);
        let mut target = self.handled_cursor;
        while target
            .checked_add(1)
            .is_some_and(|next| self.settled_cursors.contains(&next))
        {
            target += 1;
        }
        if target > self.handled_cursor {
            process
                .acknowledge(target)
                .map_err(ProviderSessionError::Process)?;
            self.handled_cursor = self.handled_cursor.max(process.acknowledged_cursor());
            let settled_ids = self
                .ticket_cursors
                .range(..=self.handled_cursor)
                .map(|(_, id)| id.clone())
                .collect::<Vec<_>>();
            self.ticket_cursors
                .retain(|cursor, _| *cursor > self.handled_cursor);
            for id in settled_ids {
                self.tickets.remove(&id);
            }
            self.seen_cursors
                .retain(|cursor| *cursor > self.handled_cursor);
            self.delivered_cursors
                .retain(|cursor| *cursor > self.handled_cursor);
            self.settled_cursors
                .retain(|cursor| *cursor > self.handled_cursor);
        }
        Ok(())
    }

    /// The current contiguous acknowledged prefix. It can trail higher cursors
    /// already settled durably while an earlier event remains unsettled.
    #[must_use]
    pub const fn handled_cursor(&self) -> u64 {
        self.handled_cursor
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn take_events(&mut self) -> Vec<(ProviderSessionEvent, Option<u64>)> {
        let events = self.queued.drain(..).collect();
        self.deliver(events).unwrap_or_default()
    }

    fn deliver(
        &mut self,
        events: Vec<(ProviderSessionEvent, Option<u64>)>,
    ) -> Result<Vec<(ProviderSessionEvent, Option<u64>)>, ProviderSessionError> {
        for (_, cursor) in &events {
            if let Some(cursor) = cursor {
                self.delivered_cursors.insert(*cursor);
            }
        }
        Ok(events)
    }

    /// Earliest request deadline for the account worker's `recv_timeout`.
    #[must_use]
    pub fn next_deadline(&self) -> Option<Duration> {
        self.deadlines
            .values()
            .min()
            .map(|deadline| Duration::from_millis(deadline.saturating_sub(now_ms(self.started))))
    }

    pub fn handle_deadline(&mut self) -> Vec<(ProviderSessionEvent, Option<u64>)> {
        let (result, actions) = self.state.expire(now_ms(self.started));
        let events = self.apply_actions(actions, None);
        let _deadline_result = result;
        self.deliver(events).unwrap_or_default()
    }

    /// Wait for one event (synchronous convenience for tests).
    pub fn receive_event(
        &mut self,
    ) -> Result<(ProviderSessionEvent, Option<u64>), ProviderSessionError> {
        loop {
            if let Some(event) = self.pop_event() {
                return Ok(event);
            }
            if self.terminal {
                return Err(ProviderSessionError::Closed);
            }
            let Some(input) = self.recv_input_until_deadline()? else {
                continue;
            };
            let events = self.process_input(input)?;
            self.queued.extend(events);
        }
    }

    pub fn receive_event_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<(ProviderSessionEvent, Option<u64>)>, ProviderSessionError> {
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if let Some(event) = self.pop_event() {
                return Ok(Some(event));
            }
            if self.terminal {
                return Err(ProviderSessionError::Closed);
            }
            let remaining = deadline
                .map(|end| end.saturating_duration_since(Instant::now()))
                .unwrap_or(timeout);
            if remaining.is_zero() {
                return Ok(None);
            }
            let request_deadline = self.next_deadline();
            let wait = request_deadline.map_or(remaining, |value| value.min(remaining));
            let Some(input) = self.recv_input(wait)? else {
                if request_deadline.is_some_and(|value| value <= remaining) {
                    let events = self.handle_deadline();
                    self.queued.extend(events);
                    if !self.queued.is_empty() {
                        continue;
                    }
                }
                return Ok(None);
            };
            let events = self.process_input(input)?;
            self.queued.extend(events);
        }
    }

    fn pop_event(&mut self) -> Option<(ProviderSessionEvent, Option<u64>)> {
        let event = self.queued.pop_front()?;
        if let Some(cursor) = event.1 {
            self.delivered_cursors.insert(cursor);
        }
        Some(event)
    }

    fn recv_input_until_deadline(
        &mut self,
    ) -> Result<Option<ProviderSessionInput>, ProviderSessionError> {
        let receiver = self
            .input_receiver
            .as_ref()
            .ok_or(ProviderSessionError::EventSourceUnavailable)?;
        let received = match self.next_deadline() {
            Some(timeout) => receiver.recv_timeout(timeout),
            None => receiver
                .recv()
                .map_err(|()| mpsc::RecvTimeoutError::Disconnected),
        };
        match received {
            Ok(input) => Ok(Some(input)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let events = self.handle_deadline();
                self.queued.extend(events);
                if !self.ready && self.terminal {
                    Err(ProviderSessionError::InitializeUnknown)
                } else {
                    Ok(None)
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ProviderSessionError::Closed),
        }
    }

    fn recv_input(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<ProviderSessionInput>, ProviderSessionError> {
        let receiver = self
            .input_receiver
            .as_ref()
            .ok_or(ProviderSessionError::EventSourceUnavailable)?;
        match receiver.recv_timeout(timeout) {
            Ok(input) => Ok(Some(input)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ProviderSessionError::Closed),
        }
    }

    /// Stop both producers and emit a FIFO barrier to the account channel. The
    /// actor can drain already-posted frames before it commits disconnect state.
    pub fn quiesce(&mut self) -> Result<(), ProviderSessionError> {
        if self.quiesced {
            return Ok(());
        }
        if self.cleanup_failed {
            return Err(ProviderSessionError::CleanupTimedOut);
        }
        self.writer.take();
        if let Some(receiver) = &self.input_receiver {
            receiver.close();
        }
        let mut stop_error = None;
        if let Some(process) = self.process.as_mut() {
            if let Err(error) = process.cancel() {
                stop_error = Some(error);
            }
            if let Err(error) = process.stop() {
                stop_error.get_or_insert(error);
            }
        }
        let writer_stopped = bounded_join(self.writer_thread.take());
        let reader_stopped = bounded_join(self.reader_thread.take());
        if !writer_stopped || !reader_stopped {
            self.cleanup_failed = true;
            return Err(ProviderSessionError::CleanupTimedOut);
        }
        self.quiesced = true;
        let _ignored = send_input(&self.input_sink, ProviderSessionInput::Quiesced);
        if let Some(error) = stop_error {
            return Err(ProviderSessionError::Process(error));
        }
        Ok(())
    }

    pub fn close(mut self) -> Result<(), ProviderSessionError> {
        self.quiesce()
    }
}

impl Drop for ProviderSession {
    fn drop(&mut self) {
        if !self.quiesced {
            let _ignored = self.quiesce();
        }
    }
}

fn action_ticket(action: &Action, generation: u64) -> Option<RequestTicket> {
    match action {
        Action::Write(frame) => Some(RequestTicket {
            generation,
            id: request_id(frame)?,
            method: Method::Initialize,
        }),
        _ => None,
    }
}

fn request_id(frame: &Frame) -> Option<RequestId> {
    match frame {
        Frame::Request(request) => Some(request.id.clone()),
        _ => None,
    }
}

fn request_id_namespace(generation: u64) -> String {
    static NEXT_NAMESPACE: AtomicU64 = AtomicU64::new(1);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NEXT_NAMESPACE.fetch_add(1, Ordering::Relaxed);
    format!(
        "studio:{generation}:{}:{timestamp}:{sequence}",
        std::process::id()
    )
}

fn spawn_reader(
    mut reader: Box<dyn ProviderProcessReader>,
    sink: ProviderSessionInputSink,
    started: Instant,
) {
    let mut decoder = LineDecoder::new();
    let mut bytes = [0_u8; 8192];
    loop {
        match reader.read(&mut bytes) {
            Ok(0) => {
                let _ignored = send_input(&sink, ProviderSessionInput::End);
                return;
            }
            Ok(count) => match decoder.push(&bytes[..count]) {
                Ok(frames) => {
                    let cursors = reader.take_cursors();
                    let received_at_ms = now_ms(started);
                    let cursors = if cursors.len() == frames.len() {
                        cursors.into_iter().map(Some).collect::<Vec<_>>()
                    } else if cursors.is_empty() {
                        vec![None; frames.len()]
                    } else {
                        let _ignored = send_input(&sink, ProviderSessionInput::End);
                        return;
                    };
                    for (frame, cursor) in frames.into_iter().zip(cursors) {
                        if send_input(
                            &sink,
                            ProviderSessionInput::Frame {
                                frame,
                                cursor,
                                received_at_ms,
                            },
                        )
                        .is_err()
                        {
                            return;
                        }
                    }
                }
                Err(_) => {
                    let _ignored = send_input(&sink, ProviderSessionInput::End);
                    return;
                }
            },
            Err(_) => {
                let _ignored = send_input(&sink, ProviderSessionInput::End);
                return;
            }
        }
    }
}

fn spawn_writer(
    mut writer: Box<dyn Write + Send>,
    receiver: Receiver<WriteCommand>,
    sink: ProviderSessionInputSink,
) {
    while let Ok(command) = receiver.recv() {
        let ticket = command.ticket.clone();
        if let Err(error) = writer
            .write_all(&command.bytes)
            .and_then(|()| writer.flush())
        {
            let _ignored = send_input(
                &sink,
                ProviderSessionInput::WriteFailed {
                    ticket,
                    message: error.to_string(),
                },
            );
            return;
        }
        if let Some(ticket) = ticket
            && send_input(&sink, ProviderSessionInput::WriteCompleted { ticket }).is_err()
        {
            return;
        }
    }
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

struct WorkerHandle {
    join: JoinHandle<()>,
    finished: Receiver<()>,
}

fn spawn_worker(name: &str, worker: impl FnOnce() + Send + 'static) -> io::Result<WorkerHandle> {
    let (finished_sender, finished) = mpsc::channel();
    let join = thread::Builder::new().name(name.into()).spawn(move || {
        worker();
        let _ignored = finished_sender.send(());
    })?;
    Ok(WorkerHandle { join, finished })
}

fn bounded_join(worker: Option<WorkerHandle>) -> bool {
    let Some(worker) = worker else {
        return true;
    };
    if worker.finished.recv_timeout(WORKER_JOIN_LIMIT).is_ok() {
        worker.join.join().is_ok()
    } else {
        drop(worker.join);
        false
    }
}

fn now_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
