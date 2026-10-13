//! Pure, deterministic provider connection state machine.

use serde_json::{Map, Value};
use std::collections::{BTreeMap, VecDeque};
use thiserror::Error;

use crate::{
    BookError, CodecError, Frame, Method, PendingBook, RequestFrame, RequestId, RequestState,
    ResponseBody, ResponseMatch, encode_line, validate_initialize_params,
};

/// Provider request or notification delivered to the account session owner.
#[derive(Clone, Debug, PartialEq)]
pub enum Inbound {
    Request(RequestFrame),
    Notification(Frame),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lifecycle {
    Initializing,
    Ready,
    Lost,
    Closed,
}

/// Progress of the provider initialization handshake. `Ready` means both the
/// initialize reply was accepted and the required `initialized` write settled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitializationStatus {
    AwaitingReply,
    InitializedWritePending,
    Ready,
    Refused,
    Unknown,
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
    #[error("Claude thread-serialized request requires a thread id")]
    MissingThreadId,
    #[error("initialized notification cannot be marked written in this state")]
    InitializationNotReady,
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
    request_id_prefix: Option<String>,
    next_request_sequence: u64,
    lifecycle: Lifecycle,
    initialization_status: InitializationStatus,
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
        Self::new_with_recovered(
            generation,
            claude,
            now_ms,
            timeout_ms,
            initialize_params,
            Vec::new(),
        )
    }

    pub fn new_with_recovered(
        generation: u64,
        claude: bool,
        now_ms: u64,
        timeout_ms: u64,
        initialize_params: Value,
        recovered: Vec<(RequestId, Method, Option<String>)>,
    ) -> Result<(Self, Vec<Action>), SessionError> {
        Self::new_with_recovered_and_prefix(
            generation,
            claude,
            now_ms,
            timeout_ms,
            initialize_params,
            recovered,
            None,
        )
    }

    /// Initialize with a connection-scoped request ID namespace. This keeps
    /// late replies from an older attachment from matching fresh requests.
    pub fn new_with_recovered_and_prefix(
        generation: u64,
        claude: bool,
        now_ms: u64,
        timeout_ms: u64,
        initialize_params: Value,
        recovered: Vec<(RequestId, Method, Option<String>)>,
        request_id_prefix: Option<String>,
    ) -> Result<(Self, Vec<Action>), SessionError> {
        validate_initialize_params(&initialize_params)?;
        let mut state = Self {
            generation,
            claude,
            next_claude_id: 1,
            request_id_prefix,
            next_request_sequence: 1,
            lifecycle: Lifecycle::Initializing,
            initialization_status: InitializationStatus::AwaitingReply,
            book: PendingBook::new(),
            pending: BTreeMap::new(),
            active_threads: BTreeMap::new(),
            queued_threads: BTreeMap::new(),
        };
        for (id, method, thread_id) in recovered {
            state.track_existing_request_until(
                id,
                method,
                thread_id,
                now_ms.saturating_add(timeout_ms),
            )?;
        }
        let ticket = state.register(
            Method::Initialize,
            initialize_params,
            now_ms,
            timeout_ms,
            None,
            false,
            None,
        )?;
        let actions = state.write_request(&ticket)?;
        Ok((state, actions))
    }

    pub fn track_existing_request(
        &mut self,
        id: RequestId,
        method: Method,
        thread_id: Option<String>,
    ) -> Result<RequestTicket, SessionError> {
        self.track_existing_request_until(id, method, thread_id, u64::MAX)
    }

    fn track_existing_request_until(
        &mut self,
        id: RequestId,
        method: Method,
        thread_id: Option<String>,
        deadline_ms: u64,
    ) -> Result<RequestTicket, SessionError> {
        if matches!(self.lifecycle, Lifecycle::Lost | Lifecycle::Closed) {
            return Err(SessionError::NotReady);
        }
        self.book.register(id.clone(), method)?;
        let ticket = RequestTicket {
            generation: self.generation,
            id: id.clone(),
            method,
        };
        self.pending.insert(
            id,
            Pending {
                ticket: ticket.clone(),
                params: Value::Null,
                deadline_ms,
                thread_id,
                serialized: false,
            },
        );
        Ok(ticket)
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn lifecycle(&self) -> Lifecycle {
        self.lifecycle
    }

    #[must_use]
    pub const fn initialization_status(&self) -> InitializationStatus {
        self.initialization_status
    }

    /// Mark the `initialized` notification complete after its writer receipt.
    /// Repeated completion reports are idempotent.
    pub fn mark_initialized_notification_written(&mut self) -> Result<(), SessionError> {
        match self.initialization_status {
            InitializationStatus::InitializedWritePending => {
                self.initialization_status = InitializationStatus::Ready;
                Ok(())
            }
            InitializationStatus::Ready => Ok(()),
            InitializationStatus::AwaitingReply
            | InitializationStatus::Refused
            | InitializationStatus::Unknown => Err(SessionError::InitializationNotReady),
        }
    }

    /// Earliest absolute deadline for an outstanding request. This is the
    /// session's single deadline authority, including recovered requests.
    #[must_use]
    pub fn next_deadline_ms(&self) -> Option<u64> {
        self.pending
            .iter()
            .filter(|(id, _)| self.book.state(id) == Some(RequestState::Pending))
            .map(|(_, pending)| pending.deadline_ms)
            .filter(|deadline| *deadline != u64::MAX)
            .min()
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
        self.submit_with_id_inner(
            None,
            method,
            params,
            now_ms,
            timeout_ms,
            thread_id,
            lightweight_read,
        )
    }

    /// Submit a request with a durable caller allocated identity.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_with_id(
        &mut self,
        id: RequestId,
        method: Method,
        params: Value,
        now_ms: u64,
        timeout_ms: u64,
        thread_id: Option<String>,
        lightweight_read: bool,
    ) -> Result<(RequestTicket, Vec<Action>), SessionError> {
        self.submit_with_id_inner(
            Some(id),
            method,
            params,
            now_ms,
            timeout_ms,
            thread_id,
            lightweight_read,
        )
    }

    /// Validate a caller allocated request before its durable Submitted marker.
    pub fn preflight_with_id(
        &self,
        id: RequestId,
        method: Method,
        params: &Value,
        thread_id: Option<&str>,
        lightweight_read: bool,
    ) -> Result<(), SessionError> {
        if self.lifecycle != Lifecycle::Ready {
            return Err(SessionError::NotReady);
        }
        let serialized = self.claude && !lightweight_read && is_thread_serialized(method);
        if serialized && thread_id.is_none() {
            return Err(SessionError::MissingThreadId);
        }
        validate_request_encoding(id, method, params)
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_with_id_inner(
        &mut self,
        request_id: Option<RequestId>,
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
        if serialized && thread_id.is_none() {
            return Err(SessionError::MissingThreadId);
        }
        let ticket = self.register(
            method, params, now_ms, timeout_ms, thread_id, serialized, request_id,
        )?;
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

    #[allow(clippy::too_many_arguments)]
    fn register(
        &mut self,
        method: Method,
        params: Value,
        now_ms: u64,
        timeout_ms: u64,
        thread_id: Option<String>,
        serialized: bool,
        request_id: Option<RequestId>,
    ) -> Result<RequestTicket, SessionError> {
        let id = if let Some(id) = request_id {
            validate_request_encoding(id.clone(), method, &params)?;
            self.book.register(id.clone(), method)?;
            id
        } else if let Some(prefix) = &self.request_id_prefix {
            let id = RequestId::String(format!("{prefix}:{}", self.next_request_sequence));
            self.next_request_sequence = self
                .next_request_sequence
                .checked_add(1)
                .ok_or(BookError::IdExhausted)?;
            validate_request_encoding(id.clone(), method, &params)?;
            self.book.register(id.clone(), method)?;
            id
        } else if self.claude {
            let id = RequestId::String(format!("claude:{:032x}", self.next_claude_id));
            let next_id = self
                .next_claude_id
                .checked_add(1)
                .ok_or(BookError::IdExhausted)?;
            validate_request_encoding(id.clone(), method, &params)?;
            self.book.register(id.clone(), method)?;
            self.next_claude_id = next_id;
            id
        } else {
            let id = self.book.next_numeric_candidate()?;
            validate_request_encoding(id.clone(), method, &params)?;
            let allocated = self.book.allocate_numeric(method)?;
            debug_assert_eq!(allocated, id);
            allocated
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
        self.receive_inner(frame, None)
    }

    /// Classify a response using the transport arrival time, even if a timer
    /// already reported uncertainty while the journal consumer was behind.
    pub fn receive_at(
        &mut self,
        frame: Frame,
        received_at_ms: u64,
    ) -> Result<Vec<Action>, SessionError> {
        self.receive_inner(frame, Some(received_at_ms))
    }

    fn receive_inner(
        &mut self,
        frame: Frame,
        received_at_ms: Option<u64>,
    ) -> Result<Vec<Action>, SessionError> {
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
                let late = received_at_ms.map_or_else(
                    || matches!(matched, ResponseMatch::Late { .. }),
                    |received| received >= pending.deadline_ms,
                );
                let initialization_failed = pending.ticket.method == Method::Initialize
                    && self.lifecycle == Lifecycle::Initializing
                    && matches!(&body, ResponseBody::Error(_));
                let mut actions = vec![Action::Resolve {
                    ticket: pending.ticket.clone(),
                    outcome: RequestOutcome::Reply { body, late },
                }];
                if pending.ticket.method == Method::Initialize
                    && self.lifecycle == Lifecycle::Initializing
                {
                    self.initialization_status = if initialization_failed {
                        InitializationStatus::Refused
                    } else {
                        InitializationStatus::InitializedWritePending
                    };
                    if initialization_failed {
                        actions.extend(self.mark_unknown(Lifecycle::Lost));
                    } else {
                        self.lifecycle = Lifecycle::Ready;
                        actions.push(Action::Write(encode_line(&Frame::Notification(
                            crate::NotificationFrame {
                                method: Method::Initialized,
                                params: None,
                                fields: Map::new(),
                            },
                        ))?));
                    }
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
        let expired = self
            .pending
            .iter()
            .filter_map(|(id, pending)| {
                (now_ms >= pending.deadline_ms
                    && self.book.state(id) == Some(RequestState::Pending))
                .then(|| id.clone())
            })
            .collect::<Vec<_>>();
        if expired.is_empty() {
            return (Ok(()), Vec::new());
        }
        let mut actions = Vec::with_capacity(expired.len());
        for id in expired {
            if self.book.mark_unknown(&id).is_err() {
                continue;
            }
            if let Some(pending) = self.pending.get(&id) {
                if pending.ticket.method == Method::Initialize {
                    self.initialization_status = InitializationStatus::Unknown;
                }
                actions.push(Action::Resolve {
                    ticket: pending.ticket.clone(),
                    outcome: RequestOutcome::Unknown,
                });
            }
        }
        (Err(SessionError::DeadlineUnknown), actions)
    }

    pub fn transport_lost(&mut self) -> Vec<Action> {
        self.mark_unknown(Lifecycle::Lost)
    }

    pub fn close(&mut self) -> Vec<Action> {
        self.mark_unknown(Lifecycle::Closed)
    }

    fn mark_unknown(&mut self, lifecycle: Lifecycle) -> Vec<Action> {
        if self.lifecycle == Lifecycle::Closed {
            return Vec::new();
        }
        if self.lifecycle == Lifecycle::Lost {
            if lifecycle == Lifecycle::Closed {
                self.lifecycle = Lifecycle::Closed;
                self.pending.clear();
                self.active_threads.clear();
                self.queued_threads.clear();
            }
            return Vec::new();
        }
        self.book.mark_connection_lost();
        self.lifecycle = lifecycle;
        if self.initialization_status == InitializationStatus::AwaitingReply {
            self.initialization_status = InitializationStatus::Unknown;
        }
        self.active_threads.clear();
        self.queued_threads.clear();
        let mut actions = Vec::with_capacity(self.pending.len() + 1);
        for pending in self.pending.values() {
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
        self.remove_request(ticket)
    }

    /// Use only with positive evidence that this request was never sent. Unlike
    /// user cancellation this also retires deadline-unknown and Codex tickets.
    pub fn reject_unsent(&mut self, ticket: &RequestTicket) -> Result<Vec<Action>, SessionError> {
        if ticket.generation != self.generation {
            return Err(SessionError::StaleGeneration);
        }
        self.book.reject_unsent(&ticket.id)?;
        self.remove_request(ticket)
    }

    fn remove_request(&mut self, ticket: &RequestTicket) -> Result<Vec<Action>, SessionError> {
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

fn validate_request_encoding(
    id: RequestId,
    method: Method,
    params: &Value,
) -> Result<(), SessionError> {
    encode_line(&Frame::Request(RequestFrame {
        id,
        method,
        params: Some(params.clone()),
        fields: Map::new(),
    }))?;
    Ok(())
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
