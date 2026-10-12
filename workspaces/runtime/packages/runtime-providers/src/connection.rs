//! Pure, deterministic provider connection state machine.

use std::collections::{BTreeMap, VecDeque};
use std::io::{Read, Write};
use std::sync::{
    Mutex,
    mpsc::{self, Receiver},
};
use std::thread;

use serde_json::{Map, Value};
use thiserror::Error;

use crate::{
    BookError, CallbackDispatcher, CodecError, Frame, Handler, Inbound, Method, PendingBook,
    RequestFrame, RequestId, ResponseBody, ResponseMatch, encode_line,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lifecycle {
    Initializing,
    Ready,
    Lost,
    Closed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestTicket {
    pub generation: u64,
    pub id: RequestId,
    pub method: Method,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RequestOutcome {
    Reply { body: ResponseBody, late: bool },
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Write(Vec<u8>),
    Resolve {
        ticket: RequestTicket,
        outcome: RequestOutcome,
    },
    Dispatch(Frame),
    GenerationClosed {
        generation: u64,
    },
    ThreadReleased {
        thread_id: String,
        ticket: RequestTicket,
    },
}

#[derive(Debug, Error, PartialEq)]
pub enum SessionError {
    #[error(transparent)]
    Book(#[from] BookError),
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("connection is not ready")]
    NotReady,
    #[error("request ticket belongs to a different generation")]
    StaleGeneration,
    #[error("request deadline elapsed; unresolved outcomes are unknown")]
    DeadlineUnknown,
    #[error("registered request is missing from session state")]
    MissingRequest,
}

struct Pending {
    ticket: RequestTicket,
    params: Value,
    deadline_ms: u64,
    thread_id: Option<String>,
    serialized: bool,
}

/// Session decisions are independent of threads and byte I/O. Call `submit`,
/// `receive`, `expire`, `cancel`, or `transport_lost`, then execute returned actions.
pub struct SessionState {
    generation: u64,
    claude: bool,
    next_claude_id: u64,
    lifecycle: Lifecycle,
    book: PendingBook,
    pending: BTreeMap<RequestId, Pending>,
    active_threads: BTreeMap<String, RequestId>,
    queued_threads: BTreeMap<String, VecDeque<RequestId>>,
}

impl SessionState {
    pub fn new(
        generation: u64,
        claude: bool,
        now_ms: u64,
        timeout_ms: u64,
        initialize_params: Value,
    ) -> Result<(Self, Vec<Action>), SessionError> {
        let mut state = Self {
            generation,
            claude,
            next_claude_id: 1,
            lifecycle: Lifecycle::Initializing,
            book: PendingBook::new(),
            pending: BTreeMap::new(),
            active_threads: BTreeMap::new(),
            queued_threads: BTreeMap::new(),
        };
        let ticket = state.register(
            Method::Initialize,
            initialize_params,
            now_ms,
            timeout_ms,
            None,
            false,
        )?;
        let actions = state.write_request(&ticket)?;
        Ok((state, actions))
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn lifecycle(&self) -> Lifecycle {
        self.lifecycle
    }

    pub fn submit(
        &mut self,
        method: Method,
        params: Value,
        now_ms: u64,
        timeout_ms: u64,
        thread_id: Option<String>,
        lightweight_read: bool,
    ) -> Result<(RequestTicket, Vec<Action>), SessionError> {
        if self.lifecycle != Lifecycle::Ready {
            return Err(SessionError::NotReady);
        }
        let serialized = self.claude && !lightweight_read && is_thread_serialized(method);
        let ticket = self.register(method, params, now_ms, timeout_ms, thread_id, serialized)?;
        let mut actions = Vec::new();
        let pending = self
            .pending
            .get(&ticket.id)
            .ok_or(SessionError::MissingRequest)?;
        if serialized {
            let thread = pending.thread_id.clone().unwrap_or_default();
            if let std::collections::btree_map::Entry::Vacant(entry) =
                self.active_threads.entry(thread.clone())
            {
                entry.insert(ticket.id.clone());
                actions.extend(self.write_request(&ticket)?);
            } else {
                self.queued_threads
                    .entry(thread)
                    .or_default()
                    .push_back(ticket.id.clone());
            }
        } else {
            actions.extend(self.write_request(&ticket)?);
        }
        Ok((ticket, actions))
    }

    fn register(
        &mut self,
        method: Method,
        params: Value,
        now_ms: u64,
        timeout_ms: u64,
        thread_id: Option<String>,
        serialized: bool,
    ) -> Result<RequestTicket, SessionError> {
        let id = if self.claude {
            let id = RequestId::String(format!("claude:{:032x}", self.next_claude_id));
            self.next_claude_id = self
                .next_claude_id
                .checked_add(1)
                .ok_or(BookError::IdExhausted)?;
            self.book.register(id.clone(), method)?;
            id
        } else {
            self.book.allocate_numeric(method)?
        };
        let ticket = RequestTicket {
            generation: self.generation,
            id: id.clone(),
            method,
        };
        self.pending.insert(
            id,
            Pending {
                ticket: ticket.clone(),
                params,
                deadline_ms: now_ms.saturating_add(timeout_ms),
                thread_id,
                serialized,
            },
        );
        Ok(ticket)
    }

    fn write_request(&self, ticket: &RequestTicket) -> Result<Vec<Action>, SessionError> {
        let pending = self
            .pending
            .get(&ticket.id)
            .ok_or(SessionError::MissingRequest)?;
        let bytes = encode_line(&Frame::Request(RequestFrame {
            id: ticket.id.clone(),
            method: ticket.method,
            params: Some(pending.params.clone()),
            fields: Map::new(),
        }))?;
        Ok(vec![Action::Write(bytes)])
    }

    pub fn receive(&mut self, frame: Frame) -> Result<Vec<Action>, SessionError> {
        if self.lifecycle == Lifecycle::Closed {
            return Ok(Vec::new());
        }
        match frame {
            Frame::Response { id, body, .. } => {
                let matched = match self.book.observe_response(&id) {
                    Ok(matched) => matched,
                    Err(BookError::UnknownId | BookError::DuplicateResponse) => {
                        return Ok(Vec::new());
                    }
                    Err(error) => return Err(error.into()),
                };
                let Some(pending) = self.pending.remove(&id) else {
                    return Ok(Vec::new());
                };
                let late = matches!(matched, ResponseMatch::Late { .. });
                let mut actions = vec![Action::Resolve {
                    ticket: pending.ticket.clone(),
                    outcome: RequestOutcome::Reply { body, late },
                }];
                if pending.ticket.method == Method::Initialize
                    && self.lifecycle == Lifecycle::Initializing
                {
                    self.lifecycle = Lifecycle::Ready;
                    actions.push(Action::Write(encode_line(&Frame::Notification(
                        crate::NotificationFrame {
                            method: Method::Initialized,
                            params: None,
                            fields: Map::new(),
                        },
                    ))?));
                }
                if pending.serialized
                    && let Some(thread) = pending.thread_id
                {
                    actions.extend(self.release_thread(&thread)?);
                }
                Ok(actions)
            }
            other => Ok(vec![Action::Dispatch(other)]),
        }
    }

    pub fn expire(&mut self, now_ms: u64) -> (Result<(), SessionError>, Vec<Action>) {
        if matches!(self.lifecycle, Lifecycle::Lost | Lifecycle::Closed) {
            return (Ok(()), Vec::new());
        }
        if !self
            .pending
            .values()
            .any(|pending| now_ms >= pending.deadline_ms)
        {
            return (Ok(()), Vec::new());
        }
        (
            Err(SessionError::DeadlineUnknown),
            self.mark_unknown(Lifecycle::Lost),
        )
    }

    pub fn transport_lost(&mut self) -> Vec<Action> {
        self.mark_unknown(Lifecycle::Lost)
    }

    pub fn close(&mut self) -> Vec<Action> {
        self.mark_unknown(Lifecycle::Closed)
    }

    fn mark_unknown(&mut self, lifecycle: Lifecycle) -> Vec<Action> {
        if matches!(self.lifecycle, Lifecycle::Lost | Lifecycle::Closed) {
            return Vec::new();
        }
        self.book.mark_connection_lost();
        self.lifecycle = lifecycle;
        self.active_threads.clear();
        self.queued_threads.clear();
        let mut actions = Vec::with_capacity(self.pending.len() + 1);
        for pending in self.pending.values_mut() {
            pending.deadline_ms = u64::MAX;
            actions.push(Action::Resolve {
                ticket: pending.ticket.clone(),
                outcome: RequestOutcome::Unknown,
            });
        }
        actions.push(Action::GenerationClosed {
            generation: self.generation,
        });
        if lifecycle == Lifecycle::Closed {
            self.pending.clear();
        }
        actions
    }

    pub fn cancel(&mut self, ticket: &RequestTicket) -> Result<Vec<Action>, SessionError> {
        if ticket.generation != self.generation {
            return Err(SessionError::StaleGeneration);
        }
        if !self.claude {
            return Err(SessionError::Book(BookError::UnknownId));
        }
        self.book.cancel(&ticket.id)?;
        let Some(pending) = self.pending.remove(&ticket.id) else {
            return Ok(Vec::new());
        };
        if !pending.serialized {
            return Ok(Vec::new());
        }
        let Some(thread) = pending.thread_id else {
            return Ok(Vec::new());
        };
        if self.active_threads.get(&thread) == Some(&ticket.id) {
            self.release_thread(&thread)
        } else {
            if let Some(queue) = self.queued_threads.get_mut(&thread) {
                queue.retain(|id| id != &ticket.id);
            }
            Ok(Vec::new())
        }
    }

    fn release_thread(&mut self, thread: &str) -> Result<Vec<Action>, SessionError> {
        let Some(queue) = self.queued_threads.get_mut(thread) else {
            self.active_threads.remove(thread);
            return Ok(Vec::new());
        };
        while let Some(id) = queue.pop_front() {
            if let Some(pending) = self.pending.get(&id) {
                self.active_threads.insert(thread.to_owned(), id);
                let ticket = pending.ticket.clone();
                let mut actions = vec![Action::ThreadReleased {
                    thread_id: thread.to_owned(),
                    ticket: ticket.clone(),
                }];
                actions.extend(self.write_request(&ticket)?);
                return Ok(actions);
            }
        }
        self.queued_threads.remove(thread);
        self.active_threads.remove(thread);
        Ok(Vec::new())
    }
}

fn is_thread_serialized(method: Method) -> bool {
    matches!(
        method,
        Method::TurnStart
            | Method::TurnSteer
            | Method::ThreadResume
            | Method::ThreadFork
            | Method::ThreadRead
    )
}

enum ReadEvent {
    Frame(Frame),
    End,
    Failure(CodecError),
}

/// Dedicated reader thread for one owned read half. Callback work is never run here.
pub struct ReaderThread {
    events: Receiver<ReadEvent>,
    worker: thread::JoinHandle<()>,
}

impl ReaderThread {
    pub fn start<R: Read + Send + 'static>(mut reader: R) -> Self {
        let (sender, events) = mpsc::sync_channel(65_536);
        let worker = thread::spawn(move || {
            let mut decoder = crate::LineDecoder::new();
            let mut bytes = [0_u8; 8192];
            loop {
                match reader.read(&mut bytes) {
                    Ok(0) => {
                        if let Err(error) = decoder.finish()
                            && sender.send(ReadEvent::Failure(error)).is_err()
                        {
                            break;
                        }
                        if sender.send(ReadEvent::End).is_err() {
                            break;
                        }
                        break;
                    }
                    Ok(count) => match decoder.push(&bytes[..count]) {
                        Ok(frames) => {
                            if frames
                                .into_iter()
                                .any(|frame| sender.send(ReadEvent::Frame(frame)).is_err())
                            {
                                break;
                            }
                        }
                        Err(error) => {
                            if sender.send(ReadEvent::Failure(error)).is_err() {
                                break;
                            }
                            break;
                        }
                    },
                    Err(_) => {
                        if sender.send(ReadEvent::End).is_err() {
                            break;
                        }
                        break;
                    }
                }
            }
        });
        Self { events, worker }
    }

    fn stop(self) {
        let Self { events, worker } = self;
        drop(events);
        let _joined = worker.join();
    }
}

/// Split-I/O adapter. The reader and callback workers are independent; each
/// write action is serialized under one mutex before it reaches the write half.
pub struct ConnectionDriver<R, W, H>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    H: Handler + Send + 'static,
{
    reader: ReaderThread,
    writer: Mutex<W>,
    state: SessionState,
    dispatcher: CallbackDispatcher<H>,
    _reader_type: std::marker::PhantomData<R>,
}

pub struct DriverConfig {
    pub generation: u64,
    pub claude: bool,
    pub now_ms: u64,
    pub timeout_ms: u64,
    pub initialize_params: Value,
}

impl<R, W, H> ConnectionDriver<R, W, H>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
    H: Handler + Send + 'static,
{
    pub fn start(
        reader: R,
        writer: W,
        handler: H,
        config: DriverConfig,
    ) -> Result<Self, DriverError> {
        let (state, actions) = SessionState::new(
            config.generation,
            config.claude,
            config.now_ms,
            config.timeout_ms,
            config.initialize_params,
        )?;
        let mut driver = Self {
            reader: ReaderThread::start(reader),
            writer: Mutex::new(writer),
            state,
            dispatcher: CallbackDispatcher::new(handler),
            _reader_type: std::marker::PhantomData,
        };
        driver.apply(actions)?;
        Ok(driver)
    }

    pub fn submit(
        &mut self,
        method: Method,
        params: Value,
        thread_id: Option<String>,
        lightweight: bool,
        now_ms: u64,
        timeout_ms: u64,
    ) -> Result<(RequestTicket, Vec<Action>), DriverError> {
        let (ticket, actions) =
            self.state
                .submit(method, params, now_ms, timeout_ms, thread_id, lightweight)?;
        Ok((ticket, self.apply(actions)?))
    }

    pub fn pump_one(&mut self, now_ms: u64) -> Result<Vec<Action>, DriverError> {
        match self.reader.events.try_recv() {
            Ok(ReadEvent::Frame(frame)) => {
                let actions = self.state.receive(frame)?;
                self.apply(actions)
            }
            Ok(ReadEvent::Failure(error)) => {
                drop(error);
                Ok(self.state.transport_lost())
            }
            Ok(ReadEvent::End) => Ok(self.state.transport_lost()),
            Err(mpsc::TryRecvError::Empty) => {
                let (result, actions) = self.state.expire(now_ms);
                self.apply(actions)?;
                result.map(|()| Vec::new()).map_err(DriverError::Session)
            }
            Err(mpsc::TryRecvError::Disconnected) => Ok(self.state.transport_lost()),
        }
    }

    /// Blocking variant for consumers that drive the session from one worker.
    pub fn pump_next(&mut self, now_ms: u64) -> Result<Vec<Action>, DriverError> {
        match self
            .reader
            .events
            .recv()
            .map_err(|_| DriverError::Dispatcher)?
        {
            ReadEvent::Frame(frame) => {
                let actions = self.state.receive(frame)?;
                self.apply(actions)
            }
            ReadEvent::Failure(error) => {
                drop(error);
                Ok(self.state.transport_lost())
            }
            ReadEvent::End => Ok(self.state.transport_lost()),
        }
        .and_then(|actions| {
            let (result, expired) = self.state.expire(now_ms);
            if !expired.is_empty() {
                let mut actions = actions;
                actions.extend(self.apply(expired)?);
                result.map(|()| actions).map_err(DriverError::Session)
            } else {
                Ok(actions)
            }
        })
    }

    pub fn cancel(&mut self, ticket: &RequestTicket) -> Result<Vec<Action>, DriverError> {
        let actions = self.state.cancel(ticket)?;
        self.apply(actions)
    }

    pub fn close(mut self) -> Result<(), DriverError> {
        let actions = self.state.close();
        self.apply(actions)?;
        let Self {
            reader, dispatcher, ..
        } = self;
        reader.stop();
        dispatcher.close().map_err(|_| DriverError::Dispatcher)
    }

    fn apply(&mut self, actions: Vec<Action>) -> Result<Vec<Action>, DriverError> {
        let mut unhandled = Vec::new();
        for action in actions {
            match action {
                Action::Write(bytes) => {
                    let mut writer = self
                        .writer
                        .lock()
                        .map_err(|_| DriverError::WriterPoisoned)?;
                    writer
                        .write_all(&bytes)
                        .and_then(|()| writer.flush())
                        .map_err(|source| DriverError::Io {
                            source,
                            unresolved: self.state.transport_lost(),
                        })?;
                }
                Action::Dispatch(frame) => {
                    let inbound = match frame {
                        Frame::Request(request) => Inbound::Request(request),
                        Frame::Notification(notification) => {
                            Inbound::Notification(Frame::Notification(notification))
                        }
                        other => Inbound::Notification(other),
                    };
                    self.dispatcher
                        .send(inbound)
                        .map_err(|_| DriverError::Dispatcher)?;
                }
                other => unhandled.push(other),
            }
        }
        Ok(unhandled)
    }
}

#[derive(Debug, Error)]
pub enum DriverError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("provider write half mutex was poisoned")]
    WriterPoisoned,
    #[error("provider byte write failed; unresolved requests have unknown outcomes")]
    Io {
        #[source]
        source: std::io::Error,
        unresolved: Vec<Action>,
    },
    #[error("provider callback lane failed")]
    Dispatcher,
}
