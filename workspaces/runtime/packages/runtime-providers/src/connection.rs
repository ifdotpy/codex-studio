//! Pure, deterministic provider connection state machine.

use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use thiserror::Error;

use crate::{
    BookError, CodecError, Frame, Method, PendingBook, RequestFrame, RequestId, RequestIdScheme,
    ResponseBody, ResponseMatch, WireProfile, encode_line, validate_initialize_params,
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
    Write(Frame),
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
    #[error("recovery contains conflicting active requests for a serialized thread")]
    ConflictingRecoveredThread,
    #[error("initialized notification cannot be marked written in this state")]
    InitializationNotReady,
}

struct Pending {
    ticket: RequestTicket,
    /// Owns the body only until its first write; emitted requests keep `Null`.
    params: Value,
    deadline_ms: u64,
    thread_id: Option<String>,
    serialized: bool,
}

/// Session decisions are independent of threads and byte I/O. Call `submit`,
/// `receive`, `expire`, `cancel`, or `transport_lost`, then execute returned actions.
pub struct SessionState {
    generation: u64,
    wire: WireProfile,
    next_prefixed_id: u64,
    request_id_prefix: Option<String>,
    next_request_sequence: u64,
    lifecycle: Lifecycle,
    initialization_status: InitializationStatus,
    book: PendingBook,
    pending: BTreeMap<RequestId, Pending>,
    /// `(deadline_ms, id)` of every pending request whose book state is
    /// `Pending` and whose deadline is finite, earliest first. It lets
    /// `expire` and `next_deadline_ms` touch only due requests.
    deadlines: BTreeSet<(u64, RequestId)>,
    active_threads: BTreeMap<String, RequestId>,
    queued_threads: BTreeMap<String, VecDeque<RequestId>>,
}

impl SessionState {
    pub fn new(
        generation: u64,
        wire: WireProfile,
        now_ms: u64,
        timeout_ms: u64,
        initialize_params: Value,
    ) -> Result<(Self, Vec<Action>), SessionError> {
        Self::new_with_recovered(
            generation,
            wire,
            now_ms,
            timeout_ms,
            initialize_params,
            Vec::new(),
        )
    }

