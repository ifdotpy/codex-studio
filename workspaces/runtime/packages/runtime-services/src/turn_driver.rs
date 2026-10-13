//! One synchronous account worker owns claims, provider input and durable settlement.

use crate::provider_session::{
    ProviderSession, ProviderSessionConfig, ProviderSessionError, ProviderSessionEvent,
    ProviderSessionInput,
};
use crate::scheduler::{
    SchedulerError, TurnIntent, TurnStart, claim_intent, mark_submitted_for_provider,
    recover_on_startup, settle_start_outcome, settle_start_rejected_provider,
    settle_started_provider, settle_terminal_attempt,
};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use studio_runtime_process::{ProviderHandle, ProviderProcessFactory};
use studio_runtime_providers::{Frame, Inbound, Method, RequestId, RequestTicket, ResponseBody};
use studio_runtime_store::{
    AgentId, AgentRepository, RuntimeStore, SchedulerRepository, StoreError,
};
use thiserror::Error;

const DEFAULT_REQUEST_TIMEOUT_MS: u64 = 60_000;
const SETTLEMENT_RETRIES: u8 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(1);
const SETTLEMENT_HELD: &str = "Provider settlement failed repeatedly; account held with its cursor retained. Stop or restart to retry settlement.";

#[derive(Debug, Error)]
pub enum TurnRuntimeError {
    #[error(transparent)]
    Scheduler(#[from] SchedulerError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Session(#[from] ProviderSessionError),
    #[error("turn runtime worker panicked during shutdown")]
    WorkerPanicked,
}

#[derive(Clone)]
pub struct TurnWakeHandle(Weak<SessionRegistry>);
impl TurnWakeHandle {
    pub fn wake(&self) {
        if let Some(registry) = self.0.upgrade() {
            let _ignored = registry.schedule();
        }
    }
}

pub struct TurnRuntime {
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
        let accounts = SchedulerRepository::accounts_with_unknown_submitted_turns(&store.read()?)?;
        let sessions = Arc::new(SessionRegistry {
            store: Arc::clone(&store),
            factory,
            shutdown: AtomicBool::new(false),
            sessions: Mutex::new(HashMap::new()),
            unknown_threads: Arc::new(AtomicU64::new(0)),
            request_timeout_ms,
        });
        for account in accounts {
            sessions.sender(&account)?;
        }
        let runtime = Self { sessions };
        runtime.wake_handle().wake();
        Ok(runtime)
    }
    #[must_use]
    pub fn wake_handle(&self) -> TurnWakeHandle {
        TurnWakeHandle(Arc::downgrade(&self.sessions))
    }
    #[must_use]
    pub fn unknown_thread_notifications(&self) -> u64 {
        self.sessions.unknown_threads.load(Ordering::Relaxed)
    }
    /// Called after the stop transaction has revoked the epoch. The worker owns interruption.
    pub fn stopped(&self, account: &str, agent: &str, epoch: i64) -> Result<(), TurnRuntimeError> {
        self.sessions
            .sender(account)?
            .send(AccountInput::Stop(agent.to_owned(), epoch))
            .map_err(|_| TurnRuntimeError::WorkerPanicked)
    }
    /// Resume retained settlement, never repeat a provider start.
    pub fn restart_account(&self, account: &str) -> Result<(), TurnRuntimeError> {
        self.sessions
            .sender(account)?
            .send(AccountInput::Restart)
            .map_err(|_| TurnRuntimeError::WorkerPanicked)
    }
    pub fn shutdown(&mut self) -> Result<(), TurnRuntimeError> {
        self.sessions.shutdown()
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
    shutdown: AtomicBool,
    sessions: Mutex<HashMap<String, ManagedSession>>,
    unknown_threads: Arc<AtomicU64>,
    request_timeout_ms: u64,
}
struct ManagedSession {
    sender: Sender<AccountInput>,
    worker: Option<JoinHandle<Result<(), TurnRuntimeError>>>,
}
/// Reader and writer callbacks use this same channel as scheduler and lifecycle commands.
enum AccountInput {
    Wake(String, i64),
    Transport(ProviderSessionInput),
    Deadline,
    Stop(String, i64),
    Restart,
    Shutdown,
}
impl SessionRegistry {
    fn sender(self: &Arc<Self>, account: &str) -> Result<Sender<AccountInput>, TurnRuntimeError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| TurnRuntimeError::WorkerPanicked)?;
        if let Some(entry) = sessions.get(account) {
            return Ok(entry.sender.clone());
        }
        if self.shutdown.load(Ordering::Acquire) {
            return Err(TurnRuntimeError::WorkerPanicked);
        }
        let (sender, receiver) = mpsc::channel();
        // Install the only entry before starting connection I/O.
        sessions.insert(
            account.to_owned(),
            ManagedSession {
                sender: sender.clone(),
                worker: None,
            },
        );
        let mut worker = AccountWorker::new(
            Arc::clone(&self.store),
            Arc::clone(&self.factory),
            account.to_owned(),
            sender.clone(),
            TurnWakeHandle(Arc::downgrade(self)),
            Arc::clone(&self.unknown_threads),
            self.request_timeout_ms,
        );
        let join = thread::spawn(move || worker.run(receiver));
        if let Some(entry) = sessions.get_mut(account) {
            entry.worker = Some(join);
        }
        Ok(sender)
    }
    fn schedule(self: &Arc<Self>) -> Result<(), TurnRuntimeError> {
        if self.shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        let reader = self.store.read()?;
        for (agent, epoch) in SchedulerRepository::pending_agents(&reader)? {
            if let Some(record) = AgentRepository::get(&reader, &AgentId::from(agent.as_str()))? {
                let account = record
                    .extra
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default");
                self.sender(account)?
                    .send(AccountInput::Wake(agent, epoch))
                    .map_err(|_| TurnRuntimeError::WorkerPanicked)?;
            }
        }
        Ok(())
    }
    fn shutdown(&self) -> Result<(), TurnRuntimeError> {
        self.shutdown.store(true, Ordering::Release);
        let sessions = std::mem::take(
            &mut *self
                .sessions
                .lock()
                .map_err(|_| TurnRuntimeError::WorkerPanicked)?,
        );
        for entry in sessions.values() {
            let _ignored = entry.sender.send(AccountInput::Shutdown);
        }
        let mut result = Ok(());
        for mut entry in sessions.into_values() {
            if let Some(worker) = entry.worker.take() {
                let outcome = worker
                    .join()
                    .map_err(|_| TurnRuntimeError::WorkerPanicked)
                    .and_then(|r| r);
                if result.is_ok() {
                    result = outcome;
                }
            }
        }
        result
    }
}

