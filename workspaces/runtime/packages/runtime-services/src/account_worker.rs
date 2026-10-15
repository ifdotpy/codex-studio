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
    sync::{Notify, Semaphore, mpsc, watch},
    task::{JoinHandle, JoinSet},
};
pub mod callback;
mod connection_recovery;
pub(crate) mod history;
pub mod preparation;
pub(crate) mod restart_recovery;
pub mod retained;
pub mod retained_frame;
pub(crate) mod store;

impl From<rusqlite::Error> for WorkerError {
    fn from(error: rusqlite::Error) -> Self {
        StoreError::from(error).into()
    }
}

pub const COMMAND_CAPACITY: usize = 16;
// Maximum provider events per journal transaction; bounds transaction work and ACK delay.
const JOURNAL_BATCH_MAX_EVENTS: usize = 32;
const STREAM_CADENCE: std::time::Duration = std::time::Duration::from_millis(750);
const PREPARATION_RETRY: std::time::Duration = std::time::Duration::from_secs(1);
const PUBLICATION_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
// A worker can be finishing a serialized provider operation when shutdown is
// requested. Keep shutdown bounded while allowing ordinary host contention to
// clear before reporting an unconfirmed stop.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

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
    #[error("VM actor identity changed while journaling")]
    StaleVmActor,
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
    event_wake: Arc<std::sync::RwLock<Arc<Notify>>>,
    continuation_wake: Arc<std::sync::RwLock<Arc<Notify>>>,
    monitor_dispatch_wake: Arc<std::sync::RwLock<Arc<Notify>>>,
    managed_tool_runtime:
        Arc<std::sync::RwLock<Option<Arc<crate::provider_callbacks::ManagedToolRuntime>>>>,
    #[cfg(test)]
    faults: Arc<tests::Faults>,
    #[cfg(test)]
    account_released: Arc<tokio::sync::Notify>,
}
impl StoreBridge {
    pub async fn new(store: Arc<RuntimeStore>) -> Result<Self, WorkerError> {
        let bridge = Self {
            store,
            slot: Arc::new(Semaphore::new(1)),
            preparation_slots: Arc::new(Semaphore::new(2)),
            accounts: Arc::default(),
            event_wake: Arc::new(std::sync::RwLock::new(Arc::new(Notify::new()))),
            continuation_wake: Arc::new(std::sync::RwLock::new(Arc::new(Notify::new()))),
            monitor_dispatch_wake: Arc::new(std::sync::RwLock::new(Arc::new(Notify::new()))),
            managed_tool_runtime: Arc::new(std::sync::RwLock::new(None)),
            #[cfg(test)]
            faults: Arc::default(),
            #[cfg(test)]
            account_released: Arc::default(),
        };
        bridge
            .run(|store| Ok(scheduler::recover_on_startup(store)?))
            .await?;
        Ok(bridge)
    }
    pub fn set_event_wake(&self, wake: Arc<Notify>) {
        *self
            .event_wake
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = wake;
    }
    pub(crate) fn event_wake(&self) -> Arc<Notify> {
        self.event_wake
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn set_continuation_wake(&self, wake: Arc<Notify>) {
        *self
            .continuation_wake
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = wake;
    }
    pub(crate) fn continuation_wake(&self) -> Arc<Notify> {
        self.continuation_wake
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub fn set_monitor_dispatch_wake(&self, wake: Arc<Notify>) {
        *self
            .monitor_dispatch_wake
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = wake;
    }
    pub fn set_managed_tool_runtime(
        &self,
        runtime: Arc<crate::provider_callbacks::ManagedToolRuntime>,
    ) {
        *self
            .managed_tool_runtime
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(runtime);
    }
    pub(crate) fn managed_tool_runtime(
        &self,
    ) -> Option<Arc<crate::provider_callbacks::ManagedToolRuntime>> {
        self.managed_tool_runtime
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
    pub(crate) fn monitor_dispatch_wake(&self) -> Arc<Notify> {
        self.monitor_dispatch_wake
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/account_worker",
            move || {
                let _permit = permit;
                operation(&store)
            },
        )
        .await
        .map_err(|_| WorkerError::TaskFailed)?
    }
    async fn prepare_run<T: Send + 'static>(
        &self,
        name: &'static str,
        operation: impl FnOnce(&RuntimeStore) -> Result<T, WorkerError> + Send + 'static,
    ) -> Result<T, WorkerError> {
        #[cfg(test)]
        let index = self.faults.before(name, None)?;
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
        let result = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/account_worker",
            move || {
                let _permit = permit;
                operation(&store)
            },
        )
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
        self.worker_run_identified(_name, None, operation).await
    }
    async fn worker_run_identified<T: Send + 'static>(
        &self,
        _name: &'static str,
        identity: Option<String>,
        operation: impl FnOnce(&RuntimeStore) -> Result<T, WorkerError> + Send + 'static,
    ) -> Result<T, WorkerError> {
        #[cfg(test)]
        let index = self.faults.before(_name, identity)?;
        #[cfg(not(test))]
        let _ = identity;
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
        #[cfg(test)]
        self.bridge.account_released.notify_one();
    }
}

pub enum AccountCommand {
    RetainedRequest(Box<retained::RequestCommand>),
    VmNotification(Box<retained::VmNotificationCommand>),
    CommittedFrame(Box<retained_frame::FrameCommand>),
    JournalBarrier(retained::BarrierCommand),
    ConnectionRecovery {
        agent_id: String,
        reply: tokio::sync::oneshot::Sender<Result<Value, retained::PortError>>,
    },
    ConnectionRecoveryComplete {
        result: Result<Value, retained::PortError>,
        streams: Option<NotificationStreamBuffer>,
        reply: tokio::sync::oneshot::Sender<Result<Value, retained::PortError>>,
    },
    #[cfg(test)]
    Barrier(tokio::sync::oneshot::Sender<usize>),
    /// The caller's durable event and epoch are rechecked inside the claim transaction.
    Wake {
        agent_id: String,
        epoch: i64,
    },
    /// Revoke the epoch and interrupt any observed native turn, including before its reply.
    Stop {
        agent_id: String,
    },
    /// Interrupt a radio turn after its scheduler has committed the one-shot intent.
    RadioInterrupt {
        agent_id: String,
        thread_id: String,
        turn_id: String,
    },
    RestartRecoveryDone,
    RestartStreams(Box<NotificationStreamBuffer>),
}

pub struct AccountHandle {
    scope: crate::recovery_history::ConnectionScope,
    commands: mpsc::Sender<AccountCommand>,
    stop: Shutdown,
    task: JoinHandle<Result<(), WorkerError>>,
    finished: watch::Receiver<bool>,
    #[cfg(test)]
    faults: Arc<tests::Faults>,
}
impl AccountHandle {
    pub fn completion_signal(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }
    pub fn retained_session(&self) -> retained::RetainedSessionPort {
        retained::RetainedSessionPort {
            scope: self.scope.clone(),
            commands: self.commands.clone(),
            stop: self.stop.clone(),
        }
    }
    pub fn try_radio_interrupt(
        &self,
        agent_id: String,
        thread_id: String,
        turn_id: String,
    ) -> Result<(), mpsc::error::TrySendError<AccountCommand>> {
        self.commands.try_send(AccountCommand::RadioInterrupt {
            agent_id,
            thread_id,
            turn_id,
        })
    }
    pub fn radio_interrupt_sender(&self) -> mpsc::Sender<AccountCommand> {
        self.commands.clone()
    }
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
    config: SessionTaskConfig,
    resources: preparation::PreparationResources,
) -> Result<AccountHandle, WorkerError> {
    start_account_worker_with_publication_sink(
        bridge,
        account,
        factory,
        options,
        config,
        resources,
        Arc::new(crate::notification_runtime_effects::UnavailablePostCommitSink),
    )
    .await
}

