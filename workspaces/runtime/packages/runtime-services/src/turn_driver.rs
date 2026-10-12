//! Account-scoped provider sessions and asynchronous scheduler turn runtime.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_providers::{Frame, Inbound, Method, RequestId, ResponseBody};
use studio_runtime_store::{
    AgentId, AgentRepository, RuntimeStore, SchedulerRepository, StoreError, TurnAttemptIdentity,
};
use thiserror::Error;

#[cfg(test)]
use crate::provider_session::ProviderProcessReader;
use crate::provider_session::{
    ProviderProcess, ProviderSession, ProviderSessionConfig, ProviderSessionError,
    ProviderSessionEvent,
};
use crate::scheduler::{
    SchedulerError, TurnDriver, TurnIntent, TurnOutcome, TurnStart, attempt_can_prepare,
    consume_wake, mark_submitted_for_provider, recover_on_startup, settle_start_rejected_provider,
    settle_started_attempt, settle_started_provider, settle_terminal_attempt,
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

#[derive(Debug, Error)]
pub enum ProviderFactoryError {
    #[error(transparent)]
    Supervisor(#[from] studio_runtime_process::SupervisorError),
    #[error("provider reader failed: {0}")]
    Reader(#[from] std::io::Error),
    #[error("provider launch failed: {0}")]
    Launch(String),
}

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
    sessions: Mutex<HashMap<ProviderHandle, ManagedSession>>,
    unknown_threads: Arc<AtomicU64>,
    request_timeout_ms: u64,
}

struct ManagedSession {
    sender: Sender<SessionCommand>,
    worker: JoinHandle<()>,
}

enum SessionCommand {
    StartTurn(TurnIntent, Sender<TurnStart>),
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
            sessions: Mutex::new(HashMap::new()),
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
        let sender = match self.session_sender(&handle) {
            Ok(value) => value,
            Err(()) => return TurnStart::NotSubmitted,
        };
        let (reply, result) = mpsc::channel();
        if sender
            .send(SessionCommand::StartTurn(intent, reply))
            .is_err()
        {
            return TurnStart::NotSubmitted;
        }
        result.recv().unwrap_or(TurnStart::Unknown)
    }

    fn ensure_session(&self, handle: &ProviderHandle) -> Result<(), ()> {
        self.session_sender(handle).map(|_| ())
    }

    fn session_sender(&self, handle: &ProviderHandle) -> Result<Sender<SessionCommand>, ()> {
        if self.shutdown.load(Ordering::Acquire) {
            return Err(());
        }
        let mut sessions = self.sessions.lock().map_err(|_| ())?;
        if let Some(session) = sessions.get(handle) {
            return Ok(session.sender.clone());
        }
        let (sender, receiver) = mpsc::channel();
        let store = Arc::clone(&self.store);
        let factory = Arc::clone(&self.factory);
        let shutdown = Arc::clone(&self.shutdown);
        let unknown_threads = Arc::clone(&self.unknown_threads);
        let request_timeout_ms = self.request_timeout_ms;
        let worker_handle = handle.clone();
        let worker = thread::spawn(move || {
            provider_account_worker(
                store,
                factory,
                worker_handle,
                receiver,
                shutdown,
                unknown_threads,
                request_timeout_ms,
            );
        });
        sessions.insert(
            handle.clone(),
            ManagedSession {
                sender: sender.clone(),
                worker,
            },
        );
        Ok(sender)
    }

    fn shutdown(&self) -> Result<(), TurnRuntimeError> {
        let sessions = std::mem::take(
            &mut *self
                .sessions
                .lock()
                .map_err(|_| TurnRuntimeError::WorkerPanicked)?,
        );
        for session in sessions.values() {
            let _ignored = session.sender.send(SessionCommand::Shutdown);
        }
        for (_, session) in sessions {
            session
                .worker
                .join()
                .map_err(|_| TurnRuntimeError::WorkerPanicked)?;
        }
        Ok(())
    }
}

#[derive(Clone)]
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
    commands: &Receiver<SessionCommand>,
    shutdown: Arc<AtomicBool>,
    unknown_threads: Arc<AtomicU64>,
    request_timeout_ms: u64,
    event_state: &mut SessionEventState,
) -> bool {
    event_state.resumed_threads.clear();
    loop {
        loop {
            match commands.try_recv() {
                Ok(SessionCommand::StartTurn(intent, reply)) => {
                    match attempt_can_prepare(&store, &intent) {
                        Ok(true) => {}
                        Ok(false) => {
                            let _ignored = reply.send(TurnStart::NotSubmitted);
                            continue;
                        }
                        Err(_) => {
                            let _ignored = reply.send(TurnStart::Blocked {
                                reason: "scheduler state could not be read; input remains pending."
                                    .into(),
                            });
                            continue;
                        }
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
                    notify_pending_unknown(&mut event_state.pending);
                    let _ignored = session.close();
                    return false;
                }
                Err(TryRecvError::Disconnected) => {
                    notify_pending_unknown(&mut event_state.pending);
                    let _ignored = session.close();
                    return false;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        if shutdown.load(Ordering::Acquire) {
            notify_pending_unknown(&mut event_state.pending);
            let _ignored = session.close();
            return false;
        }
        match session.receive_event_timeout(SESSION_POLL) {
            Ok(Some((event, cursor))) => {
                let mut retry_delay = Duration::from_millis(100);
                loop {
                    match handle_provider_event(
                        &store,
                        &handle,
                        session.generation(),
                        &mut session,
                        event.clone(),
                        event_state,
                        unknown_threads.as_ref(),
                        request_timeout_ms,
                    ) {
                        Ok(()) => break,
                        Err(error) => {
                            eprintln!("provider event settlement failed; retaining frame: {error}");
                            if shutdown.load(Ordering::Acquire) {
                                let _ignored = session.close();
                                return false;
                            }
                            thread::sleep(retry_delay);
                            retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(5));
                        }
                    }
                }
                if let Some(cursor) = cursor {
                    if session.acknowledge_handled(cursor).is_err() {
                        let _ignored = session.close();
                        return true;
                    }
                }
            }
            Ok(None) => {}
            Err(ProviderSessionError::Closed) => {
                notify_pending_unknown(&mut event_state.pending);
                let _ignored = session.close();
                return true;
            }
            Err(_) => {
                notify_pending_unknown(&mut event_state.pending);
                let _ignored = session.close();
                return true;
            }
        }
    }
}

fn provider_account_worker(
    store: Arc<RuntimeStore>,
    factory: Arc<dyn ProviderProcessFactory>,
    handle: ProviderHandle,
    commands: Receiver<SessionCommand>,
    shutdown: Arc<AtomicBool>,
    unknown_threads: Arc<AtomicU64>,
    request_timeout_ms: u64,
) {
    let mut event_state = SessionEventState::default();
    loop {
        if shutdown.load(Ordering::Acquire) {
            notify_pending_unknown(&mut event_state.pending);
            return;
        }
        let connected = match factory.start_or_reattach(&handle) {
            Ok(mut process) => {
                let generation = process.generation();
                let recovered = store.read().and_then(|connection| {
                    SchedulerRepository::submitted_start_requests(
                        &connection,
                        handle.account_key(),
                        generation,
                    )
                });
                match recovered {
                    Ok(requests) => {
                        event_state.pending.clear();
                        let mut tickets = Vec::with_capacity(requests.len());
                        for (agent_id, epoch, attempt_id, request_id, thread_id) in requests {
                            let request = RequestId::String(request_id);
                            tickets.push((
                                request.clone(),
                                Method::TurnStart,
                                Some(thread_id.clone()),
                            ));
                            event_state.pending.insert(
                                request,
                                PendingStart {
                                    intent: TurnIntent {
                                        agent_id,
                                        epoch,
                                        attempt_id,
                                        account_key: handle.account_key().to_owned(),
                                        provider: "codex".to_owned(),
                                        thread_id: Some(thread_id),
                                        cwd: None,
                                        model: None,
                                        events: Vec::new(),
                                    },
                                    reply: None,
                                    stage: PendingStartStage::Turn,
                                },
                            );
                        }
                        ProviderSession::connect_recovering(
                            process,
                            ProviderSessionConfig::new(),
                            tickets,
                        )
                    }
                    Err(_) => {
                        let _ignored = process.stop();
                        Err(ProviderSessionError::Closed)
                    }
                }
            }
            Err(_) => Err(ProviderSessionError::Closed),
        };
        match connected {
            Ok(session) => {
                if !provider_session_loop(
                    session,
                    Arc::clone(&store),
                    handle.clone(),
                    &commands,
                    Arc::clone(&shutdown),
                    Arc::clone(&unknown_threads),
                    request_timeout_ms,
                    &mut event_state,
                ) {
                    return;
                }
            }
            Err(_) => match commands.recv_timeout(SCHEDULER_POLL) {
                Ok(SessionCommand::StartTurn(_, reply)) => {
                    let _ignored = reply.send(TurnStart::NotSubmitted);
                }
                Ok(SessionCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    notify_pending_unknown(&mut event_state.pending);
                    return;
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            },
        }
    }
}

fn submit_turn_start(
    session: &mut ProviderSession,
    store: &RuntimeStore,
    pending: &mut HashMap<RequestId, PendingStart>,
    intent: TurnIntent,
    reply: Sender<TurnStart>,
    request_timeout_ms: u64,
) {
    let mut params = match provider_turn_params(store, &intent) {
        Ok(params) => params,
        Err(reason) => {
            let _ignored = reply.send(TurnStart::Blocked { reason });
            return;
        }
    };
    match attempt_can_prepare(store, &intent) {
        Ok(true) => {}
        Ok(false) => {
            let _ignored = reply.send(TurnStart::NotSubmitted);
            return;
        }
        Err(_) => {
            let _ignored = reply.send(TurnStart::NotSubmitted);
            return;
        }
    }
    let request_id = format!("studio-turn-start:{}:{}", intent.attempt_id, intent.epoch);
    match mark_submitted_for_provider(store, &intent, &request_id, session.generation()) {
        Ok(true) => {}
        Ok(false) | Err(_) => {
            let _ignored = reply.send(TurnStart::NotSubmitted);
            return;
        }
    }
    params["clientUserMessageId"] = intent
        .events
        .first()
        .map_or(Value::Null, |event| Value::String(event.id.clone()));
    match session.send_request_with_id(
        RequestId::String(request_id.clone()),
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
    generation: u64,
    session: &mut ProviderSession,
    event: ProviderSessionEvent,
    state: &mut SessionEventState,
    unknown_threads: &AtomicU64,
    request_timeout_ms: u64,
) -> Result<(), TurnRuntimeError> {
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
                return Ok(());
            };
            if start.stage != PendingStartStage::Resume {
                return Ok(());
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
                return Ok(());
            }
            let Some(thread_id) = start.intent.thread_id.clone() else {
                if let Some(reply) = start.reply.take() {
                    let _ignored = reply.send(TurnStart::Blocked {
                        reason: "Provider thread resume returned no thread identity.".to_owned(),
                    });
                }
                return Ok(());
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
            let Some(mut start) = pending.get(&ticket.id).cloned() else {
                return Ok(());
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
                        if let Some(start) = pending.get_mut(&ticket.id) {
                            start.reply.take();
                        }
                        return Ok(());
                    };
                    let request_id = request_id_string(&ticket.id);
                    let identity = identity_for(&start.intent);
                    let settled = settle_started_provider(
                        store,
                        &start.intent,
                        &request_id,
                        ticket.generation,
                        &turn_id,
                    )?;
                    pending.remove(&ticket.id);
                    if !settled {
                        if let Some(reply) = start.reply.take() {
                            let _ignored = reply.send(TurnStart::Unknown);
                        }
                        return Ok(());
                    }
                    if let Some(reply) = start.reply.take() {
                        let _ignored = reply.send(TurnStart::Accepted {
                            turn_id: turn_id.clone(),
                        });
                    }
                    if !terminal_turn_matches(store, &identity, &turn_id)
                        && let Some(thread_id) = start.intent.thread_id.clone()
                    {
                        active.insert((thread_id, turn_id.clone()), identity.clone());
                    }
                    if was_stopped_after_submit(store, &identity) {
                        let id = RequestId::String(format!(
                            "studio-turn-interrupt:{}:{}",
                            start.intent.attempt_id, turn_id
                        ));
                        let _ignored = session.send_request_with_id(
                            id,
                            Method::TurnInterrupt,
                            json!({"threadId":start.intent.thread_id,"turnId":turn_id}),
                            start.intent.thread_id.clone(),
                            false,
                            request_timeout_ms,
                        );
                    }
                }
                ResponseBody::Error(error) => {
                    let error = json!({"message":provider_error_message(&error)});
                    let request_id = request_id_string(&ticket.id);
                    let settled = settle_start_rejected_provider(
                        store,
                        &start.intent,
                        &request_id,
                        ticket.generation,
                        error.clone(),
                    )?;
                    pending.remove(&ticket.id);
                    if settled && let Some(reply) = start.reply.take() {
                        let _ignored = reply.send(TurnStart::Rejected { error });
                    } else if let Some(reply) = start.reply.take() {
                        let _ignored = reply.send(TurnStart::Unknown);
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
                    generation,
                    notification.method,
                    notification.params.unwrap_or(Value::Null),
                    active,
                    unknown_threads,
                )?;
            }
        }
        ProviderSessionEvent::Closed { .. } => {}
        ProviderSessionEvent::Inbound(Inbound::Request(request)) => {
            let _ignored = session.reject_request(
                request.id,
                "Provider callback is not supported by the Rust turn driver.",
            );
        }
        ProviderSessionEvent::Reply { .. } | ProviderSessionEvent::UnknownOutcome { .. } => {}
    }
    Ok(())
}

fn route_notification(
    store: &RuntimeStore,
    handle: &ProviderHandle,
    generation: u64,
    method: Method,
    params: Value,
    active: &mut HashMap<(String, String), TurnAttemptIdentity>,
    unknown_threads: &AtomicU64,
) -> Result<(), TurnRuntimeError> {
    if !matches!(method, Method::TurnStarted | Method::TurnCompleted) {
        return Ok(());
    }
    let Some(thread_id) = params.get("threadId").and_then(Value::as_str) else {
        return Ok(());
    };
    let Some(turn) = params.get("turn") else {
        return Ok(());
    };
    let Some(turn_id) = turn
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
    else {
        return Ok(());
    };
    let (thread_known, identity) = store.read().and_then(|connection| {
        SchedulerRepository::find_attempt_for_thread(
            &connection,
            handle.account_key(),
            generation,
            thread_id,
            Some(turn_id),
        )
    })?;
    if !thread_known {
        unknown_threads.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    let Some(identity) = identity else {
        return Ok(());
    };
    if method == Method::TurnStarted {
        if settle_started_attempt(store, &identity, turn_id)? {
            active.insert((thread_id.to_owned(), turn_id.to_owned()), identity);
        }
        return Ok(());
    }
    let outcome = match turn.get("status").and_then(Value::as_str) {
        Some("completed") => TurnOutcome::Completed,
        Some("failed") => TurnOutcome::Failed {
            error: turn.get("error").cloned(),
        },
        Some("interrupted") => TurnOutcome::Interrupted {
            error: turn.get("error").cloned(),
        },
        _ => return Ok(()),
    };
    if method == Method::TurnCompleted && matches!(&outcome, TurnOutcome::Completed) {
        // The current Rust slice has no durable transcript/context projection or
        // answer-item writer. Do not claim completion without those records.
        let outcome = TurnOutcome::Failed {
            error: Some(
                json!({"message":"Rust turn history persistence is unavailable: submitted user transcript/context projection and provider answer items are not stored."}),
            ),
        };
        if settle_terminal_attempt(store, &identity, turn_id, outcome)? {
            active.remove(&(thread_id.to_owned(), turn_id.to_owned()));
        }
        return Ok(());
    }
    if settle_terminal_attempt(store, &identity, turn_id, outcome)? {
        active.remove(&(thread_id.to_owned(), turn_id.to_owned()));
    }
    Ok(())
}

fn request_id_string(id: &RequestId) -> String {
    match id {
        RequestId::Number(id) => id.to_string(),
        RequestId::String(id) => id.clone(),
    }
}

fn provider_error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| error.to_string())
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

fn was_stopped_after_submit(store: &RuntimeStore, identity: &TurnAttemptIdentity) -> bool {
    store
        .read()
        .ok()
        .and_then(|connection| {
            AgentRepository::get(&connection, &AgentId::from(identity.agent_id.as_str()))
                .ok()
                .flatten()
        })
        .is_some_and(|agent| {
            agent.epoch != identity.epoch
                && agent.status == studio_runtime_store::AgentStatus::Paused
                && agent.extra.get("autoWake").and_then(Value::as_bool) == Some(false)
                && agent.extra.get("turnEpoch").and_then(Value::as_i64) == Some(identity.epoch)
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

fn provider_turn_params(store: &RuntimeStore, intent: &TurnIntent) -> Result<Value, String> {
    let agent = store
        .read()
        .and_then(|connection| {
            AgentRepository::get(&connection, &AgentId::from(intent.agent_id.as_str()))
        })
        .map_err(|_| "scheduler state could not be read; input remains pending.".to_owned())?
        .ok_or_else(|| "agent settings are unavailable; input remains pending.".to_owned())?;
    if agent.extra.get("pendingSettings").is_some_and(|value| {
        !value.is_null() && value.as_object().is_none_or(|object| !object.is_empty())
    }) {
        return Err(
            "pendingSettings are not supported by the Rust turn driver; apply settings first."
                .into(),
        );
    }
    if agent.extra.get("workspaceReady").and_then(Value::as_bool) == Some(false)
        || (agent.extra.get("imageWorkspace").and_then(Value::as_bool) == Some(true)
            && agent
                .extra
                .get("imageWorkspaceReady")
                .and_then(Value::as_bool)
                != Some(true))
        || (agent.extra.get("executionMode").and_then(Value::as_str) == Some("vm")
            && agent.extra.get("layrReady").and_then(Value::as_bool) == Some(false))
    {
        return Err(
            "workspace is not ready; prepare the workspace before submitting input.".into(),
        );
    }
    let mut params = turn_start_params(intent);
    if let Some(effort) = agent
        .extra
        .get("nativeEffort")
        .or_else(|| agent.extra.get("effort"))
        .filter(|value| !value.is_null())
    {
        params["effort"] = effort.clone();
    }
    params["serviceTier"] = Value::String(
        if agent.extra.get("fastMode").and_then(Value::as_bool) == Some(true) {
            "priority"
        } else {
            "default"
        }
        .into(),
    );
    let program = if agent.extra.get("daybreakEnabled").and_then(Value::as_bool) == Some(true) {
        agent
            .extra
            .get("cyberAccessProgram")
            .and_then(Value::as_str)
            .filter(|value| matches!(*value, "daybreakBlue" | "daybreakRed"))
            .ok_or_else(|| {
                "selected Daybreak access program is unavailable; input remains pending.".to_owned()
            })?
    } else {
        "standard"
    };
    params["cyberAccessProgram"] = Value::String(program.to_owned());
    if let Some(yolo) = agent.extra.get("yoloMode") {
        let Some(yolo) = yolo.as_bool() else {
            return Err(
                "permission settings cannot be applied because yoloMode is invalid.".into(),
            );
        };
        params["approvalPolicy"] = Value::String(if yolo { "never" } else { "on-request" }.into());
        if yolo {
            params["sandboxPolicy"] = json!({"type":"dangerFullAccess"});
        } else if agent.extra.get("role").and_then(Value::as_str) == Some("reviewer") {
            params["sandboxPolicy"] = json!({"type":"readOnly"});
        } else {
            let Some(cwd) = intent.cwd.as_deref().filter(|cwd| !cwd.is_empty()) else {
                return Err(
                    "workspace path is missing; permission settings cannot be applied.".into(),
                );
            };
            params["sandboxPolicy"] =
                json!({"type":"workspaceWrite","writableRoots":[cwd],"networkAccess":false});
        }
    }
    Ok(params)
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
        initializes: AtomicUsize,
    }

    impl FakeSignals {
        fn wait_for_starts(&self, expected: usize) -> bool {
            let Ok(stops) = self.stops.lock() else {
                return false;
            };
            self.changed
                .wait_timeout_while(stops, Duration::from_secs(20), |_| {
                    self.starts.load(Ordering::Relaxed) < expected
                })
                .is_ok_and(|(_, timeout)| {
                    self.starts.load(Ordering::Relaxed) >= expected && !timeout.timed_out()
                })
        }

        fn wait_for_initializes(&self, expected: usize) -> bool {
            let Ok(stops) = self.stops.lock() else {
                return false;
            };
            self.changed
                .wait_timeout_while(stops, Duration::from_secs(20), |_| {
                    self.initializes.load(Ordering::Relaxed) < expected
                })
                .is_ok_and(|(_, timeout)| {
                    self.initializes.load(Ordering::Relaxed) >= expected && !timeout.timed_out()
                })
        }

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
                initializes: AtomicUsize::new(0),
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
                return Err(ProviderFactoryError::Launch(
                    "invalid account handle".into(),
                ));
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
                .map_err(|error| ProviderFactoryError::Launch(error.to_string()))?;
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
        fn split(&mut self) -> io::Result<(Box<dyn ProviderProcessReader>, Box<dyn Write + Send>)> {
            let stdout = self
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("fake provider stdout already taken"))?;
            let stdin = self
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("fake provider stdin already taken"))?;
            Ok((Box::new(CursorReader(stdout)), Box::new(stdin)))
        }

        fn acknowledged_cursor(&self) -> u64 {
            0
        }
        fn generation(&self) -> u64 {
            1
        }
        fn acknowledge(&mut self, _cursor: u64) -> io::Result<()> {
            Ok(())
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
                return Err(ProviderFactoryError::Launch(
                    "invalid account handle".into(),
                ));
            }
            if let Ok(_stops) = self.0.signals.stops.lock() {
                self.0.signals.starts.fetch_add(1, Ordering::Relaxed);
                self.0.signals.changed.notify_all();
            }
            let (output, input) = mpsc::channel::<Option<Vec<u8>>>();
            *self
                .0
                .output
                .lock()
                .map_err(|_| ProviderFactoryError::Launch("fake output mutex poisoned".into()))? =
                Some(output.clone());
            Ok(Box::new(ControlledProcess {
                provider: Arc::clone(&self.0),
                input: Some(input),
                output,
            }))
        }
    }

    struct ReturningUnknown<'a>(&'a RuntimeStore);

    impl TurnDriver for ReturningUnknown<'_> {
        fn start(&mut self, intent: TurnIntent) -> TurnStart {
            mark_submitted_for_provider(self.0, &intent, "restart-ticket", 1)
                .expect("fake provider start identity is persisted");
            TurnStart::Unknown
        }
    }

    struct ControlledProcess {
        provider: Arc<ControlledProvider>,
        input: Option<Receiver<Option<Vec<u8>>>>,
        output: Sender<Option<Vec<u8>>>,
    }

    impl ProviderProcess for ControlledProcess {
        fn split(&mut self) -> io::Result<(Box<dyn ProviderProcessReader>, Box<dyn Write + Send>)> {
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

        fn acknowledged_cursor(&self) -> u64 {
            0
        }
        fn generation(&self) -> u64 {
            1
        }
        fn acknowledge(&mut self, _cursor: u64) -> io::Result<()> {
            Ok(())
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

    struct CursorReader<R>(R);

    impl<R: Read + Send> Read for CursorReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.0.read(buffer)
        }
    }

    impl<R: Read + Send> ProviderProcessReader for CursorReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.0.read(buffer)
        }
        fn take_cursors(&mut self) -> Vec<u64> {
            Vec::new()
        }
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

    impl ProviderProcessReader for ControlledReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            <Self as Read>::read(self, buffer)
        }
        fn take_cursors(&mut self) -> Vec<u64> {
            Vec::new()
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
                        if let Ok(_stops) = self.provider.signals.stops.lock() {
                            self.provider
                                .signals
                                .initializes
                                .fetch_add(1, Ordering::Relaxed);
                            self.provider.signals.changed.notify_all();
                        }
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
        assert_eq!(agent.status, AgentStatus::Failed);
        assert_eq!(agent.extra["lastCompletedTurnStatus"], "failed");
        assert!(
            agent.extra["lastCompletedTurnError"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("transcript/context projection"))
        );
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
        assert_eq!(agent.status, AgentStatus::Running);
        assert_eq!(agent.extra["inFlight"], true);
        assert!(agent.extra.get("lastCompletedTurnStatus").is_none());
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
        assert_eq!(agent.status, AgentStatus::Running);
        assert_eq!(agent.extra["inFlight"], true);
        assert!(agent.extra.get("lastCompletedTurnStatus").is_none());
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
            initializes: AtomicUsize::new(0),
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
            assert_eq!(agent.status, AgentStatus::Failed);
            assert_eq!(agent.extra["lastCompletedTurnStatus"], "failed");
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
            initializes: AtomicUsize::new(0),
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
            assert_eq!(agent.status, AgentStatus::Failed);
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
            &mut ReturningUnknown(&store),
        )
        .expect("submitted turn is recorded as unknown");
        drop(store);

        let store =
            Arc::new(RuntimeStore::open(Some(&state.0)).expect("Rust reopens fixture state"));
        let signals = Arc::new(FakeSignals {
            stops: Mutex::new(0),
            changed: Condvar::new(),
            starts: AtomicUsize::new(0),
            initializes: AtomicUsize::new(0),
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
        assert!(signals.wait_for_starts(1), "recovery account attaches");
        assert!(
            signals.wait_for_initializes(1),
            "recovered session initializes"
        );
        assert!(
            provider
                .requests
                .lock()
                .expect("fake requests are available")
                .is_empty()
        );

        provider
            .send_frame(json!({"id":"restart-ticket","result":{"turn":{"id":"recovered-turn"}}}));
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
        assert_eq!(agent.status, AgentStatus::Failed);
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
