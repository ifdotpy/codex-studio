//! One Tokio task owns an account. SQLite work is serialized through StoreBridge;
//! dropping a handle signals shutdown and never aborts an in-flight commit.
use crate::{
    notification_dispatch::NotificationStreamBuffer,
    provider_session_task::{
        MAX_UNSETTLED, RecoveredRequest, RequestSpec, SessionControl, SessionEvent, SessionExit,
        SessionHandle, SessionOutcome, SessionTaskConfig, SessionTaskError, WriteOutcome,
    },
    scheduler::{self, TurnIntent, TurnStart},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use studio_runtime_process::{
    Generation, OperationId, ProviderHandle,
    provider_seam::{
        InitializationStatus, JournalEntry, JournalPayload, MAX_PENDING_RESPONSES, OpenOptions,
        ProviderTaskFactory, Shutdown,
    },
};
use studio_runtime_providers::{Frame, Method, RequestId, RequestOutcome, ResponseBody};
use studio_runtime_store::{
    AgentId, AgentRepository, RuntimeStore, SchedulerRepository, StoreError,
};
use tokio::{
    sync::{Semaphore, mpsc, watch},
    task::JoinHandle,
};
pub mod callback;
pub(crate) mod history;
pub mod preparation;
mod store;

impl From<rusqlite::Error> for WorkerError {
    fn from(error: rusqlite::Error) -> Self {
        StoreError::from(error).into()
    }
}

pub const COMMAND_CAPACITY: usize = 16;
const STREAM_CADENCE: std::time::Duration = std::time::Duration::from_millis(750);
const PREPARATION_RETRY: std::time::Duration = std::time::Duration::from_secs(1);
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Scheduler(#[from] scheduler::SchedulerError),
    #[error(transparent)]
    Session(#[from] SessionTaskError),
    #[error(transparent)]
    Notification(#[from] crate::notification_dispatch::NotificationDispatchError),
    #[error("{0}")]
    PreparationRefused(String),
    #[error("{0}")]
    PreparationPending(String),
    #[error("provider cleanup is not confirmed: {0}")]
    CleanupUnconfirmed(String),
    #[error("account already has a worker")]
    AlreadyRunning,
    #[error("account worker is closed")]
    Closed,
    #[error("account worker shutdown timed out; pending store work remains owned by bridge")]
    ShutdownTimeout,
    #[error("account worker task failed")]
    TaskFailed,
    #[error("account evidence capacity exhausted; recovery required")]
    Capacity,
    #[error("provider callback response is unresolved; recovery required")]
    CallbackUnknown,
}

/// Share one bridge among all account workers of a runtime. The permit bounds
/// queued blocking work as well as SQLite access; no database call runs on Tokio.
#[derive(Clone)]
pub struct StoreBridge {
    store: Arc<RuntimeStore>,
    slot: Arc<Semaphore>,
    preparation_slots: Arc<Semaphore>,
    accounts: Arc<std::sync::Mutex<BTreeSet<String>>>,
    #[cfg(test)]
    faults: Arc<tests::Faults>,
}
impl StoreBridge {
    pub async fn new(store: Arc<RuntimeStore>) -> Result<Self, WorkerError> {
        let bridge = Self {
            store,
            slot: Arc::new(Semaphore::new(1)),
            preparation_slots: Arc::new(Semaphore::new(2)),
            accounts: Arc::default(),
            #[cfg(test)]
            faults: Arc::default(),
        };
        bridge
            .run(|store| Ok(scheduler::recover_on_startup(store)?))
            .await?;
        Ok(bridge)
    }
    async fn run<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&RuntimeStore) -> Result<T, WorkerError> + Send + 'static,
    ) -> Result<T, WorkerError> {
        let permit = self
            .slot
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| WorkerError::Closed)?;
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            operation(&store)
        })
        .await
        .map_err(|_| WorkerError::TaskFailed)?
    }
    async fn prepare_run<T: Send + 'static>(
        &self,
        name: &'static str,
        operation: impl FnOnce(&RuntimeStore) -> Result<T, WorkerError> + Send + 'static,
    ) -> Result<T, WorkerError> {
        #[cfg(test)]
        let index = self.faults.before(name)?;
        #[cfg(test)]
        self.faults.pause(index).await;
        #[cfg(not(test))]
        let _ = name;
        let permit = self
            .preparation_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| WorkerError::Closed)?;
        let store = self.store.clone();
        // The blocking owner retains its permit even if its caller is cancelled.
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            operation(&store)
        })
        .await
        .map_err(|_| WorkerError::TaskFailed)?;
        #[cfg(test)]
        self.faults.after(index, result.is_ok())?;
        result
    }
    async fn worker_run<T: Send + 'static>(
        &self,
        _name: &'static str,
        operation: impl FnOnce(&RuntimeStore) -> Result<T, WorkerError> + Send + 'static,
    ) -> Result<T, WorkerError> {
        #[cfg(test)]
        let index = self.faults.before(_name)?;
        #[cfg(test)]
        self.faults.pause(index).await;
        let result = self.run(operation).await;
        #[cfg(test)]
        self.faults.after(index, result.is_ok())?;
        result
    }
}