pub async fn start_account_worker_with_publication_sink(
    bridge: StoreBridge,
    account: String,
    factory: Arc<dyn ProviderTaskFactory>,
    options: OpenOptions,
    mut config: SessionTaskConfig,
    resources: preparation::PreparationResources,
    publication_sink: Arc<dyn crate::notification_runtime_effects::NotificationPostCommitSink>,
) -> Result<AccountHandle, WorkerError> {
    let lease = AccountLease::acquire(&bridge, &account)?;
    let generation = config.generation;
    let recovered = loop {
        let recovery_account = account.clone();
        match bridge
            .worker_run("recover", move |store| {
                store::recover(store, &recovery_account, generation)
            })
            .await
        {
            Ok(recovered) => break recovered,
            Err(error) if invalid_agent_record_error(&error) => {
                let (invalid_id, pointer, reason) = invalid_agent_record_details(&error)
                    .expect("guard matches InvalidSyncObjectField");
                let invalid_id = invalid_id.to_owned();
                let pointer = pointer.to_owned();
                let reason = reason.to_owned();
                let diagnostic = error.to_string();
                bridge
                    .worker_run("hold_invalid_agent", move |store| {
                        store::hold_invalid_agent_record(
                            store,
                            &invalid_id,
                            &pointer,
                            &reason,
                            &diagnostic,
                        )
                    })
                    .await?;
            }
            Err(error) => return Err(error),
        }
    };
    config.recovered = recovered.iter().map(|(_, record)| record.clone()).collect();
    let recovery_account = account.clone();
    let retained = bridge
        .worker_run("recover_retained", move |s| {
            retained::recover(s, &recovery_account, generation.0)
        })
        .await?;
    config
        .recovered
        .extend(retained.iter().map(|(_, request)| request.clone()));
    if config.recovered.len() > MAX_PENDING_RESPONSES {
        return Err(WorkerError::Capacity);
    }
    let timeout_ms = config.timeout_ms;
    let mut session = SessionHandle::open(
        factory,
        ProviderHandle::for_account(&account),
        options,
        config,
    )
    .await?;
    let connection_id = uuid::Uuid::new_v4().to_string();
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
    let (vm_event_tx, vm_events) = mpsc::channel(COMMAND_CAPACITY);
    let (commands, inbox) = mpsc::channel(COMMAND_CAPACITY);
    let stop = Shutdown::default();
    let scope = crate::recovery_history::ConnectionScope {
        account: account.clone(),
        connection: connection_id.clone(),
        generation: generation.0,
    };
    let retained_port = retained::RetainedSessionPort {
        scope: scope.clone(),
        commands: commands.clone(),
        stop: stop.clone(),
    };
    let auth_probe_at = (resources.claude_account_pin.get("provider") == Some(&json!("claude")))
        .then(tokio::time::Instant::now);
    let worker = Worker {
        resources,
        retained_port,
        bridge: bridge.clone(),
        account: account.clone(),
        auth_probe_at,
        generation,
        connection_id,
        timeout_ms,
        control: session.control.clone(),
        session,
        events,
        vm_events,
        vm_event_tx,
        vm_sessions: BTreeMap::new(),
        resolving_vm_actors: BTreeSet::new(),
        vm_event_forwarders: Vec::new(),
        vm_request_waiters: BTreeMap::new(),
        stop: stop.subscribe(),
        inbox,
        commands: commands.clone(),
        retained_session: retained::RetainedSessionPort {
            scope: scope.clone(),
            commands: commands.clone(),
            stop: stop.clone(),
        },
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
        vm_notification_waiters: BTreeMap::new(),
        callbacks: BTreeMap::new(),
        managed_callback_cursors: BTreeSet::new(),
        retained_requests: retained
            .into_iter()
            .map(|(pending, request)| (request.ticket.id, pending))
            .collect(),
        retained_barriers: Vec::new(),
        retained_frames: BTreeMap::new(),
        effects,
        publication_sink,
        publication_retry_at: Some(tokio::time::Instant::now()),
        closing: false,
        restart_recovery_started: false,
        restart_recovery: None,
        connection_recovery: tokio::task::JoinSet::new(),
        connection_recovery_active: false,
        managed_callbacks: JoinSet::new(),
    };
    let mut cancelled_owner = stop.subscribe();
    let (finished_tx, finished) = watch::channel(false);
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
        let _ = finished_tx.send(true);
        result
    });
    Ok(AccountHandle {
        scope,
        commands,
        stop,
        task,
        finished,
        #[cfg(test)]
        faults: bridge.faults.clone(),
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
    Callback(studio_runtime_process::EventCursor, Frame, Option<String>),
    Submit(TurnIntent),
    Interrupt {
        agent: String,
        thread: String,
        turn: String,
    },
    Wake(String, i64),
}
struct Worker {
    retained_requests: BTreeMap<RequestId, retained::PendingRequest>,
    retained_frames: BTreeMap<String, retained_frame::PendingFrame>,
    retained_barriers: Vec<retained::PendingBarrier>,
    resources: preparation::PreparationResources,
    retained_port: retained::RetainedSessionPort,
    bridge: StoreBridge,
    account: String,
    auth_probe_at: Option<tokio::time::Instant>,
    generation: Generation,
    connection_id: String,
    timeout_ms: u64,
    control: SessionControl,
    session: SessionHandle,
    events: mpsc::Receiver<SessionEvent>,
    vm_events: mpsc::Receiver<(String, i64, SessionEvent)>,
    vm_event_tx: mpsc::Sender<(String, i64, SessionEvent)>,
    vm_sessions: BTreeMap<String, ActorProviderSession>,
    resolving_vm_actors: BTreeSet<String>,
    vm_event_forwarders: Vec<JoinHandle<()>>,
    vm_request_waiters: BTreeMap<String, tokio::sync::oneshot::Sender<RequestOutcome>>,
    stop: watch::Receiver<bool>,
    inbox: mpsc::Receiver<AccountCommand>,
    commands: mpsc::Sender<AccountCommand>,
    retained_session: retained::RetainedSessionPort,
    queued_commands: VecDeque<AccountCommand>,
    retry_at: Option<tokio::time::Instant>,
    pending: BTreeMap<RequestId, Pending>,
    streams: NotificationStreamBuffer,
    flush_at: Option<tokio::time::Instant>,
    deferred: VecDeque<(JournalEntry, Option<String>, Option<i64>)>,
    vm_notification_waiters:
        BTreeMap<(String, u64), tokio::sync::oneshot::Sender<Result<(), retained::PortError>>>,
    effects: VecDeque<Effect>,
    publication_sink: Arc<dyn crate::notification_runtime_effects::NotificationPostCommitSink>,
    publication_retry_at: Option<tokio::time::Instant>,
    closing: bool,
    callbacks: BTreeMap<String, (studio_runtime_process::EventCursor, Option<String>)>,
    managed_callback_cursors: BTreeSet<(Option<String>, u64)>,
    restart_recovery_started: bool,
    restart_recovery: Option<JoinHandle<()>>,
    connection_recovery: tokio::task::JoinSet<()>,
    connection_recovery_active: bool,
    managed_callbacks: JoinSet<(
        studio_runtime_process::EventCursor,
        Option<String>,
        Option<Frame>,
    )>,
}

struct ActorProviderSession {
    session: SessionHandle,
    epoch: i64,
    control: SessionControl,
    bootstrap: Option<Value>,
}
impl Worker {
    async fn run(mut self) -> Result<(), WorkerError> {
        let result = self.serve().await;
        let drain_result = self.drain_managed_callbacks().await;
        let result = result.and(drain_result);
        if let Some(recovery) = self.restart_recovery.take() {
            recovery.abort();
            let _joined = recovery.await;
        }
        self.connection_recovery.abort_all();
        while self.connection_recovery.join_next().await.is_some() {}
        self.control.cancel();
        let exit = self.session.completion().await;
        for session in self.vm_sessions.values() {
            session.control.cancel();
        }
        let mut vm_cleanup_error = None;
        for (agent, session) in &mut self.vm_sessions {
            match tokio::time::timeout(SHUTDOWN_GRACE, session.session.completion()).await {
                Ok(exit) => {
                    if let Some(error) = exit.adapter.and_then(|adapter| adapter.cleanup_error) {
                        vm_cleanup_error.get_or_insert_with(|| {
                            WorkerError::CleanupUnconfirmed(format!("VM actor {agent}: {error}"))
                        });
                    }
                    if let Err(error) = exit.result {
                        vm_cleanup_error.get_or_insert_with(|| {
                            WorkerError::CleanupUnconfirmed(format!("VM actor {agent}: {error}"))
                        });
                    }
                }
                Err(_) => {
                    vm_cleanup_error.get_or_insert_with(|| {
                        WorkerError::CleanupUnconfirmed(format!(
                            "VM actor {agent}: shutdown timed out"
                        ))
                    });
                }
            }
        }
        let mut forwarders = std::mem::take(&mut self.vm_event_forwarders);
        while let Some(mut forwarder) = forwarders.pop() {
            loop {
                tokio::select! {
                    _joined = &mut forwarder => break,
                    event = self.vm_events.recv() => match event {
                        Some((agent_id, actor_epoch, event)) => {
                            let closing = self.closing;
                            self.closing = false;
                            if let Err(error) = self.event_from(event, Some(&agent_id), Some(actor_epoch)).await {
                                vm_cleanup_error.get_or_insert(error);
                            }
                            self.closing = closing;
                        }
                        None => break,
                    }
                }
            }
        }
        while let Ok((agent_id, actor_epoch, event)) = self.vm_events.try_recv() {
            let closing = self.closing;
            self.closing = false;
            if let Err(error) = self.event_from(event, Some(&agent_id), Some(actor_epoch)).await {
                vm_cleanup_error.get_or_insert(error);
            }
            self.closing = closing;
        }
        // Completion has an independent lane. Drain bounded delivered and retained
        // evidence before persisting disconnect; abandoned cursors are never ACKed.
        let cleanup = self.finish(exit).await;
        let cleanup = cleanup.and_then(|()| vm_cleanup_error.map_or(Ok(()), Err));
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
            if !self.restart_recovery_started
                && self.session.status.borrow().initialization == InitializationStatus::Ready
            {
                self.restart_recovery_started = true;
                let first_pass = restart_recovery::FirstPass {
                    bridge: self.bridge.clone(),
                    account: self.account.clone(),
                    scope: self.retained_session.scope().clone(),
                    session: self.retained_session.clone(),
                    commands: self.commands.clone(),
                };
                self.restart_recovery = Some(restart_recovery::start_first_pass(first_pass));
            }
            if self
                .publication_retry_at
                .is_some_and(|at| at <= tokio::time::Instant::now())
            {
                self.drain_publications().await?;
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
                    Effect::Callback(cursor, frame, actor) => {
                        self.answer_callback(cursor, frame, actor).await?
                    }
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
                _ = Shutdown::cancelled(&mut self.stop) => { self.closing = true; },
                _ = tokio::time::sleep_until(self.flush_at.unwrap_or_else(||tokio::time::Instant::now()+STREAM_CADENCE)), if self.flush_at.is_some() => self.flush().await?,
                _ = tokio::time::sleep_until(self.publication_retry_at.unwrap_or_else(||tokio::time::Instant::now()+STREAM_CADENCE)), if self.publication_retry_at.is_some() => self.drain_publications().await?,
                _ = tokio::time::sleep_until(self.retry_at.unwrap_or_else(|| tokio::time::Instant::now() + PREPARATION_RETRY)), if self.retry_at.is_some() => self.retry_preparations().await?,
                _ = tokio::time::sleep_until(self.auth_probe_at.unwrap_or_else(||tokio::time::Instant::now()+crate::claude_auth_runtime::PROBE_INTERVAL)), if self.auth_probe_at.is_some() && self.session.status.borrow().initialization == InitializationStatus::Ready => self.probe_auth_waits().await?,
                command = async { match self.queued_commands.pop_front() { Some(command) => Some(command), None => self.inbox.recv().await } }, if self.session.status.borrow().initialization == InitializationStatus::Ready => match command {
                    #[cfg(test)]
                    Some(AccountCommand::Barrier(reply)) => { let _ignored = reply.send(self.bridge.faults.journal_count.load(std::sync::atomic::Ordering::SeqCst)); },
                    Some(AccountCommand::VmNotification(command)) => self.vm_notification(*command).await?,
                    Some(AccountCommand::CommittedFrame(command)) => self.committed_frame(*command).await?,
                    Some(AccountCommand::RetainedRequest(command)) => self.retained_request(*command).await?,
                    Some(AccountCommand::JournalBarrier(command)) => self.retained_barrier(command).await?,
                    Some(AccountCommand::ConnectionRecovery { agent_id, reply }) => {
                        if self.connection_recovery_active {
                            let _sent = reply.send(Err(retained::PortError::Capacity));
                        } else {
                            self.connection_recovery_active = true;
                            let request = connection_recovery::Command {
                                bridge: self.bridge.clone(),
                                account: self.account.clone(),
                                scope: self.retained_session.scope().clone(),
                                session: self.retained_session.clone(),
                                agent_id,
                                runtime: tokio::runtime::Handle::current(),
                            };
                            let commands = self.commands.clone();
                            self.connection_recovery.spawn(async move {
                                let result = tokio::task::spawn_blocking(move || {
                                    connection_recovery::run(request)
                                }).await;
                                let (response, streams) = match result {
                                    Ok(Ok(completed)) => (Ok(completed.receipt), completed.streams),
                                    Ok(Err(_)) | Err(_) => (Err(retained::PortError::Unknown), None),
                                };
                                let (done, acknowledged) = tokio::sync::oneshot::channel();
                                if commands.send(AccountCommand::ConnectionRecoveryComplete {
                                    result: response,
                                    streams,
                                    reply: done,
                                }).await.is_ok() {
                                    let final_result = acknowledged.await.unwrap_or(Err(retained::PortError::Unknown));
                                    let _sent = reply.send(final_result);
                                } else {
                                    let _sent = reply.send(Err(retained::PortError::Unknown));
                                }
                            });
                        }
                    },
                    Some(AccountCommand::ConnectionRecoveryComplete { result, streams, reply }) => {
                        if let Some(streams) = streams {
                            self.streams = streams;
                            self.schedule_publication_drain();
                        }
                        let _sent = reply.send(result);
                    },
                    Some(AccountCommand::Wake { agent_id, epoch }) => self.wake(agent_id, epoch).await?,
                    Some(AccountCommand::Stop { agent_id }) => self.stop_agent(agent_id).await?,
                    Some(AccountCommand::RadioInterrupt { agent_id, thread_id, turn_id }) => {
                        self.interrupt(agent_id, thread_id, turn_id).await?
                    },
                    Some(AccountCommand::RestartRecoveryDone) => {
                        self.restart_recovery.take();
                        self.schedule_publication_drain();
                        #[cfg(test)]
                        {
                            self.bridge
                                .faults
                                .startup_recovery_done
                                .store(true, std::sync::atomic::Ordering::SeqCst);
                            self.bridge.faults.startup_recovery.notify_one();
                        }
                    },
                    Some(AccountCommand::RestartStreams(streams)) => {
                        self.streams = *streams;
                        self.schedule_publication_drain();
                    },
                    None => return Ok(()),
                },
                event = self.events.recv() => match event { Some(event) => self.event(event).await?, None => return Ok(()) },
                vm_event = self.vm_events.recv() => match vm_event {
                    Some((agent_id, actor_epoch, event)) => self.event_from(event, Some(&agent_id), Some(actor_epoch)).await?,
                    None => return Ok(()),
                },
                _ = self.connection_recovery.join_next(), if !self.connection_recovery.is_empty() => {
                    self.connection_recovery_active = false;
                },
                callback = self.managed_callbacks.join_next(), if !self.managed_callbacks.is_empty() => {
                    let (cursor, actor, response) = callback
                        .ok_or(WorkerError::TaskFailed)?
                        .map_err(|_| WorkerError::TaskFailed)?;
                    if let Some(response) = response {
                        self.push(Effect::Callback(cursor, response, actor))?;
                    }
                },
                changed = self.session.status.changed() => { if changed.is_err() { return Ok(()); } },
            }
            // A queued command bounds provider-lane work to the transaction in flight.
            if self.queued_commands.is_empty() {
                match self.inbox.try_recv() {
                    Ok(command) => self.queued_commands.push_back(command),
                    Err(mpsc::error::TryRecvError::Disconnected) => return Ok(()),
                    Err(mpsc::error::TryRecvError::Empty) => {
                        if let Ok(event) = self.events.try_recv() {
                            self.event(event).await?;
                        }
                    }
                }
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
    async fn db_identified<T: Send + 'static>(
        &mut self,
        name: &'static str,
        identity: String,
        operation: impl FnOnce(&RuntimeStore) -> Result<T, WorkerError> + Send + 'static,
    ) -> Result<T, WorkerError> {
        let bridge = self.bridge.clone();
        let job = bridge.worker_run_identified(name, Some(identity), operation);
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
                    Some(AccountCommand::VmNotification(command)) => self.vm_notification(*command).await?,
                    Some(command) => self.queued_commands.push_back(command),
                    None => { self.closing = true; self.control.cancel(); }
                },
            }
        }
    }
    // Session commands can wait behind event backpressure. Keep settling events
    // while waiting; the future is retained exactly once, including shutdown.
    async fn drive<T, E: Into<WorkerError>>(
        &mut self,
        future: impl Future<Output = Result<T, E>>,
    ) -> Result<T, WorkerError> {
        tokio::pin!(future);
        loop {
            tokio::select! { biased;
                _ = Shutdown::cancelled(&mut self.stop), if !self.closing => { self.closing = true; self.control.cancel(); },
                _ = tokio::time::sleep_until(self.flush_at.unwrap_or_else(||tokio::time::Instant::now()+STREAM_CADENCE)), if self.flush_at.is_some() => self.flush().await?,
                result = &mut future => return result.map_err(Into::into),
                event = self.events.recv(), if !self.closing => match event { Some(event) => self.event(event).await?, None => { self.closing=true; self.control.cancel(); } },
                vm_event = self.vm_events.recv(), if !self.closing => match vm_event {
                    Some((agent_id, actor_epoch, event)) => self.event_from(event, Some(&agent_id), Some(actor_epoch)).await?,
                    None => { self.closing=true; self.control.cancel(); }
                },
                command = self.inbox.recv(), if !self.closing && self.queued_commands.len() < COMMAND_CAPACITY => match command {
                    Some(AccountCommand::VmNotification(command)) => self.vm_notification(*command).await?,
                    Some(AccountCommand::RetainedRequest(command)) => Box::pin(self.retained_request(*command)).await?,
                    Some(AccountCommand::CommittedFrame(command)) => Box::pin(self.committed_frame(*command)).await?,
                    Some(AccountCommand::JournalBarrier(command)) => Box::pin(self.retained_barrier(command)).await?,
                    Some(AccountCommand::Stop { agent_id }) => self.stop_agent(agent_id).await?,
                    Some(command) => self.queued_commands.push_back(command),
                    None => { self.closing=true; self.control.cancel(); }
                },
            }
        }
    }
    /// Progress retained bridge requests while an owner waits for a reply from
    /// that same account worker (Claude's outbound account/read auth proof).
    async fn drive_retained<T>(
        &mut self,
        future: impl Future<Output = crate::continuation::Result<T>>,
    ) -> crate::continuation::Result<T> {
        tokio::pin!(future);
        loop {
            tokio::select! { biased;
                _ = Shutdown::cancelled(&mut self.stop), if !self.closing => {
                    self.closing = true;
                    self.control.cancel();
                    return Err(crate::continuation::ContinuationError::Unavailable("claude-auth/worker-stopped"));
                },
                _ = tokio::time::sleep_until(self.flush_at.unwrap_or_else(||tokio::time::Instant::now()+STREAM_CADENCE)), if self.flush_at.is_some() => {
                    self.flush().await.map_err(|_| crate::continuation::ContinuationError::Unavailable("claude-auth/worker-flush"))?;
                },
                result = &mut future => return result,
                event = self.events.recv(), if !self.closing => match event {
                    Some(event) => self.event(event).await.map_err(|_| crate::continuation::ContinuationError::Unavailable("claude-auth/worker-event"))?,
                    None => { self.closing=true; self.control.cancel(); }
                },
                vm_event = self.vm_events.recv(), if !self.closing => match vm_event {
                    Some((agent_id, actor_epoch, event)) => self.event_from(event, Some(&agent_id), Some(actor_epoch)).await
                        .map_err(|_| crate::continuation::ContinuationError::Unavailable("claude-auth/worker-event"))?,
                    None => { self.closing=true; self.control.cancel(); }
                },
                command = self.inbox.recv(), if !self.closing && self.queued_commands.len() < COMMAND_CAPACITY => match command {
                    Some(AccountCommand::VmNotification(command)) => self.vm_notification(*command).await.map_err(|_| crate::continuation::ContinuationError::Unavailable("claude-auth/vm-notification"))?,
                    Some(AccountCommand::RetainedRequest(command)) => self.retained_request(*command).await.map_err(|_| crate::continuation::ContinuationError::Unavailable("claude-auth/worker-request"))?,
                    Some(AccountCommand::Stop { agent_id }) => self.stop_agent(agent_id).await.map_err(|_| crate::continuation::ContinuationError::Unavailable("claude-auth/worker-stop"))?,
                    Some(command) => self.queued_commands.push_back(command),
                    None => { self.closing=true; self.control.cancel(); }
                },
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
    async fn probe_auth_waits(&mut self) -> Result<(), WorkerError> {
        let Some(driver) = self.resources.account_driver.clone() else {
            self.auth_probe_at = None;
            return Ok(());
        };
        if self.resources.claude_account_pin.get("provider") != Some(&json!("claude")) {
            self.auth_probe_at = None;
            return Ok(());
        }
        self.auth_probe_at =
            Some(tokio::time::Instant::now() + crate::claude_auth_runtime::PROBE_INTERVAL);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| WorkerError::TaskFailed)?
            .as_secs_f64();
        let account = self.account.clone();
        let waits = self
            .db("claude_auth_due_waits", move |store| {
                crate::claude_auth_runtime::due_waits(store, &account, now)
                    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
            })
            .await?;
        for (agent, epoch, settings) in waits {
            let _probe_slot = crate::claude_auth_runtime::acquire_probe_slot()
                .await
                .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
            let pin = self.resources.claude_account_pin.clone();
            let driver_for_probe = driver.clone();
            let account_for_probe = self.account.clone();
            let driver_for_pin = driver.clone();
            let account_for_pin = self.account.clone();
            let driver_after_probe = driver.clone();
            let account_after_probe = self.account.clone();
            let completed = crate::claude_auth_runtime::probe_due(
                self.bridge.store.clone(),
                &agent,
                &settings,
                &pin,
                now,
                move |_| async move {
                    let status = studio_runtime_process::blocking_pool::spawn_blocking(
                        "runtime-services/account_worker",
                        move || driver_for_probe.get(&account_for_probe),
                    )
                    .await
                    .ok()
                    .and_then(std::result::Result::ok);
                    let current = studio_runtime_process::blocking_pool::spawn_blocking(
                        "runtime-services/account_worker",
                        move || driver_for_pin.internal_record(&account_for_pin),
                    )
                    .await
                    .ok()
                    .and_then(std::result::Result::ok)
                    .unwrap_or(Value::Null);
                    if status == Some(crate::account_driver::AccountStatus::Ready) {
                        json!({
                            "status":"ready",
                            "accountId":current.get("accountId"),
                            "_credentialIdentity":current.get("_credentialIdentity")
                        })
                    } else {
                        json!({"status":"unavailable"})
                    }
                },
                || {
                    driver_after_probe
                        .auth_pin(&account_after_probe)
                        .unwrap_or(Value::Null)
                },
            )
            .await
            .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
            if completed {
                self.wake(agent, epoch).await?;
            }
        }
        Ok(())
    }
    async fn wake(&mut self, agent: String, epoch: i64) -> Result<(), WorkerError> {
        if self.closing || self.pending.len() >= MAX_PENDING_RESPONSES {
            return Ok(());
        }
        let candidate = loop {
            let account = self.account.clone();
            let claim_agent = agent.clone();
            match self
                .db("claim", move |store| {
                    store::claim(store, &account, &claim_agent, epoch)
                })
                .await
            {
                Ok(candidate) => break candidate,
                Err(error) if invalid_agent_record_error(&error) => {
                    let (invalid_id, pointer, reason) = invalid_agent_record_details(&error)
                        .expect("guard matches InvalidSyncObjectField");
                    let invalid_id = invalid_id.to_owned();
                    let pointer = pointer.to_owned();
                    let reason = reason.to_owned();
                    let was_requested_agent = invalid_id == agent;
                    let diagnostic = error.to_string();
                    self.db("hold_invalid_agent", move |store| {
                        store::hold_invalid_agent_record(
                            store,
                            &invalid_id,
                            &pointer,
                            &reason,
                            &diagnostic,
                        )
                    })
                    .await?;
                    if was_requested_agent {
                        return Ok(());
                    }
                }
                Err(error) => return Err(error),
            }
        };
        if let Some(intent) = candidate {
            self.submit(intent, Method::ThreadResume, None).await?;
        }
        Ok(())
    }
    async fn require_claude_auth(
        &mut self,
        driver: Arc<crate::account_driver::AccountDriver>,
        intent: &TurnIntent,
    ) -> Result<bool, WorkerError> {
        let port = self.retained_port.clone();
        crate::claude_auth_runtime::require_before_start(
            self.bridge.store.clone(),
            driver,
            self.resources.claude_account_pin.clone(),
            intent,
            |actor, deadline| {
                let port = port.clone();
                async move {
                    let request = crate::claude_auth_runtime::account_read(&port, actor, deadline);
                    self.drive_retained(request).await
                }
            },
        )
        .await
        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
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
        if method == Method::TurnStart && intent.provider == "claude" {
            let Some(driver) = self.resources.account_driver.clone() else {
                return Err(WorkerError::PreparationRefused(
                    "claude-auth/account-driver-unavailable".into(),
                ));
            };
            if !self.require_claude_auth(driver, &intent).await? {
                return Ok(());
            }
        }
        let turn = interrupt_turn.clone();
        let generation = self.generation.0;
        let connection_id = self.connection_id.clone();
        let preparation_connection_id = connection_id.clone();
        let prepared = if method == Method::TurnInterrupt {
            None
        } else {
            let i = intent.clone();
            let resources = self.resources.clone();
            let Some(selection) = self
                .prepare("preparation", move |s| {
                    if method == Method::TurnStart {
                        crate::account_transfer_executor::before_start(
                            s,
                            resources.account_session.clone(),
                            &resources.model_catalog,
                            &resources,
                            &i.agent_id,
                            now(),
                        )
                        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                    }
                    store::preparation_step(s, &i, || preparation::admit(s, &resources, &i))
                })
                .await?
            else {
                self.retry_at = Some(tokio::time::Instant::now() + PREPARATION_RETRY);
                return Ok(());
            };
            let i = intent.clone();
            let resources = self.resources.clone();
            let Some(token) = self
                .prepare("workspace", move |s| {
                    store::preparation_step(s, &i, || preparation::workspace(s, &resources, &i))
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
                            s,
                            &resources,
                            &i,
                            method,
                            generation,
                            &preparation_connection_id,
                            selection,
                            token,
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
        let actor_session = if matches!(
            method,
            Method::ThreadStart | Method::TurnStart | Method::TurnInterrupt | Method::ThreadResume
        ) {
            self.ensure_vm_session(&context.intent).await?
        } else {
            None
        };
        let control = actor_session
            .as_deref()
            .and_then(|agent| {
                self.vm_sessions
                    .get(agent)
                    .map(|session| session.control.clone())
            })
            .unwrap_or_else(|| self.control.clone());
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
                    &connection_id,
                )
            })
            .await?;
        if !authorized || self.closing {
            return Ok(());
        }
        self.pending.insert(id.clone(), context);
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

    /// Open a VM provider session for the durable actor at the turn boundary.
    /// The launch resolver is host-owned because it reads the account profile,
    /// guest provider configuration, and the minimal credential copy.
    async fn ensure_vm_session(
        &mut self,
        intent: &TurnIntent,
    ) -> Result<Option<String>, WorkerError> {
        let agent_id = intent.agent_id.clone();
        let lookup_id = agent_id.clone();
        let intent_cwd = intent.cwd.clone();
        let account_key = intent.account_key.clone();
        let provider = intent.provider.clone();
        let launch_request = self
            .db("vm_launch_request", move |store| {
                let connection = store.read()?;
                let Some(record) =
                    AgentRepository::get(&connection, &AgentId::from(lookup_id.as_str()))?
                else {
                    return Err(WorkerError::PreparationRefused("vm-agent/missing".into()));
                };
                if record.extra.get("executionMode").and_then(Value::as_str) != Some("vm") {
                    return Ok(None);
                }
                let cwd = record
                    .extra
                    .get("cwd")
                    .and_then(Value::as_str)
                    .or(intent_cwd.as_deref())
                    .filter(|value| value.starts_with('/'))
                    .ok_or_else(|| WorkerError::PreparationRefused("vm-agent/cwd".into()))?;
                let guest_home = record
                    .extra
                    .get("layrHome")
                    .and_then(Value::as_str)
                    .filter(|value| value.starts_with('/'))
                    .unwrap_or("/home/studio");
                Ok(Some(preparation::VmProviderLaunchRequest {
                    agent_id: record.id.0,
                    epoch: record.epoch,
                    account_key,
                    provider,
                    cwd: cwd.to_owned(),
                    guest_home: guest_home.to_owned(),
                }))
            })
            .await?;
        let Some(request) = launch_request else {
            return Ok(None);
        };
        if let Some(session) = self.vm_sessions.get(&request.agent_id) {
            if session.session.status.borrow().initialization == InitializationStatus::Ready {
                let bootstrap = session.bootstrap.clone();
                if let Some(payload) = bootstrap {
                    self.bootstrap_vm_provider(&request.agent_id, request.epoch, payload)
                        .await?;
                    if let Some(session) = self.vm_sessions.get_mut(&request.agent_id) {
                        session.bootstrap = None;
                    }
                }
                return Ok(Some(request.agent_id));
            }
        } else {
            let resolver = self.resources.vm_provider_launch.clone().ok_or_else(|| {
                WorkerError::PreparationPending("provider-session/vm-launch-resolver".into())
            })?;
            let guest = self.resources.guest_rpc.clone().ok_or_else(|| {
                WorkerError::PreparationPending("provider-session/vm-guest-unavailable".into())
            })?;
            self.resolving_vm_actors.insert(request.agent_id.clone());
            let resolved = self.drive(resolver.resolve(request.clone())).await;
            self.resolving_vm_actors.remove(&request.agent_id);
            let resolved = resolved?;
            let recovery_agent = request.agent_id.clone();
            let recovery_account = self.account.clone();
            let generation = self.generation.0;
            let recovered = self
                .db("vm_actor_recovery", move |store| {
                    store::recover_vm_actor_requests(
                        store,
                        &recovery_account,
                        &recovery_agent,
                        generation,
                    )
                })
                .await?;
            let factory = Arc::new(crate::linux_vm_provider::GuestProviderTaskFactory::new(
                Arc::clone(&self.bridge.store),
                guest,
                request.agent_id.clone(),
                request.epoch,
                resolved.provider.clone(),
                resolved.profile.clone(),
            ));
            let generation = self.generation;
            let clock = studio_runtime_process::provider_seam::SessionClock::default();
            let mut session = SessionHandle::open(
                factory,
                ProviderHandle::for_account(&request.agent_id),
                OpenOptions {
                    clock: clock.clone(),
                    launch: resolved.launch,
                    expected_generation: Some(generation),
                },
                SessionTaskConfig {
                    initialize_params: json!({"clientInfo":{"name":"codex_agents_canvas","version":"1.0.0"},"capabilities":{"experimentalApi":true}}),
                    generation,
                    claude: resolved.provider == "claude",
                    timeout_ms: self.timeout_ms,
                    initialize_operation: OperationId(format!(
                        "vm:{}:initialize:{}:{}",
                        request.agent_id, request.epoch, generation.0
                    )),
                    request_id_prefix: None,
                    recovered: recovered
                        .iter()
                        .map(|(_, request)| request.clone())
                        .collect(),
                },
            )
            .await?;
            for (context, request) in recovered {
                self.pending.insert(request.ticket.id.clone(), context);
            }
            let control = session.control.clone();
            let mut events = std::mem::replace(&mut session.events, mpsc::channel(1).1);
            let actor = request.agent_id.clone();
            let actor_epoch = request.epoch;
            let sender = self.vm_event_tx.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    if sender.send((actor.clone(), actor_epoch, event)).await.is_err() {
                        break;
                    }
                }
            });
            self.vm_event_forwarders.push(forwarder);
            self.vm_sessions.insert(
                request.agent_id.clone(),
                ActorProviderSession {
                    session,
                    epoch: request.epoch,
                    control,
                    bootstrap: resolved.bootstrap,
                },
            );
        }
        let mut status = self
            .vm_sessions
            .get(&request.agent_id)
            .ok_or(WorkerError::Closed)?
            .session
            .status
            .clone();
        while status.borrow().initialization != InitializationStatus::Ready {
            tokio::select! {
                _ = Shutdown::cancelled(&mut self.stop) => return Err(WorkerError::Closed),
                changed = status.changed() => changed.map_err(|_| WorkerError::Closed)?,
                event = self.vm_events.recv() => match event {
                    Some((actor, actor_epoch, event)) => self.event_from(event, Some(&actor), Some(actor_epoch)).await?,
                    None => return Err(WorkerError::Closed),
                },
                command = self.inbox.recv(), if self.queued_commands.len() < COMMAND_CAPACITY => match command {
                    Some(AccountCommand::VmNotification(command)) => self.vm_notification(*command).await?,
                    Some(command) => self.queued_commands.push_back(command),
                    None => return Err(WorkerError::Closed),
                },
            }
        }
        let bootstrap = self
            .vm_sessions
            .get(&request.agent_id)
            .and_then(|session| session.bootstrap.clone());
        if let Some(payload) = bootstrap {
            self.bootstrap_vm_provider(&request.agent_id, request.epoch, payload)
                .await?;
            if let Some(session) = self.vm_sessions.get_mut(&request.agent_id) {
                session.bootstrap = None;
            }
        }
        Ok(Some(request.agent_id))
    }
    async fn bootstrap_vm_provider(
        &mut self,
        agent_id: &str,
        epoch: i64,
        payload: Value,
    ) -> Result<(), WorkerError> {
        let signature = studio_runtime_store::sha256_hex(payload.to_string().as_bytes());
        let operation = format!("vm-auth-bootstrap:{agent_id}:{epoch}:{}", &signature[..24]);
        let admission_operation = operation.clone();
        let admission_signature = signature.clone();
        if self
            .db("vm_auth_bootstrap_admission", move |store| {
                store::vm_bootstrap_admission(store, &admission_operation, &admission_signature)
            })
            .await?
        {
            return Ok(());
        }
        let control = self.control_for_actor(Some(agent_id))?;
        let request_id = RequestId::String(operation.clone());
        let prepared = control
            .prepare(RequestSpec {
                id: request_id,
                operation_id: OperationId(operation.clone()),
                method: Method::AccountLoginStart,
                params: payload,
                thread_id: None,
                lightweight: false,
                timeout_ms: self.timeout_ms,
            })
            .await?;
        let (reply, mut response) = tokio::sync::oneshot::channel();
        self.vm_request_waiters.insert(operation.clone(), reply);
        if let Err(error) = self.drive(control.submit(prepared)).await {
            self.vm_request_waiters.remove(&operation);
            return Err(error);
        }
        let outcome = loop {
            tokio::select! {
                _ = Shutdown::cancelled(&mut self.stop) => return Err(WorkerError::Closed),
                response = &mut response => break response.map_err(|_| WorkerError::CallbackUnknown)?,
                event = self.vm_events.recv() => match event {
                    Some((actor, actor_epoch, event)) => self.event_from(event, Some(&actor), Some(actor_epoch)).await?,
                    None => return Err(WorkerError::Closed),
                },
                event = self.events.recv() => self.event(event.ok_or(WorkerError::Closed)?).await?,
                command = self.inbox.recv(), if self.queued_commands.len() < COMMAND_CAPACITY => match command {
                    Some(AccountCommand::VmNotification(command)) => self.vm_notification(*command).await?,
                    Some(command) => self.queued_commands.push_back(command),
                    None => return Err(WorkerError::Closed),
                },
            }
        };
        match outcome {
            RequestOutcome::Reply {
                body: ResponseBody::Result(_),
                ..
            } => {
                let settled_operation = operation.clone();
                let settled_signature = signature.clone();
                self.db("vm_auth_bootstrap_settled", move |store| {
                    store::vm_bootstrap_settled(store, &settled_operation, &settled_signature)
                })
                .await
            }
            RequestOutcome::Reply {
                body: ResponseBody::Error(_),
                ..
            }
            | RequestOutcome::Unknown => Err(WorkerError::CallbackUnknown),
        }
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
        #[cfg(test)]
        if event.outcomes.is_empty()
            && event.journal.is_some()
            && self
                .bridge
                .faults
                .hold_journal_batch
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            self.bridge.faults.journal_batch_ready.notify_one();
            self.bridge.faults.release_journal_batch.notified().await;
        }
        let mut ready = VecDeque::from([event]);
        while let Some(event) = ready.pop_front() {
            if event.outcomes.is_empty() && event.journal.is_some() {
                let mut entries = vec![event.journal.expect("checked journal")];
                while entries.len() < JOURNAL_BATCH_MAX_EVENTS {
                    if !self.queued_commands.is_empty() {
                        break;
                    }
                    match self.inbox.try_recv() {
                        Ok(command) => {
                            self.queued_commands.push_back(command);
                            break;
                        }
                        Err(mpsc::error::TryRecvError::Disconnected) => return Ok(()),
                        Err(mpsc::error::TryRecvError::Empty) => {}
                    }
                    let next = ready.pop_front().or_else(|| self.events.try_recv().ok());
                    let Some(next) = next else { break };
                    if next.outcomes.is_empty() {
                        if let Some(entry) = next.journal {
                            entries.push(entry);
                        } else {
                            ready.push_front(next);
                            break;
                        }
                    } else {
                        ready.push_front(next);
                        break;
                    }
                }
                self.journal_batch(entries, None, None, true).await?;
                self.complete_retained().await?;
            } else {
                self.event_one(event, None).await?;
            }
        }
        Ok(())
    }

    async fn event_from(
        &mut self,
        event: SessionEvent,
        actor_id: Option<&str>,
        source_epoch: Option<i64>,
    ) -> Result<(), WorkerError> {
        let Some(actor_id) = actor_id else {
            return Ok(());
        };
        let Some(source_epoch) = source_epoch else {
            return Ok(());
        };
        let Some(current_epoch) = self.vm_sessions.get(actor_id).map(|session| session.epoch) else {
            return Ok(());
        };
        if current_epoch != source_epoch {
            return Ok(());
        }
        match self
            .event_one(event, Some(actor_id.to_owned()), Some(source_epoch))
            .await
        {
            Err(WorkerError::StaleVmActor) => {
                self.retire_vm_actor(actor_id);
                Ok(())
            }
            result => result,
        }
    }

    async fn event_one(
        &mut self,
        event: SessionEvent,
        actor_id: Option<String>,
        actor_epoch: Option<i64>,
    ) -> Result<(), WorkerError> {
        let reclassify = !event.outcomes.is_empty();
        for outcome in event.outcomes {
            if actor_id.is_some()
                && matches!(&outcome, SessionOutcome::GenerationClosed { .. })
            {
                if let Some(actor) = actor_id.as_deref() {
                    self.retire_vm_actor(actor);
                }
                continue;
            }
            if let SessionOutcome::Resolved { ticket, outcome } = &outcome {
                let operation = store::id_string(&ticket.id);
                if let Some(waiter) = self.vm_request_waiters.remove(&operation) {
                    let _sent = waiter.send(outcome.clone());
                }
            }
            if let SessionOutcome::Write {
                operation_id,
                ticket: None,
                outcome: result,
                ..
            } = &outcome
                && self
                    .retained_frame_outcome(&operation_id.0, *result)
                    .await?
            {
                continue;
            }
            if self
                .retained_outcome(
                    &outcome,
                    event.journal.as_ref().map_or(0, |e| e.cursor.0),
                    actor_id.as_deref(),
                )
                .await?
            {
                continue;
            }
            if let SessionOutcome::Write {
                operation_id,
                ticket: None,
                outcome,
                ..
            } = &outcome
                && let Some((cursor, callback_actor)) = self.callbacks.remove(&operation_id.0)
            {
                let operation = operation_id.0.clone();
                let written = *outcome == WriteOutcome::Written;
                let write_outcome = *outcome;
                let identity = operation.clone();
                self.db_identified("callback_written", identity, move |store| {
                    store::callback_written(store, &operation, write_outcome)
                })
                .await?;
                if !written {
                    return Err(WorkerError::CallbackUnknown);
                }
                let journal_scope = self.journal_scope(callback_actor.as_deref());
                let generation = self.generation.0;
                self.db("callback_settled", move |store| {
                    store::callback_settled(store, &journal_scope, generation, cursor.0)
                })
                .await?;
                self.managed_callback_cursors
                    .remove(&(callback_actor.clone(), cursor.0));
                if !self.closing {
                    self.control_for_actor(callback_actor.as_deref())?
                        .settled(cursor)
                        .await?;
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
            // Pending bookkeeping and transport request IDs can change when a
            // journal lane retires a ticket. Keep the crash witness stable.
            let identity = match &outcome {
                SessionOutcome::Write {
                    ticket: Some(ticket),
                    ..
                }
                | SessionOutcome::Resolved { ticket, .. } => {
                    format!(
                        "generation={};request-method={}",
                        ticket.generation,
                        ticket.method.as_str(),
                    )
                }
                SessionOutcome::Write { ticket: None, .. } => {
                    format!("generation={};request-method=unknown", self.generation.0)
                }
                SessionOutcome::GenerationClosed { generation } => {
                    format!("generation-closed={generation}")
                }
            };
            let account = self.account.clone();
            let resources = self.resources.clone();
            let (retire, effect) = self
                .db_identified("outcome", identity, move |store| {
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
            self.journal(entry, actor_id.as_deref(), actor_epoch).await?;
        }
        if reclassify {
            for _ in 0..self.deferred.len() {
                let Some((entry, deferred_actor, deferred_epoch)) = self.deferred.pop_front() else {
                    continue;
                };
                if deferred_actor
                    .as_deref()
                    .is_some_and(|actor| !self.vm_sessions.contains_key(actor))
                {
                    continue;
                }
                match self
                    .journal(entry, deferred_actor.as_deref(), deferred_epoch)
                    .await
                {
                    Err(WorkerError::StaleVmActor) => {
                        if let Some(actor) = deferred_actor.as_deref() {
                            self.retire_vm_actor(actor);
                        }
                    }
                    result => result?,
                }
            }
        }
        self.complete_retained().await?;
        Ok(())
    }
    fn retire_vm_actor(&mut self, actor_id: &str) {
        if let Some(session) = self.vm_sessions.remove(actor_id) {
            session.control.cancel();
        }
    }

    fn control_for_actor(&self, actor_id: Option<&str>) -> Result<SessionControl, WorkerError> {
        match actor_id {
            Some(actor) => self
                .vm_sessions
                .get(actor)
                .map(|session| session.control.clone())
                .ok_or(WorkerError::Closed),
            None => Ok(self.control.clone()),
        }
    }
    fn control_status(&self, actor_id: Option<&str>) -> Result<InitializationStatus, WorkerError> {
        match actor_id {
            Some(actor) => self
                .vm_sessions
                .get(actor)
                .map(|session| session.session.status.borrow().initialization)
                .ok_or(WorkerError::Closed),
            None => Ok(self.session.status.borrow().initialization),
        }
    }
    fn journal_scope(&self, actor_id: Option<&str>) -> String {
        actor_id.map_or_else(
            || self.account.clone(),
            |actor| format!("{}:vm:{actor}", self.account),
        )
    }
    async fn journal_with_policy(
        &mut self,
        entry: JournalEntry,
        actor_id: Option<&str>,
        settle: bool,
        actor_epoch: Option<i64>,
    ) -> Result<bool, WorkerError> {
        self.journal_batch(vec![entry], actor_id, actor_epoch, settle).await
    }
    async fn journal(
        &mut self,
        entry: JournalEntry,
        actor_id: Option<&str>,
        actor_epoch: Option<i64>,
    ) -> Result<(), WorkerError> {
        self.journal_batch(vec![entry], actor_id, actor_epoch, true)
            .await
            .map(|_| ())
    }
    async fn journal_batch(
        &mut self,
        entries: Vec<JournalEntry>,
        actor_id: Option<&str>,
        actor_epoch: Option<i64>,
        settle: bool,
    ) -> Result<bool, WorkerError> {
        #[cfg(test)]
        self.bridge
            .faults
            .journal_transactions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let account = self.account.clone();
        let journal_scope = self.journal_scope(actor_id);
        let streams = std::mem::take(&mut self.streams);
        let claude_account_pin = self.resources.claude_account_pin.clone();
        let expected_vm_actor = actor_id
            .zip(actor_epoch)
            .map(|(actor, epoch)| (actor.to_owned(), epoch));
        let (previous, committed) = self
            .db("journal", move |store| {
                let result = store::journal_batch(
                    store,
                    &account,
                    &journal_scope,
                    &streams,
                    entries,
                    &claude_account_pin,
                    expected_vm_actor
                        .as_deref()
                        .map(|(actor, epoch)| (actor, *epoch)),
                );
                Ok((streams, result))
            })
            .await?;
        self.streams = previous;
        let Some((streams, committed, deferred)) = committed? else {
            return Err(WorkerError::TaskFailed);
        };
        if self.deferred.len().saturating_add(deferred.len()) > MAX_UNSETTLED {
            return Err(WorkerError::Capacity);
        }
        let had_deferred = !deferred.is_empty();
        self.deferred.extend(deferred.into_iter().map(|entry| {
            (entry, actor_id.map(str::to_owned), actor_epoch)
        }));
        #[cfg(test)]
        self.bridge
            .faults
            .journal_count
            .fetch_add(committed.len(), std::sync::atomic::Ordering::SeqCst);
        self.streams = streams; // commit precedes buffer replacement and ACK
        self.arm_flush();
        self.schedule_publication_drain();
        self.drain_publications().await?;
        let committed_any = !committed.is_empty();
        for (_index, (entry, effect)) in committed.into_iter().enumerate() {
            #[cfg(test)]
            if self
                .bridge
                .faults
                .fail_journal_ack_at
                .lock()
                .unwrap()
                .take_if(|fail_at| *fail_at == _index)
                .is_some()
            {
                return Err(WorkerError::TaskFailed);
            }
            self.journal_committed(entry, effect, actor_id, settle).await?;
        }
        Ok(committed_any && !had_deferred)
    }
    async fn journal_committed(
        &mut self,
        entry: JournalEntry,
        effect: Option<Effect>,
        actor_id: Option<&str>,
        settle: bool,
    ) -> Result<(), WorkerError> {
        let cursor = entry.cursor;
        let completed = match &entry.payload {
            JournalPayload::Frame(Frame::Notification(frame)) => {
                frame.method.as_str() == "turn/completed"
            }
            JournalPayload::Frame(Frame::UnknownMethod(frame))
                if !frame.fields.contains_key("id") =>
            {
                frame.method.as_str() == "turn/completed"
            }
            _ => false,
        };
        if completed {
            self.bridge.continuation_wake().notify_one();
        }
        if let Some(effect) = effect {
            self.push(effect)?;
        }
        if let JournalPayload::Frame(frame) = &entry.payload
            && let Frame::Notification(notification) = frame
            && notification.method.as_str() == "command/exec/outputDelta"
        {
            self.bridge.monitor_dispatch_wake().notify_one();
        }
        if let JournalPayload::Frame(frame) = &entry.payload
            && let Frame::Request(request) = frame
            && request.method.as_str() == "item/tool/call"
        {
            let account = self.account.clone();
            let generation = self.generation.0;
            let actor = actor_id.map(str::to_owned);
            let now = entry.received_at_ms as f64 / 1000.0;
            let frame = frame.clone();
            let wake = self.bridge.event_wake();
            let monitor_dispatch_wake = self.bridge.monitor_dispatch_wake();
            if self.callbacks.len() + self.managed_callbacks.len() >= MAX_PENDING_RESPONSES {
                return Err(WorkerError::Capacity);
            }
            let store = Arc::clone(&self.bridge.store);
            let managed_tool_runtime = self.bridge.managed_tool_runtime();
            self.managed_callback_cursors
                .insert((actor.clone(), cursor.0));
            self.managed_callbacks.spawn(async move {
                if let Err(dependency) = callback::wait_for_managed_catalog(
                    &store,
                    &account,
                    generation,
                    actor.as_deref(),
                    &frame,
                    managed_tool_runtime.as_deref(),
                )
                .await
                {
                    let Frame::Request(request) = &frame else {
                        return (cursor, actor, None);
                    };
                    return (
                        cursor,
                        actor,
                        Some(callback::tool_call_failure(request, dependency)),
                    );
                }
                let response = studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-services/account_callback",
                    move || {
                        callback::managed_response(
                            &store,
                            &account,
                            generation,
                            actor.as_deref(),
                            &frame,
                            now,
                            wake,
                            monitor_dispatch_wake,
                            managed_tool_runtime,
                        )
                    },
                )
                .await
                .ok()
                .flatten();
                (cursor, actor, response)
            });
        } else if let JournalPayload::Frame(frame) = &entry.payload
            && let Some(response) = callback::response(frame)
        {
            self.push(Effect::Callback(
                cursor,
                response,
                actor_id.map(str::to_owned),
            ))?;
        } else if settle && !self.closing {
            let control = self.control_for_actor(actor_id)?;
            if control.settled(cursor).await.is_err() {
                self.closing = true;
                control.cancel();
            }
        }
        if let Some(actor_id) = actor_id
            && let Some(reply) = self
                .vm_notification_waiters
                .remove(&(actor_id.to_owned(), cursor.0))
        {
            let _sent = reply.send(Ok(()));
        }
        Ok(())
    }
    async fn vm_notification(
        &mut self,
        command: retained::VmNotificationCommand,
    ) -> Result<(), WorkerError> {
        let retained::VmNotificationCommand {
            scope,
            agent_id,
            epoch,
            sequence,
            frame,
            reply,
        } = command;
        if scope.account != self.account
            || scope.connection != self.connection_id
            || scope.generation != self.generation.0
            || sequence == 0
            || epoch < 0
        {
            let _sent = reply.send(Err(retained::PortError::Identity));
            return Ok(());
        }
        let entry = JournalEntry {
            received_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| WorkerError::TaskFailed)?
                .as_millis()
                .try_into()
                .map_err(|_| WorkerError::TaskFailed)?,
            cursor: studio_runtime_process::EventCursor(sequence),
            generation: self.generation,
            payload: JournalPayload::Frame(frame),
        };
        let key = (agent_id.clone(), sequence);
        match self
            .journal_with_policy(entry, Some(&agent_id), false, Some(epoch))
            .await
        {
            Ok(true) => {
                let _sent = reply.send(Ok(()));
            }
            Ok(false) => {
                if self.vm_notification_waiters.contains_key(&key) {
                    let _sent = reply.send(Err(retained::PortError::Capacity));
                } else {
                    self.vm_notification_waiters.insert(key, reply);
                }
            }
            Err(_) => {
                let _sent = reply.send(Err(retained::PortError::Unknown));
            }
        }
        Ok(())
    }
    fn journal_scope(&self, actor_id: Option<&str>) -> String {
        actor_id.map_or_else(
            || self.account.clone(),
            |actor| format!("{}:vm:{actor}", self.account),
        )
    }
    async fn answer_callback(
        &mut self,
        cursor: studio_runtime_process::EventCursor,
        frame: Frame,
        actor_id: Option<String>,
    ) -> Result<(), WorkerError> {
        while self.control_status(actor_id.as_deref())? != InitializationStatus::Ready {
            tokio::select! { biased;
                _ = Shutdown::cancelled(&mut self.stop) => { self.closing = true; return Ok(()); },
                event = self.events.recv(), if actor_id.is_none() => { self.event(event.ok_or(WorkerError::Closed)?).await?; },
                event = self.vm_events.recv(), if actor_id.is_some() => {
                    match event { Some((actor, actor_epoch, event)) => self.event_from(event, Some(&actor), Some(actor_epoch)).await?, None => return Err(WorkerError::Closed) }
                },
                command = self.inbox.recv(), if self.queued_commands.len() < COMMAND_CAPACITY => match command {
                    Some(AccountCommand::VmNotification(command)) => self.vm_notification(*command).await?,
                    Some(command) => self.queued_commands.push_back(command),
                    None => return Err(WorkerError::Closed),
                },
            }
        }
        let control = self.control_for_actor(actor_id.as_deref())?;
        let journal_scope = self.journal_scope(actor_id.as_deref());
        let operation = if actor_id.is_some() {
            format!(
                "callback:{journal_scope}:{}:{}",
                self.generation.0, cursor.0
            )
        } else {
            format!(
                "callback:{}:{}:{}",
                self.account, self.generation.0, cursor.0
            )
        };
        let prepared = self
            .control_for_actor(actor_id.as_deref())?
            .prepare_frame(OperationId(operation.clone()), &frame)?;
        let key = operation.clone();
        let generation = self.generation.0;
        let admission = self
            .db("callback_admission", move |store| {
                store::callback_admission(store, &key, &journal_scope, generation, cursor.0)
            })
            .await?;
        if admission {
            control.settled(cursor).await?;
            self.managed_callback_cursors
                .remove(&(actor_id.clone(), cursor.0));
            return Ok(());
        }
        if self.callbacks.len() >= MAX_PENDING_RESPONSES {
            return Err(WorkerError::Capacity);
        }
        self.callbacks.insert(operation.clone(), (cursor, actor_id));
        if let Err(error) = self.drive(control.submit_frame(prepared)).await {
            if definite_rejection(&error) {
                self.callbacks.remove(&operation);
                let identity = operation.clone();
                self.db_identified("callback_written", identity, move |store| {
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
        let flush = move |store: &RuntimeStore| store::flush_streams(store, &account, streams);
        #[cfg(test)]
        let updated_streams = self
            .db_identified(
                "flush_streams",
                format!(
                    "account={};streams={}",
                    self.account,
                    self.streams.boundary_identity()
                ),
                flush,
            )
            .await?;
        #[cfg(not(test))]
        let updated_streams = self.db("flush_streams", flush).await?;
        self.streams = updated_streams;
        self.flush_at = None;
        self.arm_flush();
        self.schedule_publication_drain();
        self.drain_publications().await?;
        Ok(())
    }
    fn schedule_publication_drain(&mut self) {
        if self.publication_retry_at.is_none() {
            self.publication_retry_at = Some(tokio::time::Instant::now());
        }
    }
    async fn drain_publications(&mut self) -> Result<(), WorkerError> {
        if self
            .publication_retry_at
            .is_some_and(|at| at > tokio::time::Instant::now())
        {
            return Ok(());
        }
        let account = self.account.clone();
        let sink = Arc::clone(&self.publication_sink);
        match self
            .db("drain_publications", move |store| {
                store::drain_publications(store, &account, sink.as_ref())
            })
            .await
        {
            Ok(count) => {
                self.publication_retry_at =
                    (count == store::PUBLICATION_DRAIN_LIMIT).then(tokio::time::Instant::now);
                Ok(())
            }
            Err(WorkerError::Notification(_)) => {
                // A failed sink leaves its receipt in place. Retry on a fixed
                // cadence so a broken consumer cannot spin the account worker.
                self.publication_retry_at =
                    Some(tokio::time::Instant::now() + PUBLICATION_RETRY_DELAY);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
    async fn drain_managed_callbacks(&mut self) -> Result<(), WorkerError> {
        #[cfg(test)]
        self.bridge.faults.shutdown_callbacks_draining.notify_one();
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        let completed = tokio::time::timeout_at(deadline, async {
            while let Some(callback) = self.managed_callbacks.join_next().await {
                match callback {
                    Ok((cursor, actor, Some(response))) => {
                        self.push(Effect::Callback(cursor, response, actor))?;
                    }
                    Ok((_, _, None)) => {}
                    Err(error) => {
                        eprintln!(
                            "managed account callback task failed during shutdown; account={} generation={} cursors={:?}: {error}",
                            self.account, self.generation.0, self.managed_callback_cursors
                        );
                    }
                }
            }
            Ok::<(), WorkerError>(())
        })
        .await;
        match completed {
            Ok(result) => result?,
            Err(_) => {
                eprintln!(
                    "managed account callbacks did not finish before shutdown; account={} generation={} cursors={:?}",
                    self.account, self.generation.0, self.managed_callback_cursors
                );
            }
        }

        // Use the same callback admission and write path as normal serving.
        let mut remaining = VecDeque::new();
        while let Some(effect) = self.effects.pop_front() {
            match effect {
                Effect::Callback(cursor, response, actor) => {
                    self.answer_callback(cursor, response, actor).await?;
                }
                effect => remaining.push_back(effect),
            }
        }
        self.effects = remaining;

        let written = tokio::time::timeout_at(deadline, async {
            while !self.callbacks.is_empty() {
                let event = self.events.recv().await.ok_or(WorkerError::Closed)?;
                self.event(event).await?;
            }
            Ok::<(), WorkerError>(())
        })
        .await;
        match written {
            Ok(result) => result?,
            Err(_) => eprintln!(
                "managed callback responses were not confirmed written before shutdown; account={} generation={} cursors={:?}",
                self.account,
                self.generation.0,
                self.callbacks.values().collect::<Vec<_>>()
            ),
        }
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
        for (_, reply) in std::mem::take(&mut self.vm_notification_waiters) {
            let _sent = reply.send(Err(retained::PortError::Unknown));
        }
        self.flush().await?;
        let account = self.account.clone();
        let generation = self.generation.0;
        let cleanup_error = exit.adapter.as_ref().and_then(|a| a.cleanup_error.clone());
        let identity = format!("account={account};generation={generation}");
        let recovery = exit.adapter.and_then(|a| a.recovery_needed);
        self.db_identified("finish", identity, move |store| {
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

fn invalid_agent_record_error(error: &WorkerError) -> bool {
    invalid_agent_record_details(error).is_some()
}
fn invalid_agent_record_details(error: &WorkerError) -> Option<(&str, &str, &str)> {
    let invalid = match error {
        WorkerError::Store(StoreError::InvalidSyncObjectField {
            scope,
            id,
            pointer,
            reason,
        }) if scope == "runtime_agents" => Some((id.as_str(), pointer.as_str(), reason.as_str())),
        WorkerError::Scheduler(scheduler::SchedulerError::Store(
            StoreError::InvalidSyncObjectField {
                scope,
                id,
                pointer,
                reason,
            },
        )) if scope == "runtime_agents" => Some((id.as_str(), pointer.as_str(), reason.as_str())),
        _ => None,
    };
    invalid
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
    connection_id: &str,
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
        connection_id,
        intent.thread_id.as_deref().unwrap_or(""),
    )? {
        return Ok(false);
    }
    crate::notification_dispatch::record_user_submission(tx, &intent.agent_id, submission)?;
    Ok(true)
}
