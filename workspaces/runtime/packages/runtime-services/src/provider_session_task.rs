//! A bounded Tokio session. Protocol decisions belong to the single SessionState;
//! journal settlement belongs to the durable consumer, including initialization.
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc};

use futures_util::FutureExt;
use serde_json::{Map, Value};
use studio_runtime_process::{
    EventCursor, Generation, OperationId, ProviderHandle,
    provider_seam::{
        AdapterCommand, AdapterError, AttachmentStatus, CommandReceipt, InitializationStatus,
        JournalEntry, JournalPayload, LANE_CAPACITY, MAX_PENDING_RESPONSES, OpenOptions,
        PreparedWrite, ProviderAttachment, ProviderTaskFactory, SessionClock, Shutdown, TaskExit,
        initialized_operation,
    },
};
use studio_runtime_providers::{
    Action, Frame, Method, RequestFrame, RequestId, RequestOutcome, RequestTicket, ResponseBody,
    SessionError, SessionState,
};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

pub const MAX_UNSETTLED: usize = 64;
const MAX_TIMEOUT_MS: u64 = u32::MAX as u64;

pub struct SessionTaskConfig {
    pub initialize_params: Value,
    pub generation: Generation,
    pub claude: bool,
    pub timeout_ms: u64,
    pub initialize_operation: OperationId,
    pub request_id_prefix: Option<String>,
    pub recovered: Vec<RecoveredRequest>,
}

/// Deadline uses the supplied SessionClock epoch; preserve it across reattach.
#[derive(Clone, Debug, PartialEq)]
pub struct RecoveredRequest {
    pub ticket: RequestTicket,
    pub operation_id: OperationId,
    pub thread_id: Option<String>,
    pub deadline_ms: u64,
    pub unknown: bool,
    pub remote_id: Option<u64>,
}

#[derive(Clone)]
pub struct RequestSpec {
    pub id: RequestId,
    pub operation_id: OperationId,
    pub method: Method,
    pub params: Value,
    pub thread_id: Option<String>,
    pub lightweight: bool,
    pub timeout_ms: u64,
}

/// Validated wire size/shape before the account commits its Submitted marker.
/// Admission can still be refused if readiness or capacity changes afterward.
pub struct PreparedRequest {
    spec: RequestSpec,
    write: Option<PreparedWrite>,
    generation: Generation,
    owner: mpsc::Sender<Command>,
}
pub struct PreparedFrame {
    write: PreparedWrite,
    operation: OperationId,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    Written,
    NotSent,
    Unknown,
}
pub enum SessionOutcome {
    Write {
        operation_id: OperationId,
        ticket: Option<RequestTicket>,
        outcome: WriteOutcome,
        recovery: Option<RecoveredRequest>,
    },
    Resolved {
        ticket: RequestTicket,
        outcome: RequestOutcome,
    },
    GenerationClosed {
        generation: u64,
    },
}

/// One state transition, bounded by MAX_PENDING_RESPONSES outcomes and one entry.
/// Positive replies remain associated with their durable journal envelope.
pub struct SessionEvent {
    pub journal: Option<JournalEntry>,
    pub outcomes: Vec<SessionOutcome>,
}

#[derive(Clone)]
pub struct SessionStatus {
    pub generation: Generation,
    pub initialization: InitializationStatus,
    pub attachment: AttachmentStatus,
}

