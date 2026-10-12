//! Account-scoped provider sessions and asynchronous scheduler turn runtime.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_providers::{Frame, Inbound, Method, RequestId, ResponseBody};
use studio_runtime_store::{
    AgentId, AgentRepository, RuntimeStore, SchedulerRepository, StoreError, TurnAttemptIdentity,
};
use thiserror::Error;

use crate::provider_session::{
    ProviderProcess, ProviderSession, ProviderSessionConfig, ProviderSessionError,
    ProviderSessionEvent,
};
use crate::scheduler::{
    SchedulerError, TurnDriver, TurnIntent, TurnOutcome, TurnStart, consume_wake,
    recover_on_startup, settle_started_attempt, settle_terminal_attempt,
};

const SESSION_POLL: Duration = Duration::from_millis(100);
const SCHEDULER_POLL: Duration = Duration::from_secs(1);
const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 60_000;

/// Stable supervisor identity for one provider app-server process.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct ProviderHandle(String);

impl ProviderHandle {
    #[must_use]
    pub fn for_account(account_key: &str) -> Self {
        Self(format!("account:{account_key}"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn account_key(&self) -> &str {
        self.0.strip_prefix("account:").unwrap_or(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Error)]
#[error("provider process could not be attached")]
pub struct ProviderFactoryError;

/// Supplies supervisor-owned provider streams. Implementations must attach or
/// start only the stable handle they are given.
pub trait ProviderProcessFactory: Send + Sync + 'static {
    fn start_or_reattach(
        &self,
        handle: &ProviderHandle,
    ) -> Result<Box<dyn ProviderProcess>, ProviderFactoryError>;
}

#[derive(Debug, Error)]
pub enum TurnRuntimeError {
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("turn runtime worker panicked during shutdown")]
    WorkerPanicked,
}

#[derive(Clone)]
pub struct TurnWakeHandle(Sender<()>);

impl TurnWakeHandle {
    pub fn wake(&self) {
        let _ignored = self.0.send(());
    }
}

/// Owns the scheduler wake loop and all account-scoped provider reader tasks.
pub struct TurnRuntime {
    shutdown: Arc<AtomicBool>,
    wake: Sender<()>,
    scheduler: Option<JoinHandle<()>>,
    sessions: Arc<SessionRegistry>,
}

impl TurnRuntime {
    pub fn start(
        store: Arc<RuntimeStore>,
        factory: Arc<dyn ProviderProcessFactory>,
    ) -> Result<Self, TurnRuntimeError> {
        Self::start_with_request_timeout(store, factory, DEFAULT_REQUEST_TIMEOUT_MS)
    }

    pub fn start_with_request_timeout(
        store: Arc<RuntimeStore>,
        factory: Arc<dyn ProviderProcessFactory>,
        request_timeout_ms: u64,
    ) -> Result<Self, TurnRuntimeError> {
        recover_on_startup(&store)?;
        let recovery_accounts = store.read().and_then(|connection| {
            SchedulerRepository::accounts_with_unknown_submitted_turns(&connection)
        })?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let sessions = Arc::new(SessionRegistry::new(
            Arc::clone(&store),
            factory,
            Arc::clone(&shutdown),
            request_timeout_ms,
        ));
        for account_key in recovery_accounts {
            // A failed reattach preserves the unknown attempt; it is never replayed.
            let _ignored = sessions.ensure_session(&ProviderHandle::for_account(&account_key));
        }
        let (wake, receiver) = mpsc::channel();
        let scheduler_shutdown = Arc::clone(&shutdown);
        let scheduler_sessions = Arc::clone(&sessions);
        let scheduler = thread::spawn(move || {
            scheduler_loop(store, scheduler_sessions, scheduler_shutdown, receiver)
        });
        Ok(Self {
            shutdown,
            wake,
            scheduler: Some(scheduler),
            sessions,
        })
    }

    #[must_use]
    pub fn wake_handle(&self) -> TurnWakeHandle {
        TurnWakeHandle(self.wake.clone())
    }

    #[must_use]
    pub fn unknown_thread_notifications(&self) -> u64 {
        self.sessions.unknown_threads.load(Ordering::Relaxed)
    }

    pub fn shutdown(&mut self) -> Result<(), TurnRuntimeError> {
        self.shutdown.store(true, Ordering::Release);
        let _ignored = self.wake.send(());
        self.sessions.shutdown()?;
        if let Some(scheduler) = self.scheduler.take() {
            scheduler
                .join()
                .map_err(|_| TurnRuntimeError::WorkerPanicked)?;
        }
        Ok(())
    }
}

impl Drop for TurnRuntime {
    fn drop(&mut self) {
        let _ignored = self.shutdown();
    }
}

struct SessionRegistry {
    store: Arc<RuntimeStore>,
    factory: Arc<dyn ProviderProcessFactory>,
    shutdown: Arc<AtomicBool>,
    state: Mutex<RegistryState>,
    ready: Condvar,
    next_id: AtomicU64,
    unknown_threads: Arc<AtomicU64>,
    request_timeout_ms: u64,
}

struct RegistryState {
    sessions: HashMap<ProviderHandle, ManagedSession>,
    connecting: Vec<ProviderHandle>,
    closed: bool,
}

struct ManagedSession {
    id: u64,
    sender: SyncSender<SessionCommand>,
    worker: Option<JoinHandle<()>>,
}

enum SessionCommand {
    StartTurn(Box<TurnIntent>, Sender<TurnStart>),
    Shutdown,
}

impl SessionRegistry {
    fn new(
        store: Arc<RuntimeStore>,
        factory: Arc<dyn ProviderProcessFactory>,
        shutdown: Arc<AtomicBool>,
        request_timeout_ms: u64,
    ) -> Self {
        Self {
            store,
            factory,
            shutdown,
            state: Mutex::new(RegistryState {
                sessions: HashMap::new(),
                connecting: Vec::new(),
                closed: false,
            }),
            ready: Condvar::new(),
            next_id: AtomicU64::new(1),
            unknown_threads: Arc::new(AtomicU64::new(0)),
            request_timeout_ms,
        }
    }

    fn start_turn(&self, intent: TurnIntent) -> TurnStart {
        if intent.provider != "codex" {
            return TurnStart::Blocked {
                reason: "This Rust turn driver currently supports the Codex provider only.".into(),
            };
        }
        if intent.thread_id.as_deref().is_none_or(str::is_empty)
            || intent.cwd.as_deref().is_none_or(str::is_empty)
            || intent.events.is_empty()
            || intent.events.iter().any(|event| {
                event
                    .metadata
                    .get("assets")
                    .is_some_and(|assets| assets.as_array().is_none_or(|items| !items.is_empty()))
            })
        {
            return TurnStart::Blocked {
                reason: "This Rust turn driver requires an existing text-only provider thread."
                    .into(),
            };
        }
        let handle = ProviderHandle::for_account(&intent.account_key);
        let (sender, id) = match self.session_sender(&handle) {
            Ok(value) => value,
            Err(()) => return TurnStart::NotSubmitted,
        };
        let (reply, result) = mpsc::channel();
        if sender
            .send(SessionCommand::StartTurn(Box::new(intent), reply))
            .is_err()
        {
            self.discard_session(&handle, id);
            return TurnStart::NotSubmitted;
        }
        result.recv().unwrap_or(TurnStart::Unknown)
    }

    fn ensure_session(&self, handle: &ProviderHandle) -> Result<(), ()> {
        self.session_sender(handle).map(|_| ())
    }

    fn session_sender(
        &self,
        handle: &ProviderHandle,
    ) -> Result<(SyncSender<SessionCommand>, u64), ()> {
        loop {
            let mut state = self.state.lock().map_err(|_| ())?;
            if state.closed || self.shutdown.load(Ordering::Acquire) {
                return Err(());
            }
            if let Some(session) = state.sessions.get(handle) {
                return Ok((session.sender.clone(), session.id));
            }
            if state.connecting.iter().any(|existing| existing == handle) {
                state = self.ready.wait(state).map_err(|_| ())?;
                drop(state);
                continue;
            }
            state.connecting.push(handle.clone());
            drop(state);

            let connected = self
                .factory
                .start_or_reattach(handle)
                .map_err(|_| ())
                .and_then(|process| {
                    ProviderSession::connect(process, ProviderSessionConfig::new()).map_err(|_| ())
                });
            let mut state = self.state.lock().map_err(|_| ())?;
            state.connecting.retain(|existing| existing != handle);
            self.ready.notify_all();
            let Ok(session) = connected else {
                return Err(());
            };
            if state.closed || self.shutdown.load(Ordering::Acquire) {
                let _ignored = session.close();
                return Err(());
            }
            let (sender, receiver) = mpsc::sync_channel(128);
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let store = Arc::clone(&self.store);
            let shutdown = Arc::clone(&self.shutdown);
            let unknown_threads = Arc::clone(&self.unknown_threads);
            let request_timeout_ms = self.request_timeout_ms;
            let handle_for_worker = handle.clone();
            let worker = thread::spawn(move || {
                provider_session_loop(
                    session,
                    store,
                    handle_for_worker,
                    receiver,
                    shutdown,
                    unknown_threads,
                    request_timeout_ms,
                );
            });
            state.sessions.insert(
                handle.clone(),
                ManagedSession {
                    id,
                    sender: sender.clone(),
                    worker: Some(worker),
                },
            );
            return Ok((sender, id));
        }
    }

    fn discard_session(&self, handle: &ProviderHandle, id: u64) {
        let worker = self.state.lock().ok().and_then(|mut state| {
            let matches = state
                .sessions
                .get(handle)
                .is_some_and(|session| session.id == id);
            matches
                .then(|| {
                    state
                        .sessions
                        .remove(handle)
                        .and_then(|mut session| session.worker.take())
                })
                .flatten()
        });
        if let Some(worker) = worker {
            let _ignored = worker.join();
        }
    }

    fn shutdown(&self) -> Result<(), TurnRuntimeError> {
        let workers = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| TurnRuntimeError::WorkerPanicked)?;
            state.closed = true;
            let workers = state
                .sessions
                .drain()
                .filter_map(|(_, mut session)| {
                    let _ignored = session.sender.send(SessionCommand::Shutdown);
                    session.worker.take()
                })
                .collect::<Vec<_>>();
            self.ready.notify_all();
            workers
        };
        for worker in workers {
            worker
                .join()
                .map_err(|_| TurnRuntimeError::WorkerPanicked)?;
        }
        Ok(())
    }
}