#[derive(Clone)]
struct Pending {
    intent: TurnIntent,
    method: Method,
}
#[derive(Clone)]
struct Settlement {
    event: ProviderSessionEvent,
    cursor: Option<u64>,
    failures: u8,
}
impl From<(ProviderSessionEvent, Option<u64>)> for Settlement {
    fn from((event, cursor): (ProviderSessionEvent, Option<u64>)) -> Self {
        Self {
            event,
            cursor,
            failures: 0,
        }
    }
}
#[derive(Clone, Copy, PartialEq)]
enum Closing {
    Reconnect,
    Shutdown,
}
struct AccountWorker {
    store: Arc<RuntimeStore>,
    factory: Arc<dyn ProviderProcessFactory>,
    account: String,
    sender: Sender<AccountInput>,
    wake: TurnWakeHandle,
    unknown_threads: Arc<AtomicU64>,
    timeout_ms: u64,
    session: Option<ProviderSession>,
    pending: HashMap<RequestId, Pending>,
    resumed: HashSet<String>,
    wakes: VecDeque<(String, i64)>,
    settlements: VecDeque<Settlement>,
    deferred: VecDeque<Settlement>,
    retry_at: Option<Instant>,
    held: bool,
    closing: Option<Closing>,
    quiesced: bool,
    attachment_ready: bool,
    serial: u64,
}
impl AccountWorker {
    #[allow(clippy::too_many_arguments)]
    fn new(
        store: Arc<RuntimeStore>,
        factory: Arc<dyn ProviderProcessFactory>,
        account: String,
        sender: Sender<AccountInput>,
        wake: TurnWakeHandle,
        unknown_threads: Arc<AtomicU64>,
        timeout_ms: u64,
    ) -> Self {
        Self {
            store,
            factory,
            account,
            sender,
            wake,
            unknown_threads,
            timeout_ms,
            session: None,
            pending: HashMap::new(),
            resumed: HashSet::new(),
            wakes: VecDeque::new(),
            settlements: VecDeque::new(),
            deferred: VecDeque::new(),
            retry_at: None,
            held: false,
            closing: None,
            quiesced: false,
            attachment_ready: false,
            serial: 0,
        }
    }
    fn run(&mut self, receiver: Receiver<AccountInput>) -> Result<(), TurnRuntimeError> {
        if self.connect().is_err() {
            self.hold(None);
        }
        loop {
            let timeout = [
                self.retry_at
                    .map(|at| at.saturating_duration_since(Instant::now())),
                self.session
                    .as_ref()
                    .and_then(ProviderSession::next_deadline),
            ]
            .into_iter()
            .flatten()
            .min();
            let input = match timeout {
                Some(timeout) => match receiver.recv_timeout(timeout) {
                    Ok(input) => input,
                    Err(mpsc::RecvTimeoutError::Timeout) => AccountInput::Deadline,
                    Err(mpsc::RecvTimeoutError::Disconnected) => AccountInput::Shutdown,
                },
                None => receiver.recv().unwrap_or(AccountInput::Shutdown),
            };
            match self.input(input) {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(error) => {
                    self.hold(self.settlements.front().and_then(|event| event.cursor));
                    if self.closing == Some(Closing::Shutdown) {
                        if let Some(session) = self.session.take() {
                            let _ignored = session.close();
                        }
                        return Err(error);
                    }
                }
            }
        }
    }
    fn connect(&mut self) -> Result<(), TurnRuntimeError> {
        let mut process = self
            .factory
            .start_or_reattach(&ProviderHandle::for_account(&self.account))
            .map_err(|_| {
                StoreError::SchemaMismatch("provider process attachment unavailable".into())
            })?;
        let generation = process.generation();
        let recovered = self.store.transaction(|tx| {
            SchedulerRepository::reconcile_generation(tx, &self.account, generation, now())?;
            SchedulerRepository::submitted_start_requests(tx, &self.account, generation)
        });
        let recovered = match recovered {
            Ok(value) => value,
            Err(error) => {
                let _ignored = process.stop();
                return Err(error.into());
            }
        };
        self.attachment_ready = false;
        self.pending.clear();
        self.resumed.clear();
        let mut tickets = Vec::new();
        for (agent_id, epoch, attempt_id, request, thread_id) in recovered {
            let id = RequestId::String(request);
            tickets.push((id.clone(), Method::TurnStart, Some(thread_id.clone())));
            self.pending.insert(
                id,
                Pending {
                    method: Method::TurnStart,
                    intent: TurnIntent {
                        agent_id,
                        epoch,
                        attempt_id,
                        account_key: self.account.clone(),
                        provider: "codex".into(),
                        thread_id: Some(thread_id),
                        cwd: None,
                        model: None,
                        events: Vec::new(),
                    },
                },
            );
        }
        let sender = self.sender.clone();
        let mut config = ProviderSessionConfig::new();
        config.request_timeout_ms = self.timeout_ms;
        self.session = Some(ProviderSession::begin_connect_recovering(
            process,
            config,
            tickets,
            Arc::new(move |input| {
                let _ignored = sender.send(AccountInput::Transport(input));
            }),
        )?);
        self.collect_events();
        Ok(())
    }
    fn collect_events(&mut self) {
        if let Some(session) = &mut self.session {
            self.settlements
                .extend(session.take_events().into_iter().map(Settlement::from));
        }
    }
    fn input(&mut self, input: AccountInput) -> Result<bool, TurnRuntimeError> {
        match input {
            AccountInput::Wake(agent, epoch) if !self.held && self.closing.is_none() => {
                if !self.wakes.contains(&(agent.clone(), epoch)) {
                    self.wakes.push_back((agent, epoch));
                }
                if self.session.is_none() {
                    self.connect()?;
                }
            }
            AccountInput::Wake(..) => {}
            AccountInput::Transport(ProviderSessionInput::Quiesced) => {
                self.quiesced = true;
            }
            AccountInput::Transport(input) => {
                if let Some(session) = &mut self.session {
                    self.settlements.extend(
                        session
                            .handle_input(input)?
                            .into_iter()
                            .map(Settlement::from),
                    );
                    if !self.attachment_ready && session.is_ready() {
                        self.attachment_ready = true;
                        self.wake.wake();
                    }
                }
            }
            AccountInput::Deadline => {
                if self.retry_at.is_some_and(|at| at <= Instant::now()) {
                    self.retry_at = None;
                }
                if let Some(session) = &mut self.session {
                    self.settlements
                        .extend(session.handle_deadline().into_iter().map(Settlement::from));
                }
            }
            AccountInput::Stop(agent, epoch) => {
                self.clear_hold()?;
                let record =
                    AgentRepository::get(&self.store.read()?, &AgentId::from(agent.as_str()))?;
                if let Some(record) = record
                    && record.epoch == epoch
                    && record.status == studio_runtime_store::AgentStatus::Paused
                    && let Some(turn) = record.extra.get("turnId").and_then(Value::as_str)
                {
                    let thread = record
                        .extra
                        .get("threadId")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    self.interrupt(&agent, thread, turn)?;
                }
            }
            AccountInput::Restart => {
                self.clear_hold()?;
                self.wake.wake();
                if self.session.is_none() {
                    self.connect()?;
                }
            }
            AccountInput::Shutdown => {
                self.closing = Some(Closing::Shutdown);
                if let Some(session) = &mut self.session {
                    session.quiesce()?;
                } else {
                    return Ok(true);
                }
            }
        }
        self.drain_settlements()?;
        if !self.held
            && self.closing.is_none()
            && self.session.as_ref().is_some_and(ProviderSession::is_ready)
        {
            while let Some((agent, epoch)) = self.wakes.pop_front() {
                self.start(agent, epoch)?;
            }
        }
        self.collect_events();
        self.drain_settlements()?;
        if self.quiesced && (self.settlements.is_empty() || self.held) {
            if let Some(session) = &self.session {
                self.store.transaction(|tx| {
                    SchedulerRepository::disconnect_account(
                        tx,
                        &self.account,
                        session.generation(),
                        now(),
                        self.closing == Some(Closing::Shutdown),
                    )
                })?;
            }
            if let Some(session) = self.session.take() {
                session.close()?;
            }
            if self.closing == Some(Closing::Shutdown) {
                return Ok(true);
            }
            self.closing = None;
            self.quiesced = false;
            // Unacknowledged frames are replayed by the supervisor on reattach.
            self.settlements.clear();
            self.deferred.clear();
            if !self.held {
                self.connect()?;
            }
        }
        Ok(false)
    }