#[derive(Debug, thiserror::Error)]
pub enum SessionTaskError {
    #[error("session closed")]
    Closed,
    #[error("session admission capacity exhausted")]
    Capacity,
    #[error("session admission receipt was lost; submission may have occurred")]
    AdmissionUnknown,
    #[error("invalid session configuration or prepared command")]
    Invalid,
    #[error("cursor was not delivered by this session")]
    UndeliveredCursor,
    #[error("session task failed")]
    TaskFailed,
    #[error(transparent)]
    Protocol(#[from] SessionError),
    #[error(transparent)]
    Adapter(#[from] AdapterError),
}

/// Completion is outside the bounded event lane. The account must persist
/// adapter.recovery_needed and settle these final outcomes itself.
pub struct SessionExit {
    pub undelivered: Vec<SessionEvent>,
    pub pending_acknowledgement: Option<EventCursor>,
    pub result: Result<(), SessionTaskError>,
    pub adapter: Option<TaskExit>,
    pub outcomes: Vec<SessionOutcome>,
}

pub struct SessionHandle {
    pub events: mpsc::Receiver<SessionEvent>,
    pub status: watch::Receiver<SessionStatus>,
    pub control: SessionControl,
    stop: Shutdown,
    task: Option<JoinHandle<SessionExit>>,
}

/// Clone this control half so an account task can select admission/settlement
/// futures alongside the separately owned bounded event receiver.
#[derive(Clone)]
pub struct SessionControl {
    commands: mpsc::Sender<Command>,
    settlements: mpsc::Sender<Settlement>,
    status: watch::Receiver<SessionStatus>,
    stop: Shutdown,
}

impl SessionHandle {
    /// No startup receive loop: the returned handle can drain replay immediately.
    pub async fn open(
        factory: Arc<dyn ProviderTaskFactory>,
        handle: ProviderHandle,
        mut options: OpenOptions,
        config: SessionTaskConfig,
    ) -> Result<Self, SessionTaskError> {
        if config.generation.0 == 0
            || options
                .expected_generation
                .is_some_and(|generation| generation != config.generation)
            || config
                .request_id_prefix
                .as_ref()
                .is_some_and(|prefix| prefix.len() > 1024)
            || config.recovered.iter().any(|r| {
                matches!(r.ticket.method, Method::Initialize | Method::Initialized)
                    || !valid_id(&r.ticket.id)
                    || r.ticket.generation != config.generation.0
                    || r.operation_id.0.is_empty()
                    || r.operation_id.0.len() > 1024
            })
            || config.timeout_ms > MAX_TIMEOUT_MS
            || config.recovered.len() >= MAX_PENDING_RESPONSES
            || config.recovered.iter().any(|r| {
                r.thread_id
                    .as_ref()
                    .is_some_and(|thread| thread.len() > 1024)
            })
        {
            return Err(SessionTaskError::Invalid);
        }
        let clock = options.clock.clone();
        let (mut state, initial) = SessionState::new_with_recovered_and_prefix(
            config.generation.0,
            config.claude,
            clock.now_ms(),
            config.timeout_ms,
            config.initialize_params,
            vec![],
            config.request_id_prefix,
        )?;
        let frame = decode_write(
            initial
                .into_iter()
                .next()
                .ok_or(SessionTaskError::Invalid)?,
        )?;
        // Validate the stricter supervisor envelope limit before factory.open.
        let initialize = PreparedWrite::new(config.initialize_operation.clone(), &frame)?;
        let Frame::Request(initialize_frame) = frame else {
            return Err(SessionTaskError::Invalid);
        };
        let ticket = RequestTicket {
            generation: config.generation.0,
            id: initialize_frame.id.clone(),
            method: Method::Initialize,
        };
        let mut records = BTreeMap::new();
        for recovered in config.recovered {
            state.restore_request(
                &recovered.ticket,
                recovered.thread_id.clone(),
                recovered.deadline_ms,
                recovered.unknown,
            )?;
            if let Some(remote) = recovered.remote_id {
                let RequestId::Number(local) = recovered.ticket.id else {
                    return Err(SessionTaskError::Invalid);
                };
                if options
                    .launch
                    .retained_response_ids
                    .insert(remote, local)
                    .is_some()
                {
                    return Err(SessionTaskError::Invalid);
                }
            }
            records.insert(
                recovered.ticket.id.clone(),
                Record {
                    unknown_reported: recovered.unknown,
                    ticket: recovered.ticket,
                    operation: Some(recovered.operation_id),
                    thread_id: recovered.thread_id,
                    send: SendState::Unknown,
                    write: None,
                    ready: false,
                },
            );
        }
        records.insert(
            ticket.id.clone(),
            Record {
                unknown_reported: false,
                ticket: ticket.clone(),
                operation: Some(config.initialize_operation.clone()),
                send: SendState::Queued,
                thread_id: None,
                write: Some(initialize),
                ready: true,
            },
        );
        let (commands, inbox) = mpsc::channel(LANE_CAPACITY);
        let (settlements, settled) = mpsc::channel(LANE_CAPACITY);
        let (events, receiver) = mpsc::channel(LANE_CAPACITY);
        let (status, observed) = watch::channel(SessionStatus {
            generation: config.generation,
            initialization: InitializationStatus::AwaitingReply,
            attachment: AttachmentStatus {
                handle: handle.clone(),
                open_attempted: false,
                info: None,
                initialization: InitializationStatus::AwaitingReply,
            },
        });
        let stop = Shutdown::default();
        let actor = SessionTask {
            state,
            clock,
            handle,
            records,
            commands: inbox,
            settled,
            events,
            status,
            stop: stop.clone(),
            seen: BTreeMap::new(),
            attachment_known: false,
            acknowledged: EventCursor(0),
            ack_target: None,
            retained: Retained::default(),
        };
        let task = tokio::spawn(actor.run(factory, options));
        Ok(Self {
            events: receiver,
            control: SessionControl {
                commands,
                settlements,
                status: observed.clone(),
                stop: stop.clone(),
            },
            status: observed,
            stop,
            task: Some(task),
        })
    }

    pub async fn shutdown(&mut self) -> SessionExit {
        self.stop.cancel();
        self.completion().await
    }
    pub async fn completion(&mut self) -> SessionExit {
        let result = match self.task.as_mut() {
            Some(task) => task.await.unwrap_or_else(|_| SessionExit {
                undelivered: vec![],
                pending_acknowledgement: None,
                result: Err(SessionTaskError::TaskFailed),
                adapter: None,
                outcomes: vec![],
            }),
            None => SessionExit {
                undelivered: vec![],
                pending_acknowledgement: None,
                result: Err(SessionTaskError::Closed),
                adapter: None,
                outcomes: vec![],
            },
        };
        self.task.take();
        result
    }
}
impl SessionControl {
    pub fn cancel(&self) {
        self.stop.cancel();
    }
    pub async fn prepare(&self, spec: RequestSpec) -> Result<PreparedRequest, SessionTaskError> {
        if *self.stop.subscribe().borrow() {
            return Err(SessionTaskError::Closed);
        }
        if self.status.borrow().initialization != InitializationStatus::Ready {
            return Err(SessionError::NotReady.into());
        }
        if !valid_id(&spec.id)
            || spec.timeout_ms > MAX_TIMEOUT_MS
            || spec
                .thread_id
                .as_ref()
                .is_some_and(|thread| thread.len() > 1024)
            || matches!(spec.method, Method::Initialize | Method::Initialized)
        {
            return Err(SessionTaskError::Invalid);
        }
        let frame = Frame::Request(RequestFrame {
            id: spec.id.clone(),
            method: spec.method,
            params: Some(spec.params.clone()),
            fields: Map::new(),
        });
        let write = PreparedWrite::new(spec.operation_id.clone(), &frame)?;
        let prepared = PreparedRequest {
            spec,
            write: Some(write),
            generation: self.status.borrow().generation,
            owner: self.commands.clone(),
        };
        let (reply, receipt) = oneshot::channel();
        self.commands
            .send(Command::Prepare { prepared, reply })
            .await
            .map_err(|_| SessionTaskError::Closed)?;
        receipt.await.map_err(|_| SessionTaskError::Closed)?
    }

    pub async fn submit(
        &self,
        prepared: PreparedRequest,
    ) -> Result<RequestTicket, SessionTaskError> {
        if !prepared.owner.same_channel(&self.commands) {
            return Err(SessionTaskError::Invalid);
        }
        let (reply, receipt) = oneshot::channel();
        let mut guard = AdmissionGuard(Some(self.stop.clone()));
        self.commands
            .send(Command::Request { prepared, reply })
            .await
            .map_err(|_| SessionTaskError::Closed)?;
        let result = receipt
            .await
            .map_err(|_| SessionTaskError::AdmissionUnknown)?;
        guard.0 = None;
        result
    }

    /// Callback responses do not register outgoing request correlation.
    pub fn prepare_frame(
        &self,
        operation: OperationId,
        frame: &Frame,
    ) -> Result<PreparedFrame, SessionTaskError> {
        if !matches!(frame, Frame::Response { .. })
            && !matches!(frame, Frame::Notification(notification) if !matches!(notification.method, Method::Initialize | Method::Initialized))
        {
            return Err(SessionTaskError::Invalid);
        }
        Ok(PreparedFrame {
            write: PreparedWrite::new(operation.clone(), frame)?,
            operation,
        })
    }

    pub async fn submit_frame(&self, prepared: PreparedFrame) -> Result<(), SessionTaskError> {
        let (reply, receipt) = oneshot::channel();
        let mut guard = AdmissionGuard(Some(self.stop.clone()));
        self.commands
            .send(Command::Frame { prepared, reply })
            .await
            .map_err(|_| SessionTaskError::Closed)?;
        let result = receipt
            .await
            .map_err(|_| SessionTaskError::AdmissionUnknown)?;
        guard.0 = None;
        result
    }

    /// Call only after durable settlement. Success means recorded for contiguous
    /// forwarding; it is not a receipt that the supervisor has already ACKed.
    pub async fn settled(&self, cursor: EventCursor) -> Result<(), SessionTaskError> {
        let (reply, receipt) = oneshot::channel();
        self.settlements
            .send(Settlement { cursor, reply })
            .await
            .map_err(|_| SessionTaskError::Closed)?;
        receipt.await.map_err(|_| SessionTaskError::Closed)?
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

struct AdmissionGuard(Option<Shutdown>);
impl Drop for AdmissionGuard {
    fn drop(&mut self) {
        if let Some(stop) = &self.0 {
            stop.cancel();
        }
    }
}

struct SettlementState {
    delivered: bool,
    settled: bool,
}
struct Settlement {
    cursor: EventCursor,
    reply: oneshot::Sender<Result<(), SessionTaskError>>,
}
enum Command {
    Prepare {
        prepared: PreparedRequest,
        reply: oneshot::Sender<Result<PreparedRequest, SessionTaskError>>,
    },
    Request {
        prepared: PreparedRequest,
        reply: oneshot::Sender<Result<RequestTicket, SessionTaskError>>,
    },
    Frame {
        prepared: PreparedFrame,
        reply: oneshot::Sender<Result<(), SessionTaskError>>,
    },
}
#[derive(Clone, Copy, PartialEq)]
enum SendState {
    Queued,
    Attempting,
    Written,
    Unknown,
}
struct Record {
    unknown_reported: bool,
    ticket: RequestTicket,
    operation: Option<OperationId>,
    send: SendState,
    thread_id: Option<String>,
    write: Option<PreparedWrite>,
    ready: bool,
}
struct PendingWrite {
    write: PreparedWrite,
    operation: OperationId,
    ticket: Option<RequestTicket>,
    initialized: bool,
}
#[derive(Clone)]
enum Flight {
    Write {
        recovery: Option<RecoveredRequest>,
        operation: OperationId,
        ticket: Option<RequestTicket>,
        initialized: bool,
    },
    Ack(EventCursor),
}
type InFlight = Pin<Box<dyn Future<Output = (Flight, CommandReceipt)> + Send>>;
struct SessionTask {
    state: SessionState,
    clock: SessionClock,
    handle: ProviderHandle,
    records: BTreeMap<RequestId, Record>,
    commands: mpsc::Receiver<Command>,
    settled: mpsc::Receiver<Settlement>,
    events: mpsc::Sender<SessionEvent>,
    status: watch::Sender<SessionStatus>,
    stop: Shutdown,
    seen: BTreeMap<u64, SettlementState>,
    attachment_known: bool,
    acknowledged: EventCursor,
    ack_target: Option<EventCursor>,
    retained: Retained,
}

#[derive(Default)]
struct Retained {
    pending: Option<SessionEvent>,
    control: Option<SessionEvent>,
    actions: Vec<Action>,
    receipt: Option<(Flight, CommandReceipt)>,
    receipt_reported: bool,
    command: Option<Command>,
    initialized: Option<PendingWrite>,
}

impl SessionTask {
    async fn run(
        mut self,
        factory: Arc<dyn ProviderTaskFactory>,
        options: OpenOptions,
    ) -> SessionExit {
        let mut stop = self.stop.subscribe();
        let opened = tokio::select! {
            biased;
            _ = cancelled(&mut stop) => return self.finish_without_adapter(Ok(())),
            result = factory.open(self.handle.clone(), options, self.stop.clone()) => result,
        };
        let mut adapter = match opened {
            Ok(adapter) => adapter,
            Err(error) => return self.finish_without_adapter(Err(error.into())),
        };
        // Only callbacks use this lane. Each admitted request already owns its
        // reserved prepared write in records, including serialized successors.
        let (writes, mut queued) = mpsc::channel(MAX_PENDING_RESPONSES + 2);
        let mut flight: Option<InFlight> = None;
        let result = std::panic::AssertUnwindSafe(self.serve(
            &mut adapter,
            &writes,
            &mut queued,
            &mut flight,
        ))
        .catch_unwind()
        .await
        .unwrap_or(Err(SessionTaskError::TaskFailed));
        self.commands.close();
        if let Some(command) = self.retained.command.take() {
            reject_command(command);
        }
        while let Ok(command) = self.commands.try_recv() {
            reject_command(command);
        }
        self.settled.close();
        self.stop.cancel();
        let exit = adapter.shutdown().await;
        if let Some(future) = flight {
            self.retained.receipt = Some(future.await);
            self.retained.receipt_reported = false;
        }
        // Final receipt evidence is retained before any state cleanup. No more
        // protocol transitions or writes are attempted on the terminal path.
        self.retain_receipt_outcome();
        // An unwind can leave the pure transition result unapplied. It is still
        // owned here; preserve positive evidence without invoking protocol code.
        for action in std::mem::take(&mut self.retained.actions) {
            if let Action::Resolve { ticket, outcome } = action {
                if !matches!(outcome, RequestOutcome::Unknown) {
                    self.records.remove(&ticket.id);
                }
                self.emit(
                    self.retained.pending.is_none(),
                    SessionOutcome::Resolved { ticket, outcome },
                );
            }
        }
        let mut undelivered: Vec<_> = self.retained.pending.take().into_iter().collect();
        while let Ok(journal) = adapter.journal.try_recv() {
            undelivered.push(SessionEvent {
                journal: Some(journal),
                outcomes: vec![],
            });
        }
        let mut outcomes = self
            .retained
            .control
            .take()
            .map(|e| e.outcomes)
            .unwrap_or_default();
        outcomes.extend(self.final_outcomes());
        while let Ok(write) = queued.try_recv() {
            outcomes.push(SessionOutcome::Write {
                operation_id: write.operation,
                ticket: None,
                outcome: WriteOutcome::NotSent,
                recovery: None,
            });
        }
        self.publish(&adapter);
        SessionExit {
            undelivered,
            result,
            adapter: Some(exit),
            outcomes,
            pending_acknowledgement: self.pending_ack(),
        }
    }

    fn finish_without_adapter(&mut self, result: Result<(), SessionTaskError>) -> SessionExit {
        SessionExit {
            undelivered: vec![],
            pending_acknowledgement: None,
            result,
            adapter: None,
            outcomes: self.final_outcomes(),
        }
    }

    fn final_outcomes(&self) -> Vec<SessionOutcome> {
        let mut outcomes = vec![];
        for record in self.records.values() {
            if record.send == SendState::Queued {
                if let Some(operation_id) = &record.operation {
                    outcomes.push(SessionOutcome::Write {
                        operation_id: operation_id.clone(),
                        ticket: Some(record.ticket.clone()),
                        outcome: WriteOutcome::NotSent,
                        recovery: None,
                    });
                }
            } else if !record.unknown_reported {
                outcomes.push(SessionOutcome::Resolved {
                    ticket: record.ticket.clone(),
                    outcome: RequestOutcome::Unknown,
                });
            }
        }
        outcomes.push(SessionOutcome::GenerationClosed {
            generation: self.state.generation(),
        });
        outcomes
    }

    fn pending_ack(&self) -> Option<EventCursor> {
        self.seen
            .iter()
            .take_while(|(_, s)| s.settled)
            .last()
            .map(|(cursor, _)| EventCursor(*cursor))
    }

    async fn serve(
        &mut self,
        adapter: &mut ProviderAttachment,
        writes: &mpsc::Sender<PendingWrite>,
        queued: &mut mpsc::Receiver<PendingWrite>,
        flight: &mut Option<InFlight>,
    ) -> Result<(), SessionTaskError> {
        let mut stop = self.stop.subscribe();
        let mut callback_turn = false;
        let mut acknowledgement_turn = true;
        loop {
            self.publish(adapter);
            if !self.attachment_known
                && let Some(info) = &adapter.status.borrow().info
            {
                self.acknowledged = info.acknowledged;
                self.attachment_known = true;
            }
            if *stop.borrow() {
                return Ok(());
            }
            if flight.is_none() {
                let has_write = self.retained.initialized.is_some()
                    || self.records.values().any(|r| r.ready && r.write.is_some())
                    || !queued.is_empty();
                if let Some(cursor) = self
                    .ack_target
                    .filter(|_| acknowledgement_turn || !has_write)
                {
                    self.ack_target = None;
                    acknowledgement_turn = false;
                    let (command, receipt) = AdapterCommand::ack(cursor);
                    *flight = Some(send_once(
                        adapter.commands.clone(),
                        command,
                        receipt,
                        Flight::Ack(cursor),
                    ));
                } else if self.retained.control.is_none() {
                    // An old on-time reply may release a request that is expired
                    // now. Current time governs sending; arrival governs replies.
                    self.retained.actions = self.state.expire(self.clock.now_ms()).1;
                    self.apply_actions(true)?;
                    let write = if self.retained.control.is_some() {
                        None
                    } else if let Some(write) = self.retained.initialized.take() {
                        Some(write)
                    } else if callback_turn && !queued.is_empty() {
                        callback_turn = false;
                        queued.try_recv().ok()
                    } else if let Some(record) = self
                        .records
                        .values_mut()
                        .find(|r| r.ready && r.write.is_some())
                    {
                        callback_turn = true;
                        record.ready = false;
                        record.send = SendState::Attempting;
                        record.write.take().map(|write| PendingWrite {
                            write,
                            operation: record
                                .operation
                                .clone()
                                .unwrap_or(OperationId(String::new())),
                            ticket: Some(record.ticket.clone()),
                            initialized: false,
                        })
                    } else {
                        queued.try_recv().ok()
                    };
                    if let Some(write) = write {
                        acknowledgement_turn = true;
                        let recovery = write.ticket.as_ref().and_then(|ticket| {
                            self.records.get(&ticket.id).map(|record| RecoveredRequest {
                                ticket: ticket.clone(),
                                operation_id: write.operation.clone(),
                                thread_id: record.thread_id.clone(),
                                deadline_ms: self
                                    .state
                                    .request_deadline_ms(&ticket.id)
                                    .unwrap_or(0),
                                unknown: record.unknown_reported,
                                remote_id: None,
                            })
                        });
                        let kind = Flight::Write {
                            recovery,
                            operation: write.operation,
                            ticket: write.ticket,
                            initialized: write.initialized,
                        };
                        let (command, receipt) = AdapterCommand::write(write.write);
                        *flight = Some(send_once(adapter.commands.clone(), command, receipt, kind));
                    }
                }
            }
            let due = self
                .state
                .next_deadline_ms()
                .map(|ms| self.clock.instant_at(ms));
            // Tokio's randomized selection is fair between non-shutdown lanes.
            // Sticky shutdown is checked above, so a ready lane cannot starve it.
            tokio::select! {
                _ = cancelled(&mut stop) => return Ok(()),
                _ = self.events.closed() => return Err(SessionTaskError::Closed),
                changed = adapter.status.changed() => {
                    if changed.is_err() { return Err(SessionTaskError::Closed); }
                    if adapter.status.borrow().info.as_ref().is_some_and(|info| info.generation.0 != self.state.generation()) { return Err(SessionError::StaleGeneration.into()); }
                }
                settlement = self.settled.recv() => {
                    if let Some(settlement) = settlement { let result = self.settle(settlement.cursor); let _ignored = settlement.reply.send(result); }
                    else { return Ok(()); }
                }
                slot = self.events.clone().reserve_owned(), if self.retained.pending.is_some() || self.retained.control.is_some() => {
                    let slot = slot.map_err(|_| SessionTaskError::Closed)?;
                    if let Some(event) = self.retained.pending.take().or_else(|| self.retained.control.take()) {
                        if let Some(entry) = &event.journal && let Some(seen) = self.seen.get_mut(&entry.cursor.0) { seen.delivered = true; }
                        slot.send(event);
                    }
                }
                result = async { match flight.as_mut() { Some(future) => future.await, None => std::future::pending().await } }, if flight.is_some() && self.retained.control.is_none() => {
                    self.retained.receipt = Some(result);
                    self.retained.receipt_reported = false;
                    *flight = None;
                    self.process_receipt()?;
                }
                entry = adapter.journal.recv(), if self.retained.pending.is_none() && self.retained.control.is_none() && self.seen.len() < MAX_UNSETTLED => {
                    let Some(entry) = entry else { return Err(SessionTaskError::Closed); };
                    self.retained.pending = Some(SessionEvent { journal: Some(entry), outcomes: vec![] });
                    #[cfg(test)] if flight.is_some() { tests::panic_at(3); }
                    self.process_journal()?;
                }
                _ = async { match due { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending().await } }, if due.is_some() && self.retained.pending.is_none() && self.retained.control.is_none() => {
                    self.retained.actions = self.state.expire(self.clock.now_ms()).1;
                    self.apply_actions(true)?;
                }
                command = self.commands.recv(), if self.retained.pending.is_none() && self.retained.control.is_none() => {
                    self.retained.command = command;
                    if self.retained.command.is_none() { return Ok(()); }
                    self.command(writes)?;
                }
            }
        }
    }

    fn process_journal(&mut self) -> Result<(), SessionTaskError> {
        #[cfg(test)]
        tests::panic_at(1);
        let entry = self
            .retained
            .pending
            .as_ref()
            .and_then(|event| event.journal.as_ref())
            .ok_or(SessionTaskError::Invalid)?;
        let (cursor, generation, at) = (entry.cursor, entry.generation, entry.received_at_ms);
        if cursor <= self.acknowledged || self.seen.contains_key(&cursor.0) {
            return Err(SessionTaskError::Invalid);
        }
        self.seen.insert(
            cursor.0,
            SettlementState {
                delivered: false,
                settled: false,
            },
        );
        self.retained.actions = self.state.expire(at).1;
        self.apply_actions(false)?;
        if generation.0 == self.state.generation() {
            let frame = self
                .retained
                .pending
                .as_ref()
                .and_then(|event| event.journal.as_ref())
                .and_then(|entry| match &entry.payload {
                    JournalPayload::Frame(frame) => Some(frame.clone()),
                    _ => None,
                });
            if let Some(frame) = frame {
                self.retained.actions = self.state.receive_at(frame, at)?;
                #[cfg(test)]
                tests::panic_at(2);
                self.apply_actions(false)?;
            }
        }
        Ok(())
    }

    fn emit(&mut self, control: bool, outcome: SessionOutcome) {
        let slot = if control {
            &mut self.retained.control
        } else {
            &mut self.retained.pending
        };
        slot.get_or_insert_with(|| SessionEvent {
            journal: None,
            outcomes: vec![],
        })
        .outcomes
        .push(outcome);
    }

    fn publish(&self, adapter: &ProviderAttachment) {
        self.status.send_if_modified(|status| {
            let attachment = adapter.status.borrow().clone();
            let changed = status.initialization != self.state.initialization_status()
                || status.attachment.info.is_none() != attachment.info.is_none()
                || status.attachment.initialization != attachment.initialization
                || status.attachment.open_attempted != attachment.open_attempted
                || status
                    .attachment
                    .info
                    .as_ref()
                    .map(|info| info.acknowledged)
                    != attachment.info.as_ref().map(|info| info.acknowledged);
            status.initialization = self.state.initialization_status();
            status.attachment = attachment;
            changed
        });
    }

    fn command(&mut self, writes: &mpsc::Sender<PendingWrite>) -> Result<(), SessionTaskError> {
        // Keep the taken command until its write belongs to a record/channel and
        // the admission result is ready. No fallible transition owns it locally.
        match self
            .retained
            .command
            .as_ref()
            .ok_or(SessionTaskError::Invalid)?
        {
            Command::Prepare { prepared, .. } => {
                let spec = &prepared.spec;
                let result = if self.records.len() >= MAX_PENDING_RESPONSES {
                    Err(SessionTaskError::Capacity)
                } else {
                    self.state
                        .preflight_with_id(
                            spec.id.clone(),
                            spec.method,
                            &spec.params,
                            spec.thread_id.as_deref(),
                            spec.lightweight,
                        )
                        .map_err(SessionTaskError::from)
                };
                if let Some(Command::Prepare { prepared, reply }) = self.retained.command.take() {
                    let _ignored = reply.send(result.map(|()| prepared));
                }
            }
            Command::Request { .. } => {
                let result = self.submit();
                if let Some(Command::Request { reply, .. }) = self.retained.command.take()
                    && reply.send(result).is_err()
                {
                    self.stop.cancel();
                }
            }
            Command::Frame { .. } => {
                let permit = if self.state.initialization_status() != InitializationStatus::Ready {
                    Err(SessionError::NotReady.into())
                } else {
                    writes.try_reserve().map_err(|_| SessionTaskError::Capacity)
                };
                if let Some(Command::Frame { prepared, reply }) = self.retained.command.take() {
                    let result = permit.map(|permit| {
                        permit.send(PendingWrite {
                            write: prepared.write,
                            operation: prepared.operation,
                            ticket: None,
                            initialized: false,
                        })
                    });
                    if reply.send(result).is_err() {
                        self.stop.cancel();
                    }
                }
            }
        }
        Ok(())
    }

    fn submit(&mut self) -> Result<RequestTicket, SessionTaskError> {
        let Some(Command::Request { prepared, .. }) = self.retained.command.as_ref() else {
            return Err(SessionTaskError::Invalid);
        };
        if self.state.initialization_status() != InitializationStatus::Ready {
            return Err(SessionError::NotReady.into());
        }
        if prepared.generation.0 != self.state.generation() {
            return Err(SessionError::StaleGeneration.into());
        }
        if self.records.len() >= MAX_PENDING_RESPONSES {
            return Err(SessionTaskError::Capacity);
        }
        let spec = prepared.spec.clone();
        if self
            .records
            .values()
            .any(|r| r.operation.as_ref() == Some(&spec.operation_id))
        {
            return Err(SessionTaskError::Invalid);
        }
        let (ticket, actions) = self.state.submit_with_id(
            spec.id,
            spec.method,
            spec.params,
            self.clock.now_ms(),
            spec.timeout_ms,
            spec.thread_id.clone(),
            spec.lightweight,
        )?;
        let write = match &mut self.retained.command {
            Some(Command::Request { prepared, .. }) => prepared.write.take(),
            _ => None,
        };
        self.records.insert(
            ticket.id.clone(),
            Record {
                unknown_reported: false,
                ticket: ticket.clone(),
                operation: Some(spec.operation_id),
                send: SendState::Queued,
                thread_id: spec.thread_id,
                write,
                ready: false,
            },
        );
        self.retained.actions = actions;
        self.apply_actions(true)?;
        Ok(ticket)
    }

    fn apply_actions(&mut self, control: bool) -> Result<(), SessionTaskError> {
        // Keep the entire bounded transition batch outside fallible processing.
        // Positive outcomes are retained before record removal or write release.
        while let Some(action) = self.retained.actions.first().cloned() {
            match action {
                Action::Write(frame) => match frame {
                    Frame::Request(request) => {
                        if let Some(record) = self.records.get_mut(&request.id) {
                            record.ready = true;
                        }
                    }
                    Frame::Notification(notification)
                        if notification.method == Method::Initialized =>
                    {
                        let operation = initialized_operation(
                            &self.handle,
                            Generation(self.state.generation()),
                        );
                        let write = PreparedWrite::new(
                            operation.clone(),
                            &Frame::Notification(notification),
                        )?;
                        self.retained.initialized = Some(PendingWrite {
                            write,
                            operation,
                            ticket: None,
                            initialized: true,
                        });
                    }
                    _ => return Err(SessionTaskError::Invalid),
                },
                Action::Resolve { ticket, outcome } => {
                    let queued = self
                        .records
                        .get(&ticket.id)
                        .is_some_and(|r| r.send == SendState::Queued);
                    if matches!(outcome, RequestOutcome::Unknown) && queued {
                        if let Some(operation_id) = self
                            .records
                            .get(&ticket.id)
                            .and_then(|r| r.operation.clone())
                        {
                            self.emit(
                                control,
                                SessionOutcome::Write {
                                    operation_id,
                                    ticket: Some(ticket.clone()),
                                    outcome: WriteOutcome::NotSent,
                                    recovery: None,
                                },
                            );
                        }
                        self.records.remove(&ticket.id);
                        let released = self.state.reject_unsent(&ticket)?;
                        self.retained.actions.extend(released);
                    } else {
                        let duplicate_unknown = matches!(outcome, RequestOutcome::Unknown)
                            && self
                                .records
                                .get(&ticket.id)
                                .is_some_and(|r| r.unknown_reported);
                        if !duplicate_unknown {
                            self.emit(
                                control,
                                SessionOutcome::Resolved {
                                    ticket: ticket.clone(),
                                    outcome: outcome.clone(),
                                },
                            );
                        }
                        if matches!(outcome, RequestOutcome::Unknown) {
                            if let Some(record) = self.records.get_mut(&ticket.id) {
                                record.unknown_reported = true;
                            }
                        } else {
                            self.records.remove(&ticket.id);
                        }
                    }
                }
                Action::GenerationClosed { generation } => {
                    self.emit(control, SessionOutcome::GenerationClosed { generation })
                }
                Action::Dispatch(_) | Action::ThreadReleased { .. } => {}
            }
            self.retained.actions.remove(0);
        }
        Ok(())
    }

    fn retain_receipt_outcome(&mut self) {
        if self.retained.receipt_reported {
            return;
        }
        self.retained.receipt_reported = true;
        if let Some((Flight::Ack(cursor), CommandReceipt::Acknowledged(ack))) =
            &self.retained.receipt
            && cursor == ack
        {
            self.acknowledged = *cursor;
            self.seen.retain(|seen, _| *seen > cursor.0);
        }
        let Some((
            Flight::Write {
                operation,
                ticket,
                initialized,
                recovery,
            },
            receipt,
        )) = self.retained.receipt.as_ref()
        else {
            return;
        };
        if *initialized
            || ticket
                .as_ref()
                .is_some_and(|t| t.method == Method::Initialize)
        {
            return;
        }
        let outcome = match receipt {
            CommandReceipt::Written { .. } => WriteOutcome::Written,
            CommandReceipt::NotSent => WriteOutcome::NotSent,
            _ => WriteOutcome::Unknown,
        };
        let recovery = if let CommandReceipt::Written { remote_id, .. } = receipt {
            recovery.clone().map(|mut r| {
                r.remote_id = *remote_id;
                r.unknown |= self
                    .records
                    .get(&r.ticket.id)
                    .is_some_and(|r| r.unknown_reported);
                r
            })
        } else {
            None
        };
        let ticket = ticket.clone();
        self.emit(
            true,
            SessionOutcome::Write {
                operation_id: operation.clone(),
                ticket: ticket.clone(),
                outcome,
                recovery,
            },
        );
        if let Some(ticket) = &ticket {
            if outcome == WriteOutcome::NotSent {
                self.records.remove(&ticket.id);
            } else if let Some(record) = self.records.get_mut(&ticket.id) {
                record.send = if outcome == WriteOutcome::Written {
                    SendState::Written
                } else {
                    SendState::Unknown
                };
            }
        }
    }

    fn process_receipt(&mut self) -> Result<(), SessionTaskError> {
        self.retain_receipt_outcome();
        let (flight, receipt) = self
            .retained
            .receipt
            .as_ref()
            .cloned()
            .ok_or(SessionTaskError::Invalid)?;
        match flight {
            Flight::Ack(cursor) => match receipt {
                CommandReceipt::Acknowledged(ack) if ack == cursor => {
                    self.acknowledged = cursor;
                    self.seen.retain(|seen, _| *seen > cursor.0);
                    Ok(())
                }
                _ => Err(SessionTaskError::Closed),
            },
            Flight::Write {
                ticket,
                initialized,
                ..
            } => {
                if let CommandReceipt::CachedInitialize(value) = receipt {
                    let ticket = ticket.ok_or(SessionTaskError::Invalid)?;
                    if ticket.method != Method::Initialize {
                        return Err(SessionTaskError::Invalid);
                    }
                    self.retained.actions = self.state.receive_at(
                        Frame::Response {
                            id: ticket.id,
                            body: ResponseBody::Result(value),
                            fields: Map::new(),
                        },
                        self.clock.now_ms(),
                    )?;
                    self.apply_actions(true)?;
                    if let Some(event) = &mut self.retained.control {
                        event.outcomes.retain(|o| !matches!(o, SessionOutcome::Resolved { ticket, .. } if ticket.method == Method::Initialize));
                    }
                    if self
                        .retained
                        .control
                        .as_ref()
                        .is_some_and(|e| e.outcomes.is_empty())
                    {
                        self.retained.control = None;
                    }
                    return Ok(());
                }
                if matches!(receipt, CommandReceipt::NotSent)
                    && let Some(ticket) = &ticket
                {
                    self.retained.actions = self.state.reject_unsent(ticket)?;
                    self.apply_actions(true)?;
                }
                if initialized && matches!(receipt, CommandReceipt::Written { .. }) {
                    self.state.mark_initialized_notification_written()?;
                }
                if matches!(receipt, CommandReceipt::Unknown)
                    || ((initialized
                        || ticket
                            .as_ref()
                            .is_some_and(|t| t.method == Method::Initialize))
                        && !matches!(receipt, CommandReceipt::Written { .. }))
                {
                    return Err(SessionTaskError::Closed);
                }
                Ok(())
            }
        }
    }

    fn settle(&mut self, cursor: EventCursor) -> Result<(), SessionTaskError> {
        if cursor <= self.acknowledged {
            return Ok(());
        }
        let state = self
            .seen
            .get_mut(&cursor.0)
            .ok_or(SessionTaskError::UndeliveredCursor)?;
        if !state.delivered {
            return Err(SessionTaskError::UndeliveredCursor);
        }
        state.settled = true;
        if let Some(last) = self
            .seen
            .iter()
            .take_while(|(_, state)| state.settled)
            .last()
        {
            self.ack_target = Some(EventCursor(*last.0));
        }
        Ok(())
    }
}

fn reject_command(command: Command) {
    match command {
        Command::Prepare { reply, .. } => {
            let _ignored = reply.send(Err(SessionTaskError::Closed));
        }
        Command::Request { reply, .. } => {
            let _ignored = reply.send(Err(SessionTaskError::Closed));
        }
        Command::Frame { reply, .. } => {
            let _ignored = reply.send(Err(SessionTaskError::Closed));
        }
    }
}

fn valid_id(id: &RequestId) -> bool {
    !matches!(id, RequestId::String(text) if text.len() > 1024)
}

fn decode_write(action: Action) -> Result<Frame, SessionTaskError> {
    match action {
        Action::Write(frame) => Ok(frame),
        _ => Err(SessionTaskError::Invalid),
    }
}
fn send_once(
    sender: mpsc::Sender<AdapterCommand>,
    command: AdapterCommand,
    receipt: studio_runtime_process::provider_seam::Receipt,
    kind: Flight,
) -> InFlight {
    Box::pin(async move {
        let result = if sender.send(command).await.is_err() {
            CommandReceipt::NotSent
        } else {
            receipt.receive().await
        };
        (kind, result)
    })
}
async fn cancelled(stop: &mut watch::Receiver<bool>) {
    loop {
        if *stop.borrow_and_update() || stop.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
#[path = "provider_session_task_tests.rs"]
mod tests;