struct PendingStart {
    intent: TurnIntent,
    reply: Option<Sender<TurnStart>>,
    stage: PendingStartStage,
}

#[derive(Default)]
struct SessionEventState {
    pending: HashMap<RequestId, PendingStart>,
    resumed_threads: HashSet<String>,
    active: HashMap<(String, String), TurnAttemptIdentity>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PendingStartStage {
    Resume,
    Turn,
}

fn provider_session_loop(
    mut session: ProviderSession,
    store: Arc<RuntimeStore>,
    handle: ProviderHandle,
    commands: Receiver<SessionCommand>,
    shutdown: Arc<AtomicBool>,
    unknown_threads: Arc<AtomicU64>,
    request_timeout_ms: u64,
) {
    let mut event_state = SessionEventState::default();
    loop {
        loop {
            match commands.try_recv() {
                Ok(SessionCommand::StartTurn(intent, reply)) => {
                    let intent = *intent;
                    if !attempt_can_submit(&store, &intent) {
                        let _ignored = reply.send(TurnStart::NotSubmitted);
                        continue;
                    }
                    let Some(thread_id) = intent.thread_id.as_ref() else {
                        let _ignored = reply.send(TurnStart::Blocked {
                            reason: "This turn requires an existing provider thread.".to_owned(),
                        });
                        continue;
                    };
                    if event_state.resumed_threads.contains(thread_id) {
                        submit_turn_start(
                            &mut session,
                            &store,
                            &mut event_state.pending,
                            intent,
                            reply,
                            request_timeout_ms,
                        );
                    } else {
                        let mut params = json!({
                            "threadId": thread_id,
                            "excludeTurns": true,
                            "cwd": intent.cwd
                        });
                        if let Some(model) = &intent.model {
                            params["model"] = Value::String(model.clone());
                        }
                        match session.send_request(
                            Method::ThreadResume,
                            params,
                            Some(thread_id.clone()),
                            false,
                            DEFAULT_REQUEST_TIMEOUT_MS,
                        ) {
                            Ok(ticket) => {
                                event_state.pending.insert(
                                    ticket.id.clone(),
                                    PendingStart {
                                        intent,
                                        reply: Some(reply),
                                        stage: PendingStartStage::Resume,
                                    },
                                );
                            }
                            Err(_) => {
                                let _ignored = reply.send(TurnStart::Blocked {
                                    reason: "Provider thread resume could not be submitted; input remains pending."
                                        .to_owned(),
                                });
                            }
                        }
                    }
                }
                Ok(SessionCommand::Shutdown) => {
                    settle_active_on_close(&store, &mut event_state.active);
                    notify_pending_unknown(&mut event_state.pending);
                    let _ignored = session.close();
                    return;
                }
                Err(TryRecvError::Disconnected) => {
                    settle_active_on_close(&store, &mut event_state.active);
                    notify_pending_unknown(&mut event_state.pending);
                    let _ignored = session.close();
                    return;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if shutdown.load(Ordering::Acquire) {
            settle_active_on_close(&store, &mut event_state.active);
            notify_pending_unknown(&mut event_state.pending);
            let _ignored = session.close();
            return;
        }
        match session.receive_event_timeout(SESSION_POLL) {
            Ok(Some(event)) => handle_provider_event(
                &store,
                &handle,
                &mut session,
                event,
                &mut event_state,
                unknown_threads.as_ref(),
                request_timeout_ms,
            ),
            Ok(None) => {}
            Err(ProviderSessionError::Closed) => {
                settle_active_on_close(&store, &mut event_state.active);
                notify_pending_unknown(&mut event_state.pending);
                let _ignored = session.close();
                return;
            }
            Err(_) => {
                settle_active_on_close(&store, &mut event_state.active);
                notify_pending_unknown(&mut event_state.pending);
                let _ignored = session.close();
                return;
            }
        }
    }
}

fn attempt_can_submit(store: &RuntimeStore, intent: &TurnIntent) -> bool {
    store
        .read()
        .and_then(|connection| {
            SchedulerRepository::attempt_can_submit(
                &connection,
                &intent.agent_id,
                intent.epoch,
                &intent.attempt_id,
            )
        })
        .unwrap_or(false)
}

fn submit_turn_start(
    session: &mut ProviderSession,
    store: &RuntimeStore,
    pending: &mut HashMap<RequestId, PendingStart>,
    intent: TurnIntent,
    reply: Sender<TurnStart>,
    request_timeout_ms: u64,
) {
    if !attempt_can_submit(store, &intent) {
        let _ignored = reply.send(TurnStart::NotSubmitted);
        return;
    }
    let params = turn_start_params(&intent);
    match session.send_request(
        Method::TurnStart,
        params,
        intent.thread_id.clone(),
        false,
        request_timeout_ms,
    ) {
        Ok(ticket) => {
            pending.insert(
                ticket.id.clone(),
                PendingStart {
                    intent,
                    reply: Some(reply),
                    stage: PendingStartStage::Turn,
                },
            );
        }
        Err(_) => {
            let _ignored = reply.send(TurnStart::NotSubmitted);
        }
    }
}

fn handle_provider_event(
    store: &RuntimeStore,
    handle: &ProviderHandle,
    session: &mut ProviderSession,
    event: ProviderSessionEvent,
    state: &mut SessionEventState,
    unknown_threads: &AtomicU64,
    request_timeout_ms: u64,
) {
    let pending = &mut state.pending;
    let resumed_threads = &mut state.resumed_threads;
    let active = &mut state.active;
    match event {
        ProviderSessionEvent::Reply {
            ticket,
            body,
            late: _,
        } if ticket.method == Method::ThreadResume => {
            let Some(mut start) = pending.remove(&ticket.id) else {
                return;
            };
            if start.stage != PendingStartStage::Resume {
                return;
            }
            let resumed_id = match body {
                ResponseBody::Result(result) => result
                    .get("thread")
                    .and_then(|thread| thread.get("id"))
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                ResponseBody::Error(_) => None,
            };
            if resumed_id.as_deref() != start.intent.thread_id.as_deref() {
                if let Some(reply) = start.reply.take() {
                    let _ignored = reply.send(TurnStart::Blocked {
                        reason: "Provider thread resume failed; input remains pending.".to_owned(),
                    });
                }
                return;
            }
            let Some(thread_id) = start.intent.thread_id.clone() else {
                if let Some(reply) = start.reply.take() {
                    let _ignored = reply.send(TurnStart::Blocked {
                        reason: "Provider thread resume returned no thread identity.".to_owned(),
                    });
                }
                return;
            };
            resumed_threads.insert(thread_id);
            if let Some(reply) = start.reply.take() {
                submit_turn_start(
                    session,
                    store,
                    pending,
                    start.intent,
                    reply,
                    request_timeout_ms,
                );
            }
        }
        ProviderSessionEvent::Reply {
            ticket,
            body,
            late: _,
        } if ticket.method == Method::TurnStart => {
            let Some(mut start) = pending.remove(&ticket.id) else {
                return;
            };
            match body {
                ResponseBody::Result(result) => {
                    let Some(turn_id) = result
                        .get("turn")
                        .and_then(|turn| turn.get("id"))
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_owned)
                    else {
                        if let Some(reply) = start.reply.take() {
                            let _ignored = reply.send(TurnStart::Unknown);
                        }
                        pending.insert(ticket.id, start);
                        return;
                    };
                    let identity = identity_for(&start.intent);
                    if !settle_started_attempt(store, &identity, &turn_id).unwrap_or(false) {
                        if let Some(reply) = start.reply.take() {
                            let _ignored = reply.send(TurnStart::Unknown);
                        }
                        return;
                    }
                    if let Some(reply) = start.reply.take() {
                        let _ignored = reply.send(TurnStart::Accepted {
                            turn_id: turn_id.clone(),
                        });
                    }
                    if !terminal_turn_matches(store, &identity, &turn_id)
                        && let Some(thread_id) = start.intent.thread_id
                    {
                        active.insert((thread_id, turn_id), identity);
                    }
                }
                ResponseBody::Error(_) => {
                    if let Some(reply) = start.reply.take() {
                        let _ignored = reply.send(TurnStart::NotSubmitted);
                    }
                }
            }
        }
        ProviderSessionEvent::UnknownOutcome { ticket } if ticket.method == Method::TurnStart => {
            if let Some(start) = pending.get_mut(&ticket.id)
                && start.stage == PendingStartStage::Turn
                && let Some(reply) = start.reply.take()
            {
                let _ignored = reply.send(TurnStart::Unknown);
            }
        }
        ProviderSessionEvent::UnknownOutcome { ticket }
            if ticket.method == Method::ThreadResume =>
        {
            if let Some(mut start) = pending.remove(&ticket.id)
                && let Some(reply) = start.reply.take()
            {
                let _ignored = reply.send(TurnStart::Blocked {
                    reason: "Provider thread resume outcome is unknown; input remains pending."
                        .to_owned(),
                });
            }
        }
        ProviderSessionEvent::Inbound(Inbound::Notification(frame)) => {
            if let Frame::Notification(notification) = frame {
                route_notification(
                    store,
                    handle,
                    notification.method,
                    notification.params.unwrap_or(Value::Null),
                    active,
                    unknown_threads,
                );
            }
        }
        ProviderSessionEvent::Closed { .. } => {}
        ProviderSessionEvent::Reply { .. }
        | ProviderSessionEvent::UnknownOutcome { .. }
        | ProviderSessionEvent::Inbound(Inbound::Request(_)) => {}
    }
}

fn route_notification(
    store: &RuntimeStore,
    handle: &ProviderHandle,
    method: Method,
    params: Value,
    active: &mut HashMap<(String, String), TurnAttemptIdentity>,
    unknown_threads: &AtomicU64,
) {
    if !matches!(method, Method::TurnStarted | Method::TurnCompleted) {
        return;
    }
    let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
        return;
    };
    let Some(turn) = params.get("turn") else {
        return;
    };
    let Some(turn_id) = turn
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let lookup = store.read().and_then(|connection| {
        SchedulerRepository::find_attempt_for_thread(
            &connection,
            handle.account_key(),
            thread_id,
            Some(turn_id),
        )
    });
    let Ok((thread_known, identity)) = lookup else {
        return;
    };
    if !thread_known {
        unknown_threads.fetch_add(1, Ordering::Relaxed);
        return;
    }
    let Some(identity) = identity else {
        return;
    };
    if method == Method::TurnStarted {
        if settle_started_attempt(store, &identity, turn_id).unwrap_or(false) {
            active.insert((thread_id.to_owned(), turn_id.to_owned()), identity);
        }
        return;
    }
    let outcome = match turn.get("status").and_then(Value::as_str) {
        Some("completed") => TurnOutcome::Completed,
        Some("failed") => TurnOutcome::Failed {
            error: turn.get("error").cloned(),
        },
        Some("interrupted") => TurnOutcome::Interrupted {
            error: turn.get("error").cloned(),
        },
        _ => return,
    };
    if settle_terminal_attempt(store, &identity, turn_id, outcome).unwrap_or(false) {
        active.remove(&(thread_id.to_owned(), turn_id.to_owned()));
    }
}

fn settle_active_on_close(
    store: &RuntimeStore,
    active: &mut HashMap<(String, String), TurnAttemptIdentity>,
) {
    for ((_, turn_id), identity) in active.drain() {
        let _ignored = settle_terminal_attempt(
            store,
            &identity,
            &turn_id,
            TurnOutcome::Interrupted {
                error: Some(json!({"message":"Provider process ended during the turn."})),
            },
        );
    }
}

fn notify_pending_unknown(pending: &mut HashMap<RequestId, PendingStart>) {
    for start in pending.values_mut() {
        if let Some(reply) = start.reply.take() {
            let outcome = match start.stage {
                PendingStartStage::Resume => TurnStart::Blocked {
                    reason: "Provider thread resume ended before user input was submitted; input remains pending."
                        .to_owned(),
                },
                PendingStartStage::Turn => TurnStart::Unknown,
            };
            let _ignored = reply.send(outcome);
        }
    }
}

fn identity_for(intent: &TurnIntent) -> TurnAttemptIdentity {
    TurnAttemptIdentity {
        agent_id: intent.agent_id.clone(),
        epoch: intent.epoch,
        attempt_id: intent.attempt_id.clone(),
    }
}

fn terminal_turn_matches(
    store: &RuntimeStore,
    identity: &TurnAttemptIdentity,
    turn_id: &str,
) -> bool {
    store
        .read()
        .ok()
        .and_then(|connection| {
            AgentRepository::get(&connection, &AgentId::from(identity.agent_id.as_str()))
                .ok()
                .flatten()
        })
        .is_some_and(|agent| {
            agent.epoch == identity.epoch
                && agent.extra.get("lastCompletedTurn").and_then(Value::as_str) == Some(turn_id)
        })
}

fn turn_start_params(intent: &TurnIntent) -> Value {
    let text = intent
        .events
        .iter()
        .map(|event| event.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut params = json!({
        "threadId": intent.thread_id,
        "cwd": intent.cwd,
        "clientUserMessageId": intent.events.first().map(|event| event.id.as_str()),
        "input": [{"type":"text", "text":text}]
    });
    if let Some(model) = &intent.model {
        params["model"] = Value::String(model.clone());
    }
    params
}

struct PendingWake(bool);

impl crate::scheduler::WakeSignal for PendingWake {
    fn consume(&mut self) -> bool {
        std::mem::replace(&mut self.0, false)
    }
}

struct ProviderTurnDriver {
    sessions: Arc<SessionRegistry>,
}

impl ProviderTurnDriver {
    #[must_use]
    fn new(sessions: Arc<SessionRegistry>) -> Self {
        Self { sessions }
    }
}

impl TurnDriver for ProviderTurnDriver {
    fn start(&mut self, intent: TurnIntent) -> TurnStart {
        self.sessions.start_turn(intent)
    }
}

fn scheduler_loop(
    store: Arc<RuntimeStore>,
    sessions: Arc<SessionRegistry>,
    shutdown: Arc<AtomicBool>,
    wake: Receiver<()>,
) {
    let (finished_tx, finished_rx) = mpsc::channel::<String>();
    let mut active = Vec::<String>::new();
    let mut workers = HashMap::<String, JoinHandle<()>>::new();
    while !shutdown.load(Ordering::Acquire) {
        match wake.recv_timeout(SCHEDULER_POLL) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(agent_id) = finished_rx.try_recv() {
            active.retain(|active_id| active_id != &agent_id);
            if let Some(worker) = workers.remove(&agent_id) {
                let _ignored = worker.join();
            }
        }
        if shutdown.load(Ordering::Acquire) {
            break;
        }
        let candidates = store
            .read()
            .and_then(|connection| SchedulerRepository::pending_agents(&connection));
        let Ok(candidates) = candidates else {
            continue;
        };
        for (agent_id, epoch) in candidates {
            if active.contains(&agent_id) {
                continue;
            }
            active.push(agent_id.clone());
            let worker_store = Arc::clone(&store);
            let worker_sessions = Arc::clone(&sessions);
            let finished = finished_tx.clone();
            let worker_id = agent_id.clone();
            let worker = thread::spawn(move || {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0.0, |duration| duration.as_secs_f64());
                let attempt_id = format!("rust-turn-{epoch}-{}", now.to_bits());
                let mut driver = ProviderTurnDriver::new(worker_sessions);
                let mut wake = PendingWake(true);
                let _ignored = consume_wake(
                    &worker_store,
                    &mut wake,
                    &worker_id,
                    epoch,
                    &attempt_id,
                    now,
                    &mut driver,
                );
                let _ignored = finished.send(worker_id);
            });
            workers.insert(agent_id, worker);
        }
    }
    for worker in workers.into_values() {
        let _ignored = worker.join();
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;
    use std::io::{self, Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::sync::{Condvar, Mutex};

    use crate::event_write::{AnnotationWrite, annotate};
    use crate::scheduler::consume_wake;
    use serde_json::Value;
    use studio_runtime_store::{AgentId, AgentRepository, AgentStatus, RuntimeStore};

    use super::*;

    const CREATE_AGENTS: &str = r#"
import json, pathlib, sys
sys.path.insert(0, sys.argv[2])
from codex_runtime import Runtime
class QuietRuntime(Runtime):
    def schedule(self):
        return
root = pathlib.Path(sys.argv[1])
runtime = QuietRuntime(root, lambda *_args, **_kwargs: None)
agents = []
for index in range(int(sys.argv[3])):
    agent = runtime.create({"name":"Rust turn fixture", "cwd":str(root), "prompt":"temporary"}, defer=True)
    agent.update(autoWake=True, status="queued", accountKey="default",
                 threadId="fake-thread-" + str(index), model="fake-model")
    with runtime.lock, runtime.db() as db:
        runtime.put(db, "agents", agent)
    agents.append({"id":agent["id"], "epoch":agent["epoch"]})
print(json.dumps(agents))
runtime.close()
"#;

    static TEMP_STATE_ID: AtomicUsize = AtomicUsize::new(0);

    struct TemporaryState(PathBuf);

    impl TemporaryState {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock follows Unix epoch")
                .as_nanos();
            let id = TEMP_STATE_ID.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "studio-turn-runtime-{}-{nonce}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(&root).expect("temporary state directory creates");
            Self(root)
        }
    }

    impl Drop for TemporaryState {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    struct FakeSignals {
        stops: Mutex<usize>,
        changed: Condvar,
        starts: AtomicUsize,
    }

    impl FakeSignals {
        fn wait_for_stops(&self, expected: usize) -> bool {
            let Ok(stops) = self.stops.lock() else {
                return false;
            };
            self.changed
                .wait_timeout_while(stops, Duration::from_secs(20), |count| *count < expected)
                .is_ok_and(|(count, timeout)| *count >= expected && !timeout.timed_out())
        }
    }

    struct FakeFactory {
        mode: String,
        signals: Arc<FakeSignals>,
    }

    impl FakeFactory {
        fn new(mode: &str) -> (Self, Arc<FakeSignals>) {
            let signals = Arc::new(FakeSignals {
                stops: Mutex::new(0),
                changed: Condvar::new(),
                starts: AtomicUsize::new(0),
            });
            (
                Self {
                    mode: mode.to_owned(),
                    signals: Arc::clone(&signals),
                },
                signals,
            )
        }
    }

    impl ProviderProcessFactory for FakeFactory {
        fn start_or_reattach(
            &self,
            handle: &ProviderHandle,
        ) -> Result<Box<dyn ProviderProcess>, ProviderFactoryError> {
            if !handle.as_str().starts_with("account:") {
                return Err(ProviderFactoryError);
            }
            self.signals.starts.fetch_add(1, Ordering::Relaxed);
            let fixture = format!(
                "{}/tests/fixtures/fake_provider.py",
                env!("CARGO_MANIFEST_DIR")
            );
            let mut child = Command::new("python3")
                .arg(fixture)
                .env("FAKE_PROVIDER_MODE", &self.mode)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|_| ProviderFactoryError)?;
            let stdout = child.stdout.take();
            let stdin = child.stdin.take();
            Ok(Box::new(FakeProcess {
                child,
                stdout,
                stdin,
                signals: Arc::clone(&self.signals),
            }))
        }
    }

    struct FakeProcess {
        child: Child,
        stdout: Option<ChildStdout>,
        stdin: Option<ChildStdin>,
        signals: Arc<FakeSignals>,
    }

    impl ProviderProcess for FakeProcess {
        fn split(&mut self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
            let stdout = self
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("fake provider stdout already taken"))?;
            let stdin = self
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("fake provider stdin already taken"))?;
            Ok((Box::new(stdout), Box::new(stdin)))
        }

        fn stop(&mut self) -> io::Result<()> {
            let _ignored = self.child.kill();
            let result = self.child.wait().map(|_| ());
            if let Ok(mut stops) = self.signals.stops.lock() {
                *stops += 1;
                self.signals.changed.notify_all();
            }
            result
        }
    }

    struct ControlledProvider {
        requests: Mutex<Vec<Value>>,
        output: Mutex<Option<Sender<Option<Vec<u8>>>>>,
        signals: Arc<FakeSignals>,
        auto_accept: bool,
    }

    impl ControlledProvider {
        fn send_frame(&self, frame: Value) {
            let bytes = format!(
                "{}\n",
                serde_json::to_string(&frame).expect("frame serializes")
            );
            self.output
                .lock()
                .expect("fake output lock is available")
                .as_ref()
                .expect("fake output is connected")
                .send(Some(bytes.into_bytes()))
                .expect("fake reader remains connected");
        }

        fn close_output(&self) {
            if let Some(output) = self
                .output
                .lock()
                .expect("fake output lock is available")
                .take()
            {
                let _ignored = output.send(None);
            }
        }
    }

    struct ControlledFactory(Arc<ControlledProvider>);

    impl ProviderProcessFactory for ControlledFactory {
        fn start_or_reattach(
            &self,
            handle: &ProviderHandle,
        ) -> Result<Box<dyn ProviderProcess>, ProviderFactoryError> {
            if !handle.as_str().starts_with("account:") {
                return Err(ProviderFactoryError);
            }
            self.0.signals.starts.fetch_add(1, Ordering::Relaxed);
            let (output, input) = mpsc::channel::<Option<Vec<u8>>>();
            *self.0.output.lock().map_err(|_| ProviderFactoryError)? = Some(output.clone());
            Ok(Box::new(ControlledProcess {
                provider: Arc::clone(&self.0),
                input: Some(input),
                output,
            }))
        }
    }

    struct ReturningUnknown;

    impl TurnDriver for ReturningUnknown {
        fn start(&mut self, _intent: TurnIntent) -> TurnStart {
            TurnStart::Unknown
        }
    }

    struct ControlledProcess {
        provider: Arc<ControlledProvider>,
        input: Option<Receiver<Option<Vec<u8>>>>,
        output: Sender<Option<Vec<u8>>>,
    }

    impl ProviderProcess for ControlledProcess {
        fn split(&mut self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
            let input = self
                .input
                .take()
                .ok_or_else(|| io::Error::other("fake reader already taken"))?;
            Ok((
                Box::new(ControlledReader {
                    input,
                    buffered: VecDeque::new(),
                    ended: false,
                }),
                Box::new(ControlledWriter {
                    provider: Arc::clone(&self.provider),
                    output: self.output.clone(),
                    buffered: Vec::new(),
                }),
            ))
        }

        fn stop(&mut self) -> io::Result<()> {
            self.provider.close_output();
            if let Ok(mut stops) = self.provider.signals.stops.lock() {
                *stops += 1;
                self.provider.signals.changed.notify_all();
            }
            Ok(())
        }
    }

    struct ControlledReader {
        input: Receiver<Option<Vec<u8>>>,
        buffered: VecDeque<u8>,
        ended: bool,
    }

    impl Read for ControlledReader {
        fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
            while self.buffered.is_empty() && !self.ended {
                match self.input.recv() {
                    Ok(Some(bytes)) => self.buffered.extend(bytes),
                    Ok(None) | Err(_) => self.ended = true,
                }
            }
            if self.buffered.is_empty() {
                return Ok(0);
            }
            let count = target.len().min(self.buffered.len());
            for slot in &mut target[..count] {
                *slot = self.buffered.pop_front().expect("buffered byte exists");
            }
            Ok(count)
        }
    }