    pub fn new_with_recovered(
        generation: u64,
        wire: WireProfile,
        now_ms: u64,
        timeout_ms: u64,
        initialize_params: Value,
        recovered: Vec<(RequestId, Method, Option<String>)>,
    ) -> Result<(Self, Vec<Action>), SessionError> {
        Self::new_with_recovered_and_prefix(
            generation,
            wire,
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
        wire: WireProfile,
        now_ms: u64,
        timeout_ms: u64,
        initialize_params: Value,
        recovered: Vec<(RequestId, Method, Option<String>)>,
        request_id_prefix: Option<String>,
    ) -> Result<(Self, Vec<Action>), SessionError> {
        validate_initialize_params(&initialize_params)?;
        let mut state = Self {
            generation,
            wire,
            next_prefixed_id: 1,
            request_id_prefix,
            next_request_sequence: 1,
            lifecycle: Lifecycle::Initializing,
            initialization_status: InitializationStatus::AwaitingReply,
            book: PendingBook::new(),
            pending: BTreeMap::new(),
            deadlines: BTreeSet::new(),
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

    /// Restore the original deadline and uncertainty, without restarting a timer.
    pub fn restore_request(
        &mut self,
        ticket: &RequestTicket,
        thread_id: Option<String>,
        deadline_ms: u64,
        unknown: bool,
        lightweight: bool,
    ) -> Result<(), SessionError> {
        if ticket.generation != self.generation {
            return Err(SessionError::StaleGeneration);
        }
        let serialized =
            self.wire.per_thread_serialized && !lightweight && is_thread_serialized(ticket.method);
        if serialized {
            let thread = thread_id.as_ref().ok_or(SessionError::MissingThreadId)?;
            if self.active_threads.contains_key(thread) {
                return Err(SessionError::ConflictingRecoveredThread);
            }
        }
        self.track_existing_request_until(
            ticket.id.clone(),
            ticket.method,
            thread_id.clone(),
            deadline_ms,
        )?;
        if serialized && let Some(thread) = thread_id {
            if let Some(pending) = self.pending.get_mut(&ticket.id) {
                pending.serialized = true;
            }
            self.active_threads.insert(thread, ticket.id.clone());
        }
        if unknown {
            self.book.mark_unknown(&ticket.id)?;
            self.deadlines.remove(&(deadline_ms, ticket.id.clone()));
        }
        Ok(())
    }

    pub fn request_deadline_ms(&self, id: &RequestId) -> Option<u64> {
        self.pending.get(id).map(|pending| pending.deadline_ms)
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
        self.insert_pending(Pending {
            ticket: ticket.clone(),
            params: Value::Null,
            deadline_ms,
            thread_id,
            serialized: false,
        });
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
        self.deadlines.first().map(|(deadline, _)| *deadline)
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
        let serialized =
            self.wire.per_thread_serialized && !lightweight_read && is_thread_serialized(method);
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
        let serialized =
            self.wire.per_thread_serialized && !lightweight_read && is_thread_serialized(method);
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
        } else if let RequestIdScheme::PrefixedHex(prefix) = self.wire.ids {
            let id = RequestId::String(format!("{prefix}:{:032x}", self.next_prefixed_id));
            let next_id = self
                .next_prefixed_id
                .checked_add(1)
                .ok_or(BookError::IdExhausted)?;
            validate_request_encoding(id.clone(), method, &params)?;
            self.book.register(id.clone(), method)?;
            self.next_prefixed_id = next_id;
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
        self.insert_pending(Pending {
            ticket: ticket.clone(),
            params,
            deadline_ms: now_ms.saturating_add(timeout_ms),
            thread_id,
            serialized,
        });
        Ok(ticket)
    }

    fn insert_pending(&mut self, pending: Pending) {
        let key = (pending.deadline_ms, pending.ticket.id.clone());
        if let Some(replaced) = self.pending.insert(pending.ticket.id.clone(), pending) {
            self.deadlines
                .remove(&(replaced.deadline_ms, replaced.ticket.id));
        }
        if key.0 != u64::MAX {
            self.deadlines.insert(key);
        }
    }

    fn remove_pending(&mut self, id: &RequestId) -> Option<Pending> {
        let pending = self.pending.remove(id)?;
        self.deadlines
            .remove(&(pending.deadline_ms, pending.ticket.id.clone()));
        Some(pending)
    }

    fn write_request(&mut self, ticket: &RequestTicket) -> Result<Vec<Action>, SessionError> {
        let pending = self
            .pending
            .get_mut(&ticket.id)
            .ok_or(SessionError::MissingRequest)?;
        let frame = Frame::Request(RequestFrame {
            id: ticket.id.clone(),
            method: ticket.method,
            params: Some(std::mem::take(&mut pending.params)),
            fields: Map::new(),
        });
        Ok(vec![Action::Write(frame)])
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
                let Some(pending) = self.remove_pending(&id) else {
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
                        actions.push(Action::Write(Frame::Notification(
                            crate::NotificationFrame {
                                method: Method::Initialized,
                                params: None,
                                fields: Map::new(),
                            },
                        )));
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
        let mut expired = false;
        let mut actions = Vec::new();
        while let Some((_, id)) = self
            .deadlines
            .first()
            .is_some_and(|(deadline, _)| *deadline <= now_ms)
            .then(|| self.deadlines.pop_first())
            .flatten()
        {
            expired = true;
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
        if expired {
            (Err(SessionError::DeadlineUnknown), actions)
        } else {
            (Ok(()), actions)
        }
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
        self.deadlines.clear();
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
        if !self.wire.cancellable {
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
        let Some(pending) = self.remove_pending(&ticket.id) else {
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

#[cfg(test)]
mod request_body_tests {
    use super::*;
    use crate::{ClientName, ProviderKind, initialize_params};
    use serde_json::json;

    fn written_request(actions: &[Action]) -> &RequestFrame {
        let mut requests = actions.iter().filter_map(|action| match action {
            Action::Write(Frame::Request(request)) => Some(request),
            _ => None,
        });
        let request = requests.next().expect("one request write");
        assert!(requests.next().is_none(), "request is emitted only once");
        request
    }

    fn reply(ticket: &RequestTicket) -> Frame {
        Frame::Response {
            id: ticket.id.clone(),
            body: ResponseBody::Result(json!({"ok": true})),
            fields: Map::new(),
        }
    }

    fn ready(provider: ProviderKind) -> SessionState {
        let (mut session, actions) = SessionState::new(
            9,
            provider.wire(),
            0,
            100,
            initialize_params(ClientName::AccountSession),
        )
        .expect("new session");
        let request = written_request(&actions);
        let ticket = session.pending[&request.id].ticket.clone();
        let initialized = session.receive(reply(&ticket)).expect("initialize reply");
        assert!(matches!(
            initialized.as_slice(),
            [Action::Resolve { ticket: resolved, .. }, Action::Write(Frame::Notification(notification))]
                if resolved == &ticket && notification.method == Method::Initialized
        ));
        session
            .mark_initialized_notification_written()
            .expect("initialized write receipt");
        session
    }

    fn turn_params() -> Value {
        json!({"threadId": "t", "input": [{"type": "text", "text": "owned request body".repeat(64)}]})
    }

    fn text_allocation(params: &Value) -> *const u8 {
        params["input"][0]["text"]
            .as_str()
            .expect("request text")
            .as_ptr()
    }

    fn assert_transferred(
        session: &SessionState,
        ticket: &RequestTicket,
        actions: &[Action],
        expected: &Value,
        allocation: *const u8,
    ) {
        let request = written_request(actions);
        assert_eq!((&request.id, request.method), (&ticket.id, ticket.method));
        let params = request.params.as_ref().expect("request params");
        assert_eq!(params, expected);
        assert_eq!(
            text_allocation(params),
            allocation,
            "the write owns the original body allocation"
        );
        assert!(
            session.pending[&ticket.id].params.is_null(),
            "correlation metadata must not retain an emitted body"
        );
    }

    #[test]
    fn initialize_transfers_body_allocation() {
        for provider in [ProviderKind::Codex, ProviderKind::Claude] {
            let params = initialize_params(ClientName::AccountSession);
            let allocation = params["clientInfo"]["name"]
                .as_str()
                .expect("client name")
                .as_ptr();
            let expected = params.clone();
            let (session, actions) =
                SessionState::new(9, provider.wire(), 0, 100, params).expect("new session");
            let request = written_request(&actions);
            assert_eq!(request.method, Method::Initialize);
            assert_eq!(request.params.as_ref(), Some(&expected));
            assert_eq!(
                request.params.as_ref().expect("initialize params")["clientInfo"]["name"]
                    .as_str()
                    .expect("client name")
                    .as_ptr(),
                allocation,
                "initialize transfers the original allocation to its write"
            );
            assert!(session.pending[&request.id].params.is_null());
            assert_eq!(session.next_deadline_ms(), Some(100));
        }
    }

    #[test]
    fn immediate_requests_transfer_body_allocations() {
        for provider in [ProviderKind::Codex, ProviderKind::Claude] {
            let mut session = ready(provider);
            let params = turn_params();
            let allocation = text_allocation(&params);
            let expected = params.clone();
            let (ticket, actions) = session
                .submit(Method::TurnStart, params, 1, 20, Some("t".into()), false)
                .expect("submit turn");
            assert_eq!(ticket.generation, 9);
            assert_transferred(&session, &ticket, &actions, &expected, allocation);
            assert_eq!(session.request_deadline_ms(&ticket.id), Some(21));
        }
    }

    #[test]
    fn queued_successor_keeps_body_until_first_emission() {
        let mut session = ready(ProviderKind::Claude);
        let (active, _) = session
            .submit(
                Method::TurnStart,
                turn_params(),
                1,
                20,
                Some("t".into()),
                false,
            )
            .expect("active turn");
        let params = turn_params();
        let allocation = text_allocation(&params);
        let expected = params.clone();
        let (queued, actions) = session
            .submit(Method::TurnSteer, params, 2, 20, Some("t".into()), false)
            .expect("queued turn");
        assert!(actions.is_empty());
        assert!(session.pending[&active.id].params.is_null());
        assert_eq!(session.pending[&queued.id].params, expected);
        assert_eq!(
            text_allocation(&session.pending[&queued.id].params),
            allocation
        );
        let released = session.receive(reply(&active)).expect("release successor");
        assert!(matches!(
            released.as_slice(),
            [Action::Resolve { ticket: resolved, .. }, Action::ThreadReleased { thread_id, ticket }, Action::Write(_)]
                if resolved == &active && thread_id == "t" && ticket == &queued
        ));
        assert_transferred(&session, &queued, &released, &expected, allocation);
        assert!(!session.pending.contains_key(&active.id));
        assert!(
            session
                .receive(reply(&active))
                .expect("duplicate reply")
                .is_empty()
        );
        assert_eq!(session.active_threads.get("t"), Some(&queued.id));
    }

    #[test]
    fn unknown_sent_requests_keep_only_metadata_and_accept_late_replies() {
        for provider in [ProviderKind::Codex, ProviderKind::Claude] {
            for transport_lost in [false, true] {
                let mut session = ready(provider);
                let params = turn_params();
                let allocation = text_allocation(&params);
                let expected = params.clone();
                let (ticket, writes) = session
                    .submit(Method::TurnStart, params, 1, 20, Some("t".into()), false)
                    .expect("submit turn");
                assert_transferred(&session, &ticket, &writes, &expected, allocation);
                drop(writes);
                let unknown = if transport_lost {
                    session.transport_lost()
                } else {
                    let (result, actions) = session.expire(21);
                    assert_eq!(result, Err(SessionError::DeadlineUnknown));
                    assert!(session.expire(22).1.is_empty());
                    actions
                };
                assert!(matches!(
                    unknown.first(),
                    Some(Action::Resolve { ticket: resolved, outcome: RequestOutcome::Unknown })
                        if resolved == &ticket
                ));
                assert!(
                    !unknown
                        .iter()
                        .any(|action| matches!(action, Action::Write(_)))
                );
                assert_eq!(session.pending[&ticket.id].ticket, ticket);
                assert!(session.pending[&ticket.id].params.is_null());
                assert_eq!(session.next_deadline_ms(), None);
                let late = session.receive(reply(&ticket)).expect("late reply");
                assert!(matches!(
                    late.as_slice(),
                    [Action::Resolve { ticket: resolved, outcome: RequestOutcome::Reply { late: true, .. } }]
                        if resolved == &ticket
                ));
                assert!(!session.pending.contains_key(&ticket.id));
                assert!(
                    session
                        .receive(reply(&ticket))
                        .expect("duplicate late reply")
                        .is_empty()
                );
            }
        }
    }

    #[test]
    fn cancelled_or_rejected_unsent_successor_is_never_emitted() {
        for cancel in [true, false] {
            let mut session = ready(ProviderKind::Claude);
            let (active, _) = session
                .submit(
                    Method::TurnStart,
                    turn_params(),
                    1,
                    20,
                    Some("t".into()),
                    false,
                )
                .expect("active turn");
            let params = turn_params();
            let allocation = text_allocation(&params);
            let (queued, writes) = session
                .submit(Method::TurnSteer, params, 2, 20, Some("t".into()), false)
                .expect("queued turn");
            assert!(writes.is_empty());
            assert_eq!(
                text_allocation(&session.pending[&queued.id].params),
                allocation
            );
            let removed = if cancel {
                session.cancel(&queued)
            } else {
                session.reject_unsent(&queued)
            }
            .expect("remove unsent request");
            assert!(removed.is_empty());
            assert!(!session.pending.contains_key(&queued.id));
            let settled = session.receive(reply(&active)).expect("active reply");
            assert!(
                matches!(settled.as_slice(), [Action::Resolve { ticket, .. }] if ticket == &active)
            );
            assert!(
                session
                    .receive(reply(&queued))
                    .expect("orphan reply")
                    .is_empty()
            );
            assert!(session.pending.is_empty());
            assert!(session.active_threads.is_empty());
            assert_eq!(session.next_deadline_ms(), None);
        }
    }
}