    fn start(&mut self, agent: String, epoch: i64) -> Result<(), TurnRuntimeError> {
        self.serial += 1;
        let attempt = format!(
            "rust-turn:{agent}:{epoch}:{}:{}",
            now().to_bits(),
            self.serial
        );
        let Some(intent) = claim_intent(&self.store, &agent, epoch, &attempt, now())? else {
            return Ok(());
        };
        if let Err(reason) = provider_turn_params(&self.store, &intent) {
            settle_start_outcome(&self.store, &intent, TurnStart::Blocked { reason })?;
            return Ok(());
        }
        if intent
            .thread_id
            .as_ref()
            .is_some_and(|t| self.resumed.contains(t))
        {
            return self.submit(intent);
        }
        let id = request_id(&intent, Method::ThreadResume);
        let params = json!({"threadId":intent.thread_id,"excludeTurns":true,"cwd":intent.cwd,"model":intent.model});
        self.send(
            Pending {
                intent,
                method: Method::ThreadResume,
            },
            id,
            params,
        )
    }
    fn submit(&mut self, intent: TurnIntent) -> Result<(), TurnRuntimeError> {
        let agent = AgentRepository::get(
            &self.store.read()?,
            &AgentId::from(intent.agent_id.as_str()),
        )?;
        if agent
            .as_ref()
            .and_then(|a| a.extra.get("startAttempt"))
            .is_none_or(|a| a["id"].as_str() != Some(&intent.attempt_id) || a["submitted"] == true)
        {
            return Ok(());
        }
        let params = match provider_turn_params(&self.store, &intent) {
            Ok(params) => params,
            Err(reason) => {
                settle_start_outcome(&self.store, &intent, TurnStart::Blocked { reason })?;
                return Ok(());
            }
        };
        let id = request_id(&intent, Method::TurnStart);
        let Some(session) = &self.session else {
            return Ok(());
        };
        // Both readiness and wire encoding are proven before the durable submitted boundary.
        if session
            .preflight_request_with_id(
                id.clone(),
                Method::TurnStart,
                params.clone(),
                intent.thread_id.clone(),
                false,
            )
            .is_err()
        {
            settle_start_outcome(
                &self.store,
                &intent,
                TurnStart::Blocked {
                    reason:
                        "Provider request is not ready or cannot be encoded; input remains pending."
                            .into(),
                },
            )?;
            return Ok(());
        }
        if !mark_submitted_for_provider(
            &self.store,
            &intent,
            &id_string(&id),
            session.generation(),
        )? {
            settle_start_outcome(&self.store, &intent, TurnStart::NotSubmitted)?;
            return Ok(());
        }
        self.send(
            Pending {
                intent,
                method: Method::TurnStart,
            },
            id,
            params,
        )
    }
    fn send(
        &mut self,
        pending: Pending,
        id: RequestId,
        params: Value,
    ) -> Result<(), TurnRuntimeError> {
        let Some(session) = &mut self.session else {
            return Ok(());
        };
        self.pending.insert(id.clone(), pending.clone());
        let result = session.send_request_with_id(
            id.clone(),
            pending.method,
            params,
            pending.intent.thread_id.clone(),
            false,
            self.timeout_ms,
        );
        if let Err(error) = result {
            let event = match error {
                ProviderSessionError::NotSent { ticket } => {
                    ProviderSessionEvent::NotSent { ticket }
                }
                _ => ProviderSessionEvent::UnknownOutcome {
                    ticket: RequestTicket {
                        id,
                        method: pending.method,
                        generation: session.generation(),
                    },
                },
            };
            self.settlements.push_back(Settlement {
                event,
                cursor: None,
                failures: 0,
            });
        }
        self.collect_events();
        Ok(())
    }
    fn drain_settlements(&mut self) -> Result<(), TurnRuntimeError> {
        while !self.held && self.retry_at.is_none() {
            let Some(mut settlement) = self.settlements.pop_front() else {
                break;
            };
            match self.settle(&settlement.event) {
                Ok(true) => {
                    if let Some(cursor) = settlement.cursor
                        && let Some(session) = &mut self.session
                        && session.acknowledge_handled(cursor).is_err()
                    {
                        self.begin_close(Closing::Reconnect)?;
                    }
                }
                Ok(false) => self.deferred.push_back(settlement),
                Err(_) => {
                    settlement.failures += 1;
                    let cursor = settlement.cursor;
                    let held = settlement.failures >= SETTLEMENT_RETRIES;
                    self.settlements.push_front(settlement);
                    if held {
                        self.hold(cursor);
                    } else {
                        self.retry_at = Some(Instant::now() + RETRY_DELAY);
                    }
                }
            }
        }
        Ok(())
    }
    /// False means completion preceded its correlated receipt; retain its cursor until binding.
    fn settle(&mut self, event: &ProviderSessionEvent) -> Result<bool, TurnRuntimeError> {
        match event {
            ProviderSessionEvent::Reply { ticket, body, .. }
                if ticket.method == Method::Initialize =>
            {
                if matches!(body, ResponseBody::Error(_)) {
                    self.hold(None);
                    self.begin_close(Closing::Reconnect)?;
                }
            }
            ProviderSessionEvent::Reply { ticket, body, .. }
                if ticket.method == Method::TurnInterrupt =>
            {
                if matches!(body, ResponseBody::Error(_)) {
                    self.hold(None);
                }
            }
            ProviderSessionEvent::Reply { ticket, body, .. } => {
                let Some(pending) = self.pending.get(&ticket.id).cloned() else {
                    return Ok(true);
                };
                let intent = &pending.intent;
                match pending.method {
                    Method::ThreadResume => {
                        if let ResponseBody::Result(result) = body
                            && result["thread"]["id"].as_str() == intent.thread_id.as_deref()
                        {
                            if let Some(thread) = &intent.thread_id {
                                self.resumed.insert(thread.clone());
                            }
                            self.submit(intent.clone())?;
                        } else {
                            settle_start_outcome(
                                &self.store,
                                intent,
                                TurnStart::Blocked {
                                    reason: "Provider thread resume failed; input remains pending."
                                        .into(),
                                },
                            )?;
                        }
                    }
                    Method::TurnStart => match body {
                        ResponseBody::Result(result) => {
                            if let Some(turn) =
                                result["turn"]["id"].as_str().filter(|id| !id.is_empty())
                            {
                                if let Some(interrupt) = settle_started_provider(
                                    &self.store,
                                    intent,
                                    &id_string(&ticket.id),
                                    ticket.generation,
                                    turn,
                                )? {
                                    if interrupt {
                                        self.interrupt(
                                            &intent.agent_id,
                                            intent.thread_id.as_deref().unwrap_or(""),
                                            turn,
                                        )?;
                                    }
                                    self.settlements.extend(self.deferred.drain(..));
                                }
                            } else {
                                settle_start_outcome(&self.store, intent, TurnStart::Unknown)?;
                            }
                        }
                        ResponseBody::Error(error) => {
                            settle_start_rejected_provider(
                                &self.store,
                                intent,
                                &id_string(&ticket.id),
                                ticket.generation,
                                json!({"message":error.get("message").and_then(Value::as_str).unwrap_or("Provider rejected turn/start")}),
                            )?;
                        }
                    },
                    _ => {}
                }
                self.pending.remove(&ticket.id);
            }
            ProviderSessionEvent::UnknownOutcome { ticket }
            | ProviderSessionEvent::NotSent { ticket } => {
                if let Some(pending) = self.pending.get(&ticket.id) {
                    let outcome = if matches!(event, ProviderSessionEvent::NotSent { .. }) {
                        TurnStart::NotSubmitted
                    } else if pending.method == Method::TurnStart {
                        TurnStart::Unknown
                    } else {
                        TurnStart::Blocked {
                            reason:
                                "Provider thread resume outcome is unknown; input remains pending."
                                    .into(),
                        }
                    };
                    settle_start_outcome(&self.store, &pending.intent, outcome)?;
                    if matches!(event, ProviderSessionEvent::NotSent { .. })
                        || pending.method == Method::ThreadResume
                    {
                        self.pending.remove(&ticket.id);
                    }
                } else if matches!(ticket.method, Method::TurnInterrupt | Method::Initialize) {
                    self.hold(None);
                    if ticket.method == Method::Initialize {
                        self.begin_close(Closing::Reconnect)?;
                    }
                }
            }
            ProviderSessionEvent::Inbound(Inbound::Notification(Frame::Notification(frame))) => {
                return self
                    .notification(frame.method, frame.params.as_ref().unwrap_or(&Value::Null));
            }
            ProviderSessionEvent::Inbound(Inbound::Request(request)) => {
                if let Some(session) = &mut self.session {
                    session.reject_request(
                        request.id.clone(),
                        "Provider callback is not supported by the Rust turn driver.",
                    )?;
                }
            }
            ProviderSessionEvent::Closed { .. } => {
                if !self.attachment_ready {
                    self.hold(None);
                }
                self.begin_close(Closing::Reconnect)?;
            }
            ProviderSessionEvent::OrphanReply { .. }
            | ProviderSessionEvent::WriteCompleted { .. }
            | ProviderSessionEvent::Inbound(_) => {}
        }
        Ok(true)
    }
    fn notification(&mut self, method: Method, params: &Value) -> Result<bool, TurnRuntimeError> {
        if !matches!(method, Method::TurnStarted | Method::TurnCompleted) {
            return Ok(true);
        }
        let (Some(thread), Some(turn)) =
            (params["threadId"].as_str(), params["turn"]["id"].as_str())
        else {
            return Ok(true);
        };
        let Some(session) = &self.session else {
            return Ok(false);
        };
        let (known, identity) = SchedulerRepository::find_attempt_for_thread(
            &self.store.read()?,
            &self.account,
            session.generation(),
            thread,
            Some(turn),
        )?;
        if !known {
            self.unknown_threads.fetch_add(1, Ordering::Relaxed);
        }
        let Some((identity, bound)) = identity else {
            return Ok(true);
        };
        if method == Method::TurnStarted {
            if self
                .store
                .transaction(|tx| SchedulerRepository::observe_started(tx, &identity, turn))?
                == Some(true)
            {
                self.interrupt(&identity.agent_id, thread, turn)?;
            }
            return Ok(true);
        }
        if !bound {
            return Ok(false);
        }
        settle_terminal_attempt(&self.store, &identity, params)?;
        self.wake.wake();
        Ok(true)
    }
    fn interrupt(&mut self, agent: &str, thread: &str, turn: &str) -> Result<(), TurnRuntimeError> {
        let Some(session) = &mut self.session else {
            self.hold(None);
            return Ok(());
        };
        // Replayed stopped notifications need no second interrupt on this attachment.
        let key = format!("interrupt:{agent}:{turn}");
        if self.resumed.contains(&key) {
            return Ok(());
        }
        match session.send_request(
            Method::TurnInterrupt,
            json!({"threadId":thread,"turnId":turn}),
            Some(thread.to_owned()),
            false,
            self.timeout_ms,
        ) {
            Ok(_) => {
                self.resumed.insert(key);
            }
            Err(_) => self.hold(None),
        }
        self.collect_events();
        Ok(())
    }
    fn begin_close(&mut self, closing: Closing) -> Result<(), TurnRuntimeError> {
        if self.closing.is_some() {
            return Ok(());
        }
        self.closing = Some(closing);
        if let Some(session) = &mut self.session {
            session.quiesce()?;
        }
        Ok(())
    }
    fn clear_hold(&mut self) -> Result<(), TurnRuntimeError> {
        self.store
            .transaction(|tx| SchedulerRepository::clear_account_hold(tx, &self.account))?;
        self.held = false;
        self.retry_at = None;
        for event in &mut self.settlements {
            event.failures = 0;
        }
        Ok(())
    }
    fn hold(&mut self, cursor: Option<u64>) {
        self.held = true;
        self.retry_at = None;
        // If the database itself is unavailable, retain the frame and expose failure on stderr;
        // never acknowledge or pretend the failure receipt was committed.
        if self
            .store
            .transaction(|tx| {
                SchedulerRepository::hold_account(tx, &self.account, cursor, SETTLEMENT_HELD)
            })
            .is_err()
        {
            eprintln!("{SETTLEMENT_HELD} Durable error receipt unavailable.");
        }
    }
}
fn id_string(id: &RequestId) -> String {
    match id {
        RequestId::Number(id) => id.to_string(),
        RequestId::String(id) => id.clone(),
    }
}
fn request_id(intent: &TurnIntent, method: Method) -> RequestId {
    RequestId::String(format!(
        "studio-{}:{}:{}:{}:{}",
        method.as_str(),
        intent.agent_id.len(),
        intent.agent_id,
        intent.epoch,
        intent.attempt_id
    ))
}
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
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
    if intent.provider != "codex" {
        return Err("This Rust turn driver currently supports the Codex provider only.".into());
    }
    if intent.thread_id.as_deref().is_none_or(str::is_empty)
        || intent.cwd.as_deref().is_none_or(str::is_empty)
        || intent.events.is_empty()
        || intent.events.iter().any(|event| {
            event
                .metadata
                .get("assets")
                .is_some_and(|v| v.as_array().is_none_or(|v| !v.is_empty()))
        })
    {
        return Err("This Rust turn driver requires an existing text-only provider thread.".into());
    }
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
    if agent.extra.get("executionMode").and_then(Value::as_str) == Some("vm")
        && agent.extra.get("role").and_then(Value::as_str) == Some("reviewer")
    {
        params["approvalPolicy"] = json!("never");
        params["sandboxPolicy"] = json!({"type":"readOnly"});
    } else if let Some(yolo) = agent.extra.get("yoloMode").filter(|value| !value.is_null()) {
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
            let mut roots = vec![cwd.to_owned()];
            if agent.extra.get("isLead").and_then(Value::as_bool) == Some(true) {
                let progress =
                    if agent.extra.get("executionMode").and_then(Value::as_str) == Some("vm") {
                        let home = agent
                            .extra
                            .get("layrHome")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                "VM lead progress path is unavailable; input remains pending."
                                    .to_owned()
                            })?;
                        std::path::PathBuf::from(home)
                            .join(".studio/progress")
                            .join(&intent.agent_id)
                    } else {
                        store.root().join("progress").join(&intent.agent_id)
                    };
                // Provisioning is owned by the workspace service; do not grant symlink access.
                if progress.is_dir()
                    && std::fs::canonicalize(&progress).ok().as_ref() == Some(&progress)
                {
                    roots.push(progress.to_string_lossy().into_owned());
                } else {
                    return Err("Lead progress directory requires workspace provisioning; input remains pending.".into());
                }
            }
            params["sandboxPolicy"] =
                json!({"type":"workspaceWrite","writableRoots":roots,"networkAccess":false});
        }
    }
    Ok(params)
}

#[cfg(test)]
mod tests;