    struct ControlledWriter {
        provider: Arc<ControlledProvider>,
        output: Sender<Option<Vec<u8>>>,
        buffered: Vec<u8>,
    }

    impl Write for ControlledWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.buffered.extend_from_slice(bytes);
            while let Some(end) = self.buffered.iter().position(|byte| *byte == b'\n') {
                let line = self.buffered.drain(..=end).collect::<Vec<_>>();
                let request: Value = serde_json::from_slice(&line)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                match request.get("method").and_then(Value::as_str) {
                    Some("initialize") => {
                        let response = json!({
                            "id": request["id"],
                            "result": {"protocolVersion":1}
                        });
                        let bytes = format!(
                            "{}\n",
                            serde_json::to_string(&response).expect("init response serializes")
                        );
                        self.output.send(Some(bytes.into_bytes())).map_err(|_| {
                            io::Error::new(io::ErrorKind::BrokenPipe, "fake reader closed")
                        })?;
                    }
                    Some("thread/resume") => {
                        self.provider.send_frame(json!({
                            "id":request["id"],
                            "result":{"thread":{"id":request["params"]["threadId"]}}
                        }));
                    }
                    Some("turn/start") => {
                        self.provider
                            .requests
                            .lock()
                            .map_err(|_| io::Error::other("fake request lock poisoned"))?
                            .push(request.clone());
                        if self.provider.auto_accept {
                            let turn_id = format!(
                                "turn-{}",
                                request["params"]["clientUserMessageId"]
                                    .as_str()
                                    .expect("turn/start has a client message ID")
                            );
                            self.provider.send_frame(json!({
                                "id":request["id"],
                                "result":{"turn":{"id":turn_id}}
                            }));
                        }
                    }
                    _ => {}
                }
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn fixture_agents(root: &Path, count: usize) -> Vec<(String, i64)> {
        let output = Command::new(python_executable())
            .arg("-c")
            .arg(CREATE_AGENTS)
            .arg(root)
            .arg(server_source())
            .arg(count.to_string())
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .expect("Python fixture starts");
        assert!(
            output.status.success(),
            "Python fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<Vec<Value>>(&output.stdout)
            .expect("Python fixture returns JSON")
            .into_iter()
            .map(|agent| {
                (
                    agent["id"]
                        .as_str()
                        .expect("agent id is a string")
                        .to_owned(),
                    agent["epoch"].as_i64().expect("agent epoch is an integer"),
                )
            })
            .collect()
    }

    fn write_annotation(store: &RuntimeStore, agent_id: &str, id: &str) {
        store
            .transaction(|transaction| {
                annotate(
                    transaction,
                    AnnotationWrite {
                        agent_id: agent_id.to_owned(),
                        id: Some(id.to_owned()),
                        path: "README.md".to_owned(),
                        line: 1,
                        text: "fake-provider annotation".to_owned(),
                        turn_id: None,
                        signature: id.to_owned(),
                    },
                )
                .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                Ok(())
            })
            .expect("annotation creates a pending scheduler event");
    }

    fn python_executable() -> PathBuf {
        if let Some(python) = std::env::var_os("CODEX_AGENTS_PYTHON") {
            return python.into();
        }
        let output = Command::new("python3")
            .arg(server_source().join("codex_python.py"))
            .output()
            .expect("Python resolver starts");
        assert!(output.status.success(), "Python resolver succeeds");
        String::from_utf8(output.stdout)
            .expect("resolved interpreter is UTF-8")
            .trim()
            .into()
    }

    fn server_source() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src")
    }

    #[test]
    fn event_write_annotation_runs_to_completion_on_one_account_session() {
        let state = TemporaryState::new();
        let agents = fixture_agents(&state.0, 1);
        let [(agent_id, _epoch)] = agents.as_slice() else {
            panic!("one fixture agent is created");
        };
        let store = Arc::new(RuntimeStore::open(Some(&state.0)).expect("Rust opens fixture state"));
        write_annotation(&store, agent_id, "fake-annotation");
        let (factory, signals) = FakeFactory::new("turn-complete");
        let mut runtime =
            TurnRuntime::start(Arc::clone(&store), Arc::new(factory)).expect("turn runtime starts");
        runtime.wake_handle().wake();
        assert!(signals.wait_for_stops(1), "fake provider session completes");
        assert_eq!(signals.starts.load(Ordering::Relaxed), 1);
        assert_eq!(runtime.unknown_thread_notifications(), 1);
        let connection = store.read().expect("completed state reads");
        let agent = AgentRepository::get(&connection, &AgentId::from(agent_id.as_str()))
            .expect("agent reads")
            .expect("agent exists");
        assert_eq!(agent.status, AgentStatus::Completed);
        assert_eq!(agent.extra["lastCompletedTurnStatus"], "completed");
        let delivered: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE agent=?1 AND kind='user' AND status='delivered'",
                [agent_id],
                |row| row.get(0),
            )
            .expect("annotation delivery reads");
        assert_eq!(delivered, 1);
        drop(connection);
        runtime
            .shutdown()
            .expect("runtime joins scheduler and session");
    }