struct AccountLease {
    bridge: StoreBridge,
    account: String,
}
impl AccountLease {
    fn acquire(bridge: &StoreBridge, account: &str) -> Result<Self, WorkerError> {
        if !bridge
            .accounts
            .lock()
            .map_err(|_| WorkerError::TaskFailed)?
            .insert(account.into())
        {
            return Err(WorkerError::AlreadyRunning);
        }
        Ok(Self {
            bridge: bridge.clone(),
            account: account.into(),
        })
    }
}
impl Drop for AccountLease {
    fn drop(&mut self) {
        if let Ok(mut accounts) = self.bridge.accounts.lock() {
            accounts.remove(&self.account);
        }
    }
}

pub enum AccountCommand {
    #[cfg(test)]
    Barrier(tokio::sync::oneshot::Sender<usize>),
    /// The caller's durable event and epoch are rechecked inside the claim transaction.
    Wake { agent_id: String, epoch: i64 },
    /// Revoke the epoch and interrupt any observed native turn, including before its reply.
    Stop { agent_id: String },
}

pub struct AccountHandle {
    commands: mpsc::Sender<AccountCommand>,
    stop: Shutdown,
    task: JoinHandle<Result<(), WorkerError>>,
}
impl AccountHandle {
    pub async fn send(&self, command: AccountCommand) -> Result<(), WorkerError> {
        let mut stop = self.stop.subscribe();
        tokio::select! { biased; _ = Shutdown::cancelled(&mut stop) => Err(WorkerError::Closed), result = self.commands.send(command) => result.map_err(|_|WorkerError::Closed) }
    }
    pub fn try_send(
        &self,
        command: AccountCommand,
    ) -> Result<(), mpsc::error::TrySendError<AccountCommand>> {
        self.commands.try_send(command)
    }
    pub fn cancel(&self) {
        self.stop.cancel();
    }
    pub async fn completion(&mut self) -> Result<(), WorkerError> {
        (&mut self.task)
            .await
            .map_err(|_| WorkerError::TaskFailed)?
    }
    pub async fn shutdown(&mut self) -> Result<(), WorkerError> {
        self.cancel();
        self.completion().await
    }
}
impl Drop for AccountHandle {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// Constructor for the server factory. Launch resolution and store opening belong
/// to the caller's blocking boundary. It must reuse this bridge for the runtime.
pub async fn start_account_worker(
    bridge: StoreBridge,
    account: String,
    factory: Arc<dyn ProviderTaskFactory>,
    options: OpenOptions,
    config: SessionTaskConfig,
    resources: preparation::PreparationResources,
) -> Result<AccountHandle, WorkerError> {
    start_account_worker_with_preparation(bridge, account, factory, options, config, resources)
        .await
}

pub async fn start_account_worker_with_preparation(
    bridge: StoreBridge,
    account: String,
    factory: Arc<dyn ProviderTaskFactory>,
    options: OpenOptions,
    mut config: SessionTaskConfig,
    resources: preparation::PreparationResources,
) -> Result<AccountHandle, WorkerError> {
    let lease = AccountLease::acquire(&bridge, &account)?;
    let recovery_account = account.clone();
    let generation = config.generation;
    let recovered = bridge
        .worker_run("recover", move |store| {
            store::recover(store, &recovery_account, generation)
        })
        .await?;
    config.recovered = recovered.iter().map(|(_, record)| record.clone()).collect();
    let timeout_ms = config.timeout_ms;
    let mut session = SessionHandle::open(
        factory,
        ProviderHandle::for_account(&account),
        options,
        config,
    )
    .await?;
    let effect_account = account.clone();
    let (effects, streams) = bridge
        .worker_run("restore_effects_streams", move |store| {
            Ok((
                store::recover_effects(store, &effect_account)?,
                store::restore_streams(store, &effect_account)?,
            ))
        })
        .await?;
    let events = std::mem::replace(&mut session.events, mpsc::channel(1).1);
    let (commands, inbox) = mpsc::channel(COMMAND_CAPACITY);
    let stop = Shutdown::default();
    let worker = Worker {
        resources,
        bridge: bridge.clone(),
        account: account.clone(),
        generation,
        timeout_ms,
        control: session.control.clone(),
        session,
        events,
        stop: stop.subscribe(),
        inbox,
        queued_commands: VecDeque::new(),
        retry_at: Some(tokio::time::Instant::now()),
        pending: recovered
            .into_iter()
            .map(|(context, record)| (record.ticket.id, context))
            .collect(),
        flush_at: (streams.pending_bytes() > 0)
            .then(|| tokio::time::Instant::now() + STREAM_CADENCE),
        streams,
        deferred: VecDeque::new(),
        callbacks: BTreeMap::new(),
        effects,
        closing: false,
    };
    let mut cancelled_owner = stop.subscribe();
    #[cfg(test)]
    let faults = bridge.faults.clone();
    let task = tokio::spawn(async move {
        let run = worker.run();
        tokio::pin!(run);
        let result = tokio::select! { biased;
            result = &mut run => result,
            _ = Shutdown::cancelled(&mut cancelled_owner) => {
                #[cfg(test)]
                faults.shutdown_wait.notify_one();
                tokio::time::timeout(SHUTDOWN_GRACE, &mut run).await.unwrap_or(Err(WorkerError::ShutdownTimeout))
            },
        };
        drop(lease);
        result
    });
    Ok(AccountHandle {
        commands,
        stop,
        task,
    })
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Pending {
    intent: TurnIntent,
    method: String,
    operation: String,
    #[serde(default)]
    thread: Option<crate::thread_prepare::PreparedAttempt>,
    #[serde(default)]
    compiled: Option<crate::turn_prepare::PreparedSubmission>,
}
impl Pending {
    fn method(&self) -> Result<Method, WorkerError> {
        Method::from_wire(&self.method).ok_or_else(|| {
            StoreError::InvalidSyncObject {
                scope: "account_worker".into(),
                id: self.operation.clone(),
            }
            .into()
        })
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
enum Effect {
    #[serde(skip)]
    Callback(studio_runtime_process::EventCursor, Frame),
    Submit(TurnIntent),
    Interrupt {
        agent: String,
        thread: String,
        turn: String,
    },
    Wake(String, i64),
}
struct Worker {
    resources: preparation::PreparationResources,
    bridge: StoreBridge,
    account: String,
    generation: Generation,
    timeout_ms: u64,
    control: SessionControl,
    session: SessionHandle,
    events: mpsc::Receiver<SessionEvent>,
    stop: watch::Receiver<bool>,
    inbox: mpsc::Receiver<AccountCommand>,
    queued_commands: VecDeque<AccountCommand>,
    retry_at: Option<tokio::time::Instant>,
    pending: BTreeMap<RequestId, Pending>,
    streams: NotificationStreamBuffer,
    flush_at: Option<tokio::time::Instant>,
    deferred: VecDeque<JournalEntry>,
    effects: VecDeque<Effect>,
    closing: bool,
    callbacks: BTreeMap<String, studio_runtime_process::EventCursor>,
}
impl Worker {
    async fn run(mut self) -> Result<(), WorkerError> {
        let result = self.serve().await;
        self.control.cancel();
        let exit = self.session.completion().await;
        // Completion has an independent lane. Drain bounded delivered and retained
        // evidence before persisting disconnect; abandoned cursors are never ACKed.
        let cleanup = self.finish(exit).await;
        match cleanup.and(result) {
            Err(WorkerError::Session(SessionTaskError::Closed)) if *self.stop.borrow() => Ok(()),
            result => result,
        }
    }
    async fn serve(&mut self) -> Result<(), WorkerError> {
        loop {
            if self.closing || *self.stop.borrow() {
                return Ok(());
            }
            if self
                .flush_at
                .is_some_and(|at| at <= tokio::time::Instant::now())
            {
                self.flush().await?;
            }
            if self.session.status.borrow().initialization == InitializationStatus::Ready
                && let Some(effect) = self.effects.pop_front()
            {
                let durable = store::effect_key(&self.account, &effect);
                match effect {
                    Effect::Callback(cursor, frame) => self.answer_callback(cursor, frame).await?,
                    Effect::Submit(intent) => self.submit(intent, Method::TurnStart, None).await?,
                    Effect::Interrupt {
                        agent,
                        thread,
                        turn,
                    } => self.interrupt(agent, thread, turn).await?,
                    Effect::Wake(agent_id, epoch) => self.wake(agent_id, epoch).await?,
                }
                if !self.closing
                    && let Some(key) = durable
                {
                    self.db("complete_effect", move |store| {
                        store::complete_effect(store, &key)
                    })
                    .await?;
                }
                continue;
            }
            #[cfg(test)]
            self.bridge.faults.cycle();
            tokio::select! {
                biased;
                _ = Shutdown::cancelled(&mut self.stop) => { self.closing = true; self.control.cancel(); },
                _ = tokio::time::sleep_until(self.flush_at.unwrap_or_else(||tokio::time::Instant::now()+STREAM_CADENCE)), if self.flush_at.is_some() => self.flush().await?,
                _ = tokio::time::sleep_until(self.retry_at.unwrap_or_else(|| tokio::time::Instant::now() + PREPARATION_RETRY)), if self.retry_at.is_some() => self.retry_preparations().await?,
                command = async { match self.queued_commands.pop_front() { Some(command) => Some(command), None => self.inbox.recv().await } }, if self.session.status.borrow().initialization == InitializationStatus::Ready => match command {
                    #[cfg(test)]
                    Some(AccountCommand::Barrier(reply)) => { let _ignored = reply.send(self.bridge.faults.journal_count.load(std::sync::atomic::Ordering::SeqCst)); },
                    Some(AccountCommand::Wake { agent_id, epoch }) => self.wake(agent_id, epoch).await?,
                    Some(AccountCommand::Stop { agent_id }) => self.stop_agent(agent_id).await?,
                    None => return Ok(()),
                },
                event = self.events.recv() => match event { Some(event) => self.event(event).await?, None => return Ok(()) },
                changed = self.session.status.changed() => { if changed.is_err() { return Ok(()); } },
            }
            // One ready event after a command prevents a command flood starving evidence.
            if let Ok(event) = self.events.try_recv() {
                self.event(event).await?;
            }
        }
    }
    async fn db<T: Send + 'static>(
        &mut self,
        name: &'static str,
        operation: impl FnOnce(&RuntimeStore) -> Result<T, WorkerError> + Send + 'static,
    ) -> Result<T, WorkerError> {
        let bridge = self.bridge.clone();
        let job = bridge.worker_run(name, operation);
        tokio::pin!(job);
        tokio::select! { biased;
            _ = Shutdown::cancelled(&mut self.stop), if !self.closing => { self.closing = true; self.control.cancel(); job.await },
            result = &mut job => result,
        }
    }
    async fn prepare<T: Send + 'static>(
        &mut self,
        name: &'static str,
        operation: impl FnOnce(&RuntimeStore) -> Result<T, WorkerError> + Send + 'static,
    ) -> Result<T, WorkerError> {
        let bridge = self.bridge.clone();
        let job = bridge.prepare_run(name, operation);
        tokio::pin!(job);
        loop {
            tokio::select! { biased;
                _ = Shutdown::cancelled(&mut self.stop), if !self.closing => {
                    self.closing = true;
                    self.control.cancel();
                },
                result = &mut job => return result,
                event = self.events.recv(), if !self.closing => match event {
                    Some(event) => self.event(event).await?,
                    None => { self.closing = true; self.control.cancel(); }
                },
                command = self.inbox.recv(), if !self.closing && self.queued_commands.len() < COMMAND_CAPACITY => match command {
                    Some(AccountCommand::Stop { agent_id }) => self.stop_agent(agent_id).await?,
                    Some(command) => self.queued_commands.push_back(command),
                    None => { self.closing = true; self.control.cancel(); }
                },
            }
        }
    }
    // Session commands can wait behind event backpressure. Keep settling events
    // while waiting; the future is retained exactly once, including shutdown.
    async fn drive<T>(
        &mut self,
        future: impl Future<Output = Result<T, SessionTaskError>>,
    ) -> Result<T, WorkerError> {
        tokio::pin!(future);
        loop {
            tokio::select! { biased;
                _ = Shutdown::cancelled(&mut self.stop), if !self.closing => { self.closing = true; self.control.cancel(); },
                _ = tokio::time::sleep_until(self.flush_at.unwrap_or_else(||tokio::time::Instant::now()+STREAM_CADENCE)), if self.flush_at.is_some() => self.flush().await?,
                result = &mut future => return result.map_err(Into::into),
                event = self.events.recv(), if !self.closing => match event { Some(event) => self.event(event).await?, None => { self.closing=true; self.control.cancel(); } },
            }
        }
    }
    async fn retry_preparations(&mut self) -> Result<(), WorkerError> {
        self.retry_at = None;
        let account = self.account.clone();
        let pending = self
            .db("retry_preparations", move |store| {
                store::pending_preparations(store, &account)
            })
            .await?;
        if !pending.is_empty() {
            self.retry_at = Some(tokio::time::Instant::now() + PREPARATION_RETRY);
        }
        for (agent, epoch, reason, provider) in pending {
            if reason == "model-catalog/refresh-pending" {
                // This initiates/coalesces refresh outside the store bridge. The
                // shared service owns the child and its cleanup until completion.
                let _status = self
                    .resources
                    .model_catalog
                    .read(
                        &self.account,
                        &self.resources.account_home,
                        &provider,
                        false,
                    )
                    .await;
            }
            self.wake(agent, epoch).await?;
        }
        Ok(())
    }
    async fn wake(&mut self, agent: String, epoch: i64) -> Result<(), WorkerError> {
        if self.closing || self.pending.len() >= MAX_PENDING_RESPONSES {
            return Ok(());
        }
        let account = self.account.clone();
        let candidate = self
            .db("claim", move |store| {
                store::claim(store, &account, &agent, epoch)
            })
            .await?;
        if let Some(intent) = candidate {
            self.submit(intent, Method::ThreadResume, None).await?;
        }
        Ok(())
    }
    async fn submit(
        &mut self,
        intent: TurnIntent,
        method: Method,
        interrupt_turn: Option<(String, i64)>,
    ) -> Result<(), WorkerError> {
        if self.closing {
            return Ok(());
        }
        // A recovered durable effect may outlive its submitted request. Check
        // that receipt before preparation, which requires an unsubmitted owner.
        if method == Method::TurnStart {
            let operation = format!(
                "account:{}:{}:{}",
                self.account,
                intent.attempt_id,
                method.as_str()
            );
            if self
                .db("was_submitted", move |store| {
                    store::was_submitted(store, &operation)
                })
                .await?
            {
                return Ok(());
            }
        }
        let turn = interrupt_turn.clone();
        let generation = self.generation.0;
        let prepared = if method == Method::TurnInterrupt {
            None
        } else {
            let i = intent.clone();
            let resources = self.resources.clone();
            let Some(selection) = self
                .prepare("preparation", move |s| {
                    store::preparation_step(s, &i, || preparation::admit(s, &resources, &i))
                })
                .await?
            else {
                self.retry_at = Some(tokio::time::Instant::now() + PREPARATION_RETRY);
                return Ok(());
            };
            let i = intent.clone();
            let Some(token) = self
                .prepare("workspace", move |s| {
                    store::preparation_step(s, &i, || preparation::workspace(s, &i))
                })
                .await?
            else {
                self.retry_at = Some(tokio::time::Instant::now() + PREPARATION_RETRY);
                return Ok(());
            };
            let i = intent.clone();
            let resources = self.resources.clone();
            let Some(prepared) = self
                .prepare("preparation", move |s| {
                    store::preparation_step(s, &i, || {
                        preparation::prepare_ready(
                            s, &resources, &i, method, generation, selection, token,
                        )
                    })
                })
                .await?
            else {
                self.retry_at = Some(tokio::time::Instant::now() + PREPARATION_RETRY);
                return Ok(());
            };
            Some(prepared)
        };
        let (intent, method, params, submission, guard, thread, compiled) = if let Some(prepared) =
            prepared
        {
            (
                prepared.intent,
                prepared.method,
                prepared.params,
                prepared.submission,
                Some(prepared.guard),
                prepared.thread,
                prepared.compiled,
            )
        } else {
            let params = json!({"threadId":intent.thread_id,"turnId":turn.as_ref().map(|(t,_)|t)});
            (intent, method, params, None, None, None, None)
        };
        let mut operation = format!(
            "account:{}:{}:{}",
            self.account,
            intent.attempt_id,
            method.as_str()
        );
        if let Some((_, stop_epoch)) = &interrupt_turn {
            operation.push_str(&format!(":stop:{stop_epoch}"));
        }
        let prior = operation.clone();
        if self
            .db("was_submitted", move |store| {
                store::was_submitted(store, &prior)
            })
            .await?
        {
            return Ok(());
        }
        let id = RequestId::String(operation.clone());
        let context = Pending {
            thread,
            compiled,
            intent,
            method: method.as_str().into(),
            operation: operation.clone(),
        };
        let control = self.control.clone();
        let prepared = self
            .drive(control.prepare(RequestSpec {
                id: id.clone(),
                operation_id: OperationId(operation),
                method,
                params,
                thread_id: context.intent.thread_id.clone(),
                lightweight: method == Method::TurnInterrupt,
                timeout_ms: self.timeout_ms,
            }))
            .await?;
        if self.closing {
            return Ok(());
        }
        let recovery = prepared.recovery_record();
        let save_context = context.clone();
        let account = self.account.clone();
        let authorized = self
            .db("submitted", move |store| {
                store::submitted(
                    store,
                    &save_context,
                    &account,
                    &recovery,
                    interrupt_turn.as_ref().map(|(turn, _)| turn.as_str()),
                    submission.as_ref(),
                    guard.as_ref(),
                )
            })
            .await?;
        if !authorized || self.closing {
            return Ok(());
        }
        self.pending.insert(id.clone(), context);
        let control = self.control.clone();
        let admission = control.submit(prepared);
        #[cfg(test)]
        let faults = self.bridge.faults.clone();
        #[cfg(test)]
        let admission = async move {
            let receipt = admission.await;
            if method == Method::TurnStart
                && faults
                    .hold_admission
                    .load(std::sync::atomic::Ordering::SeqCst)
            {
                faults.admission_held.notify_one();
                faults.release_admission.notified().await;
            }
            receipt
        };
        match self.drive(admission).await {
            Ok(_ticket) => {}
            Err(error) => {
                // Only the session's explicit pre-admission rejection proves NotSent.
                // A failure while draining unrelated evidence may follow an enqueued write.
                let outcome = if definite_rejection(&error) {
                    WriteOutcome::NotSent
                } else {
                    WriteOutcome::Unknown
                };
                self.event(SessionEvent {
                    journal: None,
                    outcomes: vec![SessionOutcome::Write {
                        operation_id: OperationId(String::new()),
                        ticket: Some(studio_runtime_providers::RequestTicket {
                            id,
                            generation: self.generation.0,
                            method,
                        }),
                        outcome,
                        recovery: None,
                    }],
                })
                .await?;
            }
        }
        Ok(())
    }
    async fn stop_agent(&mut self, agent: String) -> Result<(), WorkerError> {
        let account = self.account.clone();
        let effect = self
            .db("stop", move |store| store::stop(store, &account, &agent))
            .await?;
        if let Some(effect) = effect {
            self.push(effect)?;
        }
        Ok(())
    }
    async fn interrupt(
        &mut self,
        agent: String,
        thread: String,
        turn: String,
    ) -> Result<(), WorkerError> {
        let intent = self
            .db("interrupt_intent", move |store| {
                store::interrupt_intent(store, &agent, &thread)
            })
            .await?;
        if let Some((intent, stop_epoch)) = intent {
            self.submit(intent, Method::TurnInterrupt, Some((turn, stop_epoch)))
                .await?;
        }
        Ok(())
    }
    fn push(&mut self, effect: Effect) -> Result<(), WorkerError> {
        if self.effects.len() >= MAX_PENDING_RESPONSES {
            return Err(WorkerError::Capacity);
        }
        self.effects.push_back(effect);
        Ok(())
    }
    async fn event(&mut self, event: SessionEvent) -> Result<(), WorkerError> {
        let reclassify = !event.outcomes.is_empty();
        for outcome in event.outcomes {
            if let SessionOutcome::Write {
                operation_id,
                ticket: None,
                outcome,
                ..
            } = &outcome
                && let Some(cursor) = self.callbacks.remove(&operation_id.0)
            {
                let operation = operation_id.0.clone();
                let written = *outcome == WriteOutcome::Written;
                let write_outcome = *outcome;
                self.db("callback_written", move |store| {
                    store::callback_written(store, &operation, write_outcome)
                })
                .await?;
                if !written {
                    return Err(WorkerError::CallbackUnknown);
                }
                let account = self.account.clone();
                let generation = self.generation.0;
                self.db("callback_settled", move |store| {
                    store::callback_settled(store, &account, generation, cursor.0)
                })
                .await?;
                if !self.closing {
                    self.control.settled(cursor).await?;
                }
                continue;
            }
            let ticket = match &outcome {
                SessionOutcome::Write { ticket, .. } => ticket.as_ref(),
                SessionOutcome::Resolved { ticket, .. } => Some(ticket),
                _ => None,
            };
            let pending = ticket.and_then(|t| self.pending.get(&t.id)).cloned();
            let id = ticket.map(|t| t.id.clone());
            let account = self.account.clone();
            let resources = self.resources.clone();
            let (retire, effect) = self
                .db("outcome", move |store| {
                    store::outcome(store, &account, &resources, pending.as_ref(), outcome)
                })
                .await?;
            if retire && let Some(id) = id {
                self.pending.remove(&id);
            }
            if let Some(effect) = effect {
                self.push(effect)?;
            }
        }
        if let Some(entry) = event.journal {
            self.journal(entry).await?;
        }
        if reclassify {
            for _ in 0..self.deferred.len() {
                if let Some(entry) = self.deferred.pop_front() {
                    self.journal(entry).await?;
                }
            }
        }
        Ok(())
    }
    async fn journal(&mut self, entry: JournalEntry) -> Result<(), WorkerError> {
        let cursor = entry.cursor;
        let account = self.account.clone();
        let streams = std::mem::take(&mut self.streams);
        let (entry, previous, committed) = self
            .db("journal", move |store| {
                let result = store::journal(store, &account, &streams, &entry);
                Ok((entry, streams, result))
            })
            .await?;
        self.streams = previous;
        let Some((streams, effect)) = committed? else {
            if self.deferred.len() == MAX_UNSETTLED {
                return Err(WorkerError::Capacity);
            }
            self.deferred.push_back(entry);
            return Ok(());
        };
        #[cfg(test)]
        self.bridge
            .faults
            .journal_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.streams = streams; // commit precedes buffer replacement and ACK
        self.arm_flush();
        if let Some(effect) = effect {
            self.push(effect)?;
        }
        if let JournalPayload::Frame(frame) = &entry.payload
            && let Some(response) = callback::response(frame)
        {
            self.push(Effect::Callback(cursor, response))?;
        } else if !self.closing {
            let control = self.control.clone();
            if control.settled(cursor).await.is_err() {
                self.closing = true;
                self.control.cancel();
            }
        }
        Ok(())
    }
    async fn answer_callback(
        &mut self,
        cursor: studio_runtime_process::EventCursor,
        frame: Frame,
    ) -> Result<(), WorkerError> {
        while self.session.status.borrow().initialization != InitializationStatus::Ready {
            tokio::select! { biased;
                _ = Shutdown::cancelled(&mut self.stop) => { self.closing = true; return Ok(()); },
                changed = self.session.status.changed() => { changed.map_err(|_| WorkerError::Closed)?; },
                event = self.events.recv() => { self.event(event.ok_or(WorkerError::Closed)?).await?; },
            }
        }
        let operation = format!(
            "callback:{}:{}:{}",
            self.account, self.generation.0, cursor.0
        );
        let prepared = self
            .control
            .prepare_frame(OperationId(operation.clone()), &frame)?;
        let key = operation.clone();
        let account = self.account.clone();
        let generation = self.generation.0;
        let admission = self
            .db("callback_admission", move |store| {
                store::callback_admission(store, &key, &account, generation, cursor.0)
            })
            .await?;
        if admission {
            self.control.settled(cursor).await?;
            return Ok(());
        }
        if self.callbacks.len() >= MAX_PENDING_RESPONSES {
            return Err(WorkerError::Capacity);
        }
        self.callbacks.insert(operation.clone(), cursor);
        let control = self.control.clone();
        if let Err(error) = self.drive(control.submit_frame(prepared)).await {
            if definite_rejection(&error) {
                self.callbacks.remove(&operation);
                self.db("callback_written", move |store| {
                    store::callback_written(store, &operation, WriteOutcome::NotSent)
                })
                .await?;
            }
            return Err(error);
        }
        Ok(())
    }
    fn arm_flush(&mut self) {
        if self.streams.pending_bytes() == 0 {
            self.flush_at = None;
        } else if self.flush_at.is_none() {
            self.flush_at = Some(tokio::time::Instant::now() + STREAM_CADENCE);
        }
    }
    async fn flush(&mut self) -> Result<(), WorkerError> {
        let account = self.account.clone();
        let streams = std::mem::take(&mut self.streams);
        self.streams = self
            .db("flush_streams", move |store| {
                store::flush_streams(store, &account, streams)
            })
            .await?;
        self.flush_at = None;
        self.arm_flush();
        Ok(())
    }
    async fn finish(&mut self, exit: SessionExit) -> Result<(), WorkerError> {
        self.closing = true;
        while let Ok(event) = self.events.try_recv() {
            self.event(event).await?;
        }
        for event in exit.undelivered {
            self.event(event).await?;
        }
        self.event(SessionEvent {
            journal: None,
            outcomes: exit.outcomes,
        })
        .await?;
        self.flush().await?;
        let account = self.account.clone();
        let generation = self.generation.0;
        let cleanup_error = exit.adapter.as_ref().and_then(|a| a.cleanup_error.clone());
        let recovery = exit.adapter.and_then(|a| a.recovery_needed);
        self.db("finish", move |store| {
            if let Some(recovery) = recovery {
                store::save_recovery(store, &account, &recovery)?;
            }
            scheduler::lifecycle_transaction(store, |tx| {
                SchedulerRepository::disconnect_account(tx, &account, generation, now(), false)
            })?;
            Ok(())
        })
        .await?;
        if let Some(error) = cleanup_error {
            return Err(WorkerError::CleanupUnconfirmed(error));
        }
        exit.result.map_err(Into::into)
    }
}
fn definite_rejection(error: &WorkerError) -> bool {
    matches!(
        error,
        WorkerError::Session(
            SessionTaskError::Protocol(studio_runtime_providers::SessionError::NotReady)
                | SessionTaskError::Capacity
                | SessionTaskError::Invalid
        )
    )
}
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

#[cfg(test)]
mod tests;

/// Durable submission boundary shared by account transport and lifecycle replay.
/// The caller owns the transaction, including canonical agent publication.
pub fn commit_prepared_turn(
    tx: &studio_runtime_store::Transaction<'_>,
    intent: &TurnIntent,
    request_id: &str,
    generation: u64,
    submission: &crate::notification_dispatch::PreparedSubmission,
) -> Result<bool, WorkerError> {
    if AgentRepository::get(tx, &AgentId::from(intent.agent_id.as_str()))?.is_none_or(|agent| {
        agent
            .extra
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            != intent.account_key
    }) {
        return Ok(false);
    }
    let replay = studio_runtime_store::ExecutionHistoryRepository::attempt(tx, &intent.attempt_id)?
        .is_some_and(|a| a["receipts"]["submission"].is_object());
    let may_submit = replay
        || SchedulerRepository::reconcile_obsolete_before_submit(
            tx,
            &intent.agent_id,
            intent.epoch,
            &intent.attempt_id,
        )?;
    if !may_submit {
        return Ok(false);
    }
    if !SchedulerRepository::mark_submitted_identity(
        tx,
        &intent.agent_id,
        intent.epoch,
        &intent.attempt_id,
        request_id,
        &intent.account_key,
        generation,
        intent.thread_id.as_deref().unwrap_or(""),
    )? {
        return Ok(false);
    }
    crate::notification_dispatch::record_user_submission(tx, &intent.agent_id, submission)?;
    Ok(true)
}