    #[test]
    fn lost_provider_reply_keeps_event_uncertain_without_retry() {
        let state = TemporaryState::new();
        let agents = fixture_agents(&state.0, 1);
        let [(agent_id, epoch)] = agents.as_slice() else {
            panic!("one fixture agent is created");
        };
        let store = Arc::new(RuntimeStore::open(Some(&state.0)).expect("Rust opens fixture state"));
        write_annotation(&store, agent_id, "lost-annotation");
        let (factory, signals) = FakeFactory::new("turn-lost");
        let shutdown = Arc::new(AtomicBool::new(false));
        let sessions = Arc::new(SessionRegistry::new(
            Arc::clone(&store),
            Arc::new(factory),
            Arc::clone(&shutdown),
            DEFAULT_REQUEST_TIMEOUT_MS,
        ));
        let mut driver = ProviderTurnDriver::new(sessions.clone());
        let mut wake = PendingWake(true);
        consume_wake(
            &store,
            &mut wake,
            agent_id,
            *epoch,
            "lost-reply-attempt",
            1.0,
            &mut driver,
        )
        .expect("unknown provider result persists");
        let connection = store.read().expect("unknown result reads");
        let status: String = connection
            .query_row(
                "SELECT status FROM runtime_events WHERE id='annotation:lost-annotation'",
                [],
                |row| row.get(0),
            )
            .expect("uncertain event reads");
        assert_eq!(status, "uncertain");
        assert!(signals.wait_for_stops(1), "exited fake process is joined");
        sessions.shutdown().expect("session reader joins");
    }

    #[test]
    fn shutdown_interrupts_the_active_turn_and_joins_the_shared_session() {
        let state = TemporaryState::new();
        let agents = fixture_agents(&state.0, 1);
        let [(agent_id, epoch)] = agents.as_slice() else {
            panic!("one fixture agent is created");
        };
        let store = Arc::new(RuntimeStore::open(Some(&state.0)).expect("Rust opens fixture state"));
        write_annotation(&store, agent_id, "shutdown-annotation");
        let (factory, signals) = FakeFactory::new("turn-hang");
        let shutdown = Arc::new(AtomicBool::new(false));
        let sessions = Arc::new(SessionRegistry::new(
            Arc::clone(&store),
            Arc::new(factory),
            Arc::clone(&shutdown),
            DEFAULT_REQUEST_TIMEOUT_MS,
        ));
        let mut driver = ProviderTurnDriver::new(Arc::clone(&sessions));
        let mut wake = PendingWake(true);
        consume_wake(
            &store,
            &mut wake,
            agent_id,
            *epoch,
            "shutdown-attempt",
            1.0,
            &mut driver,
        )
        .expect("provider accepts active turn");
        sessions.shutdown().expect("reader closes and joins");
        assert!(signals.wait_for_stops(1), "fake process is stopped");
        let connection = store.read().expect("interrupted state reads");
        let agent = AgentRepository::get(&connection, &AgentId::from(agent_id.as_str()))
            .expect("agent reads")
            .expect("agent exists");
        assert_eq!(agent.status, AgentStatus::Interrupted);
        assert_eq!(agent.extra["lastCompletedTurnStatus"], "interrupted");
    }

    #[test]
    fn provider_exit_interrupts_the_active_turn() {
        let state = TemporaryState::new();
        let agents = fixture_agents(&state.0, 1);
        let [(agent_id, epoch)] = agents.as_slice() else {
            panic!("one fixture agent is created");
        };
        let store = Arc::new(RuntimeStore::open(Some(&state.0)).expect("Rust opens fixture state"));
        write_annotation(&store, agent_id, "provider-exit-annotation");
        let (factory, signals) = FakeFactory::new("turn-exit-mid");
        let shutdown = Arc::new(AtomicBool::new(false));
        let sessions = Arc::new(SessionRegistry::new(
            Arc::clone(&store),
            Arc::new(factory),
            Arc::clone(&shutdown),
            DEFAULT_REQUEST_TIMEOUT_MS,
        ));
        let mut driver = ProviderTurnDriver::new(Arc::clone(&sessions));
        let mut wake = PendingWake(true);
        consume_wake(
            &store,
            &mut wake,
            agent_id,
            *epoch,
            "provider-exit-attempt",
            1.0,
            &mut driver,
        )
        .expect("provider accepts active turn before exiting");
        assert!(
            signals.wait_for_stops(1),
            "provider exit closes and joins the reader"
        );
        sessions
            .shutdown()
            .expect("reader joins after provider exit");
        let connection = store.read().expect("provider exit state reads");
        let agent = AgentRepository::get(&connection, &AgentId::from(agent_id.as_str()))
            .expect("agent reads")
            .expect("agent exists");
        assert_eq!(agent.status, AgentStatus::Interrupted);
        assert_eq!(agent.extra["lastCompletedTurnStatus"], "interrupted");
    }

    #[test]
    fn late_reply_after_unknown_is_bound_by_attempt_and_epoch() {
        let state = TemporaryState::new();
        let agents = fixture_agents(&state.0, 1);
        let store = Arc::new(RuntimeStore::open(Some(&state.0)).expect("Rust opens fixture state"));
        write_annotation(&store, &agents[0].0, "late-annotation-0");
        let signals = Arc::new(FakeSignals {
            stops: Mutex::new(0),
            changed: Condvar::new(),
            starts: AtomicUsize::new(0),
        });
        let provider = Arc::new(ControlledProvider {
            requests: Mutex::new(Vec::new()),
            output: Mutex::new(None),
            signals: Arc::clone(&signals),
            auto_accept: false,
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let sessions = Arc::new(SessionRegistry::new(
            Arc::clone(&store),
            Arc::new(ControlledFactory(Arc::clone(&provider))),
            Arc::clone(&shutdown),
            0,
        ));
        let mut first_driver = ProviderTurnDriver::new(Arc::clone(&sessions));
        let mut first_wake = PendingWake(true);
        consume_wake(
            &store,
            &mut first_wake,
            &agents[0].0,
            agents[0].1,
            "late-attempt-0",
            1.0,
            &mut first_driver,
        )
        .expect("first start becomes unknown before a late reply");
        {
            let connection = store.read().expect("unknown outcome reads");
            let agent = AgentRepository::get(&connection, &AgentId::from(agents[0].0.as_str()))
                .expect("agent reads")
                .expect("agent exists");
            assert_eq!(agent.status, AgentStatus::Waiting);
            assert!(
                agent.extra["startAttempt"]["submitted"]
                    .as_bool()
                    .unwrap_or(false)
            );
            assert!(agent.extra["inFlight"].as_bool().unwrap_or(false));
        }
        let request_id = provider
            .requests
            .lock()
            .expect("fake requests are available")
            .first()
            .expect("turn/start was sent")
            .get("id")
            .cloned()
            .expect("request has an id");
        let thread_id = "fake-thread-0";
        provider.send_frame(json!({
            "id": request_id,
            "result": {"turn":{"id":"late-turn"}}
        }));
        provider.send_frame(json!({
            "method":"turn/started",
            "params":{"threadId":thread_id,"turn":{"id":"late-turn"}}
        }));
        provider.send_frame(json!({
            "method":"turn/completed",
            "params":{"threadId":thread_id,"turn":{"id":"late-turn","status":"completed"}}
        }));
        provider.close_output();
        assert!(
            signals.wait_for_stops(1),
            "fake provider consumes the late reply and exits"
        );
        assert_eq!(signals.starts.load(Ordering::Relaxed), 1);
        sessions.shutdown().expect("shared provider reader joins");
        let connection = store.read().expect("late settlement reads");
        for (agent_id, _) in &agents {
            let agent = AgentRepository::get(&connection, &AgentId::from(agent_id.as_str()))
                .expect("agent reads")
                .expect("agent exists");
            assert_eq!(agent.status, AgentStatus::Completed);
            assert_eq!(agent.extra["lastCompletedTurnStatus"], "completed");
        }
        let delivered: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM runtime_events WHERE status='delivered' AND id LIKE 'annotation:late-annotation-%'",
                [],
                |row| row.get(0),
            )
            .expect("late delivered event count reads");
        assert_eq!(delivered, 1);
    }

    #[test]
    fn agents_on_one_account_share_a_session_without_sharing_turn_state() {
        let state = TemporaryState::new();
        let agents = fixture_agents(&state.0, 2);
        let store = Arc::new(RuntimeStore::open(Some(&state.0)).expect("Rust opens fixture state"));
        write_annotation(&store, &agents[0].0, "shared-session-0");
        write_annotation(&store, &agents[1].0, "shared-session-1");
        let signals = Arc::new(FakeSignals {
            stops: Mutex::new(0),
            changed: Condvar::new(),
            starts: AtomicUsize::new(0),
        });
        let provider = Arc::new(ControlledProvider {
            requests: Mutex::new(Vec::new()),
            output: Mutex::new(None),
            signals: Arc::clone(&signals),
            auto_accept: true,
        });
        let shutdown = Arc::new(AtomicBool::new(false));
        let sessions = Arc::new(SessionRegistry::new(
            Arc::clone(&store),
            Arc::new(ControlledFactory(Arc::clone(&provider))),
            Arc::clone(&shutdown),
            DEFAULT_REQUEST_TIMEOUT_MS,
        ));

        for (index, (agent_id, epoch)) in agents.iter().enumerate() {
            let mut driver = ProviderTurnDriver::new(Arc::clone(&sessions));
            let mut wake = PendingWake(true);
            consume_wake(
                &store,
                &mut wake,
                agent_id,
                *epoch,
                &format!("shared-attempt-{index}"),
                index as f64,
                &mut driver,
            )
            .expect("each agent receives its own accepted turn");
        }
        let requests = provider
            .requests
            .lock()
            .expect("fake requests are available")
            .clone();
        assert_eq!(signals.starts.load(Ordering::Relaxed), 1);
        assert_eq!(requests.len(), 2);
        assert_ne!(
            requests[0]["params"]["threadId"],
            requests[1]["params"]["threadId"]
        );

        for request in requests {
            let turn_id = format!(
                "turn-{}",
                request["params"]["clientUserMessageId"]
                    .as_str()
                    .expect("turn/start has a client message ID")
            );
            provider.send_frame(json!({
                "method":"turn/completed",
                "params":{
                    "threadId":request["params"]["threadId"],
                    "turn":{"id":turn_id,"status":"completed"}
                }
            }));
        }
        provider.close_output();
        assert!(
            signals.wait_for_stops(1),
            "shared session consumes both completions"
        );
        sessions.shutdown().expect("shared reader joins");

        let connection = store.read().expect("shared turn state reads");
        for (agent_id, _) in agents {
            let agent = AgentRepository::get(&connection, &AgentId::from(agent_id.as_str()))
                .expect("agent reads")
                .expect("agent exists");
            assert_eq!(agent.status, AgentStatus::Completed);
        }
    }

    #[test]
    fn startup_reattaches_to_settle_a_submitted_unknown_turn_without_resending() {
        let state = TemporaryState::new();
        let agents = fixture_agents(&state.0, 1);
        let store = Arc::new(RuntimeStore::open(Some(&state.0)).expect("Rust opens fixture state"));
        write_annotation(&store, &agents[0].0, "restart-annotation");
        let mut wake = PendingWake(true);
        consume_wake(
            &store,
            &mut wake,
            &agents[0].0,
            agents[0].1,
            "restart-unknown-attempt",
            1.0,
            &mut ReturningUnknown,
        )
        .expect("submitted turn is recorded as unknown");
        drop(store);

        let store =
            Arc::new(RuntimeStore::open(Some(&state.0)).expect("Rust reopens fixture state"));
        let signals = Arc::new(FakeSignals {
            stops: Mutex::new(0),
            changed: Condvar::new(),
            starts: AtomicUsize::new(0),
        });
        let provider = Arc::new(ControlledProvider {
            requests: Mutex::new(Vec::new()),
            output: Mutex::new(None),
            signals: Arc::clone(&signals),
            auto_accept: false,
        });
        let mut runtime = TurnRuntime::start(
            Arc::clone(&store),
            Arc::new(ControlledFactory(Arc::clone(&provider))),
        )
        .expect("runtime restarts and reattaches to the provider");
        assert_eq!(signals.starts.load(Ordering::Relaxed), 1);
        assert!(
            provider
                .requests
                .lock()
                .expect("fake requests are available")
                .is_empty()
        );

        provider.send_frame(json!({
            "method":"turn/started",
            "params":{"threadId":"fake-thread-0","turn":{"id":"recovered-turn"}}
        }));
        provider.send_frame(json!({
            "method":"turn/completed",
            "params":{"threadId":"fake-thread-0","turn":{"id":"recovered-turn","status":"completed"}}
        }));
        provider.close_output();
        assert!(
            signals.wait_for_stops(1),
            "reattached reader consumes terminal notices"
        );
        runtime
            .shutdown()
            .expect("runtime shuts down and joins its workers");

        let connection = store.read().expect("recovered turn state reads");
        let agent = AgentRepository::get(&connection, &AgentId::from(agents[0].0.as_str()))
            .expect("agent reads")
            .expect("agent exists");
        assert_eq!(agent.status, AgentStatus::Completed);
        assert_eq!(agent.extra["lastCompletedTurn"], "recovered-turn");
        let (status, turn_id): (String, String) = connection
            .query_row(
                "SELECT status,turn_id FROM runtime_events WHERE id='annotation:restart-annotation'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("recovered event status reads");
        assert_eq!(status, "delivered");
        assert_eq!(turn_id, "recovered-turn");
    }
}
