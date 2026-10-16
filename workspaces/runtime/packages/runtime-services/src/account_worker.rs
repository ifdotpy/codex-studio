//! One Tokio task owns an account. SQLite writes are submitted to RuntimeStore's
//! bounded actor; dropping a handle signals shutdown and drains an in-flight job.
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
    AgentId, AgentRepository, ObservedTable, RuntimeStore, SchedulerRepository, StoreError,
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

fn monotonic_deadline(deadline: SystemTime) -> Result<tokio::time::Instant, WorkerError> {
    let delay = deadline
        .duration_since(SystemTime::now())
        .unwrap_or_default();
    tokio::time::Instant::now()
        .checked_add(delay)
        .ok_or(WorkerError::TaskFailed)
}

fn vm_refresh_deadline_after_session_change(
    had_active_session: bool,
    has_active_session: bool,
    has_resolver: bool,
    current_deadline: Option<tokio::time::Instant>,
    now: tokio::time::Instant,
) -> Option<tokio::time::Instant> {
    if !has_active_session || !has_resolver {
        None
    } else if !had_active_session {
        Some(now)
    } else {
        current_deadline
    }
}

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

fn vm_credential_maintenance_error(error: &WorkerError) -> Option<&'static str> {
    match error {
        WorkerError::PreparationPending(_) => Some("pending"),
        WorkerError::PreparationRefused(_) => Some("stale-or-unavailable"),
        _ => None,
    }
}

fn vm_account_generation_matches(
    resources: &preparation::PreparationResources,
    request: &preparation::VmProviderLaunchRequest,
) -> bool {
    resources
        .account_session
        .as_deref()
        .and_then(|session| session.current_scope(&request.account_key))
        .is_some_and(|scope| scope.generation == request.generation)
}

/// Keep legacy synchronous raw reads off Tokio workers while the dedicated
/// async reader migration remains outstanding. This helper must only wrap
/// read-only operations; all writes use RuntimeStore's async actor API.
pub(super) async fn blocking_store_read<T: Send + 'static>(
    store: Arc<RuntimeStore>,
    operation: impl FnOnce(&RuntimeStore) -> Result<T, WorkerError> + Send + 'static,
) -> Result<T, WorkerError> {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_worker_read",
        move || operation(&store),
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)?
}

/// Share one bridge among all account workers of a runtime. This permit orders
/// complete logical account operations; SQLite admission and callback lifetime
/// are owned by RuntimeStore's actor.
#[derive(Clone)]
pub struct StoreBridge {
    store: Arc<RuntimeStore>,
    operation_slot: Arc<Semaphore>,
    preparation_slots: Arc<Semaphore>,
    accounts: Arc<std::sync::Mutex<BTreeSet<String>>>,
    event_wake: Arc<std::sync::RwLock<Arc<Notify>>>,
    continuation_wake: Arc<std::sync::RwLock<Arc<Notify>>>,
    monitor_dispatch_wake: Arc<std::sync::RwLock<Arc<Notify>>>,
    auth_wait_generation: Arc<watch::Sender<u64>>,
    managed_tool_runtime:
        Arc<std::sync::RwLock<Option<Arc<crate::provider_callbacks::ManagedToolRuntime>>>>,
    #[cfg(test)]
    faults: Arc<tests::Faults>,
    #[cfg(test)]
    account_released: Arc<tokio::sync::Notify>,
}
impl StoreBridge {
    pub async fn new(store: Arc<RuntimeStore>) -> Result<Self, WorkerError> {
        let (auth_wait_generation, _) = watch::channel(0_u64);
        let auth_wait_generation = Arc::new(auth_wait_generation);
        let observed_generation = Arc::downgrade(&auth_wait_generation);
        // Workers share this generation channel; the weak callback remains
        // harmless after the bridge owner is dropped.
        store.add_table_change_observer(ObservedTable::UsageResumes, move || {
            if let Some(generation) = observed_generation.upgrade() {
                generation.send_modify(|value| *value = value.wrapping_add(1));
            }
        })?;
        let bridge = Self {
            store,
            operation_slot: Arc::new(Semaphore::new(1)),
            preparation_slots: Arc::new(Semaphore::new(2)),
            accounts: Arc::default(),
            event_wake: Arc::new(std::sync::RwLock::new(Arc::new(Notify::new()))),
            continuation_wake: Arc::new(std::sync::RwLock::new(Arc::new(Notify::new()))),
            monitor_dispatch_wake: Arc::new(std::sync::RwLock::new(Arc::new(Notify::new()))),
            auth_wait_generation,
            managed_tool_runtime: Arc::new(std::sync::RwLock::new(None)),
            #[cfg(test)]
            faults: Arc::default(),
            #[cfg(test)]
            account_released: Arc::default(),
        };
        bridge
            .run(|store| async move { Ok(scheduler::recover_on_startup(&store).await?) })
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
    fn auth_wait_changes(&self) -> watch::Receiver<u64> {
        self.auth_wait_generation.subscribe()
    }
    async fn run<T, F, Fut>(&self, operation: F) -> Result<T, WorkerError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<RuntimeStore>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, WorkerError>> + Send + 'static,
    {
        let permit = self
            .operation_slot
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| WorkerError::Closed)?;
        let result = operation(self.store.clone()).await;
        drop(permit);
        result
    }
    async fn prepare_run<T, F, Fut>(
        &self,
        name: &'static str,
        operation: F,
    ) -> Result<T, WorkerError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<RuntimeStore>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, WorkerError>> + Send + 'static,
    {
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
        let result = operation(self.store.clone()).await;
        drop(permit);
        #[cfg(test)]
        self.faults.after(index, result.is_ok())?;
        result
    }
    async fn worker_run<T, F, Fut>(
        &self,
        _name: &'static str,
        operation: F,
    ) -> Result<T, WorkerError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<RuntimeStore>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, WorkerError>> + Send + 'static,
    {
        self.worker_run_identified(_name, None, operation).await
    }
    async fn worker_run_identified<T, F, Fut>(
        &self,
        _name: &'static str,
        identity: Option<String>,
        operation: F,
    ) -> Result<T, WorkerError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<RuntimeStore>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, WorkerError>> + Send + 'static,
    {
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
    #[cfg(test)]
    session_control: SessionControl,
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
    #[cfg(test)]
    async fn session_journal_barrier(
        &self,
    ) -> Result<studio_runtime_process::EventCursor, SessionTaskError> {
        self.session_control.journal_barrier().await
    }
    pub fn try_radio_interrupt(
        &self,
        agent_id: String,
        thread_id: String,
        turn_id: String,
    ) -> Result<(), Box<mpsc::error::TrySendError<AccountCommand>>> {
        self.commands
            .try_send(AccountCommand::RadioInterrupt {
                agent_id,
                thread_id,
                turn_id,
            })
            .map_err(Box::new)
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
    ) -> Result<(), Box<mpsc::error::TrySendError<AccountCommand>>> {
        self.commands.try_send(command).map_err(Box::new)
    }
    /// Wait for one command slot without claiming it, so an owner can resume a
    /// durable queue scan when this worker drains its bounded command lane.
    pub fn wait_for_command_capacity(&self) -> impl Future<Output = bool> + Send + 'static {
        let commands = self.commands.clone();
        async move {
            match commands.reserve_owned().await {
                Ok(permit) => {
                    drop(permit);
                    true
                }
                Err(_) => false,
            }
        }
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
            .worker_run("recover", move |store| async move {
                store::recover(&store, &recovery_account, generation).await
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
                    .worker_run("hold_invalid_agent", move |store| async move {
                        store::hold_invalid_agent_record(
                            &store,
                            &invalid_id,
                            &pointer,
                            &reason,
                            &diagnostic,
                        )
                        .await
                    })
                    .await?;
            }
            Err(error) => return Err(error),
        }
    };
    config.recovered = recovered.iter().map(|(_, record)| record.clone()).collect();
    let recovery_account = account.clone();
    let retained = bridge
        .worker_run("recover_retained", move |s| async move {
            retained::recover(&s, &recovery_account, generation.0).await
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
        .worker_run("restore_effects_streams", move |store| async move {
            let read_account = effect_account.clone();
            let effects = blocking_store_read(store.clone(), move |store| {
                store::recover_effects(store, &read_account)
            })
            .await?;
            let streams = store::restore_streams(&store, &effect_account).await?;
            Ok((effects, streams))
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
    let is_claude_account = resources.claude_account_pin.get("provider") == Some(&json!("claude"));
    let auth_wait_at = is_claude_account.then(tokio::time::Instant::now);
    let vm_credential_refresh_at = None;
    // Subscribe before the worker's initial due/deadline snapshot. A commit
    // before this point is visible in that snapshot; later commits advance it.
    let auth_wait_changes = bridge.auth_wait_changes();
    let worker = Worker {
        resources,
        retained_port,
        bridge: bridge.clone(),
        account: account.clone(),
        auth_wait_at,
        auth_wait_changes,
        vm_credential_refresh_at,
        generation,
        connection_id,
        timeout_ms,
        control: session.control.clone(),
        session,
        events,
        vm_events,
        vm_event_tx,
        vm_sessions: BTreeMap::new(),
        retired_vm_sessions: BTreeMap::new(),
        vm_incarnation_counter: 0,
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
    #[cfg(test)]
    let session_control = worker.control.clone();
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
        match finished_tx.send(true) {
            Ok(()) => {}
            Err(_no_receivers) => {
                // The JoinHandle remains the completion owner when all watch receivers drop.
            }
        }
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
        #[cfg(test)]
        session_control,
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
    Callback(
        studio_runtime_process::EventCursor,
        Frame,
        Option<String>,
        Option<i64>,
    ),
    Submit(TurnIntent),
    Interrupt {
        agent: String,
        thread: String,
        turn: String,
    },
    Wake(String, i64),
}
type ManagedCallbackResult = (
    studio_runtime_process::EventCursor,
    Option<String>,
    Option<i64>,
    Option<Frame>,
);
type CallbackCursor = (
    studio_runtime_process::EventCursor,
    Option<String>,
    Option<i64>,
);
struct Worker {
    retained_requests: BTreeMap<RequestId, retained::PendingRequest>,
    retained_frames: BTreeMap<String, retained_frame::PendingFrame>,
    retained_barriers: Vec<retained::PendingBarrier>,
    resources: preparation::PreparationResources,
    retained_port: retained::RetainedSessionPort,
    bridge: StoreBridge,
    account: String,
    auth_wait_at: Option<tokio::time::Instant>,
    auth_wait_changes: watch::Receiver<u64>,
    vm_credential_refresh_at: Option<tokio::time::Instant>,
    generation: Generation,
    connection_id: String,
    timeout_ms: u64,
    control: SessionControl,
    session: SessionHandle,
    events: mpsc::Receiver<SessionEvent>,
    vm_events: mpsc::Receiver<VmForwardedMessage>,
    vm_event_tx: mpsc::Sender<VmForwardedMessage>,
    vm_sessions: BTreeMap<String, ActorProviderSession>,
    retired_vm_sessions: BTreeMap<(String, u64), ActorProviderSession>,
    vm_incarnation_counter: u64,
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
    deferred: VecDeque<(JournalEntry, Option<VmActorIdentity>)>,
    vm_notification_waiters:
        BTreeMap<(String, u64), tokio::sync::oneshot::Sender<Result<(), retained::PortError>>>,
    effects: VecDeque<Effect>,
    publication_sink: Arc<dyn crate::notification_runtime_effects::NotificationPostCommitSink>,
    publication_retry_at: Option<tokio::time::Instant>,
    closing: bool,
    callbacks: BTreeMap<String, CallbackCursor>,
    managed_callback_cursors: BTreeSet<(Option<String>, Option<i64>, u64)>,
    restart_recovery_started: bool,
    restart_recovery: Option<JoinHandle<()>>,
    connection_recovery: tokio::task::JoinSet<()>,
    connection_recovery_active: bool,
    managed_callbacks: JoinSet<ManagedCallbackResult>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VmActorIdentity {
    actor_id: String,
    epoch: i64,
    incarnation: u64,
}

enum VmForwardedMessage {
    Event {
        source: VmActorIdentity,
        event: SessionEvent,
    },
    Exit {
        source: VmActorIdentity,
    },
}

enum ServeSignal {
    Shutdown,
    Flush,
    PublicationRetry,
    PreparationRetry,
    AuthWait,
    VmCredentialRefresh,
    AuthWaitChanged(Result<(), watch::error::RecvError>),
    Command(Option<AccountCommand>),
    Event(Option<SessionEvent>),
    VmEvent(Option<VmForwardedMessage>),
    ConnectionRecovery,
    ManagedCallback(Option<Result<ManagedCallbackResult, tokio::task::JoinError>>),
    SessionStatusChanged(bool),
}

struct ActorProviderSession {
    session: SessionHandle,
    epoch: i64,
    incarnation: u64,
    control: SessionControl,
    launch_request: preparation::VmProviderLaunchRequest,
    bootstrap: Option<Value>,
}
impl Worker {
    async fn handle_vm_message(&mut self, message: VmForwardedMessage) -> Result<(), WorkerError> {
        match message {
            VmForwardedMessage::Event { source, event } => self.event_from(event, &source).await,
            VmForwardedMessage::Exit { source } => {
                let active = self
                    .vm_sessions
                    .get(&source.actor_id)
                    .is_some_and(|session| {
                        session.epoch == source.epoch && session.incarnation == source.incarnation
                    });
                let retired = self
                    .retired_vm_sessions
                    .contains_key(&(source.actor_id.clone(), source.incarnation));
                if !active && !retired {
                    return Ok(());
                }

                // The forwarder sends Exit on the same channel after the event
                // stream closes. FIFO therefore puts every delivered event ahead
                // of the joined task's remaining durable evidence.
                #[cfg(test)]
                self.bridge
                    .faults
                    .mark_shutdown_phase("joining VM actor session");
                let exit = if active {
                    self.vm_sessions
                        .get_mut(&source.actor_id)
                        .ok_or(WorkerError::StaleVmActor)?
                        .session
                        .completion()
                        .await
                } else {
                    self.retired_vm_sessions
                        .get_mut(&(source.actor_id.clone(), source.incarnation))
                        .ok_or(WorkerError::StaleVmActor)?
                        .session
                        .completion()
                        .await
                };
                #[cfg(test)]
                self.bridge
                    .faults
                    .mark_shutdown_phase("VM actor session joined");
                if let Some(recovery) = exit
                    .adapter
                    .as_ref()
                    .and_then(|adapter| adapter.recovery_needed.clone())
                {
                    let scope = self.journal_scope(Some(&source.actor_id));
                    self.db("vm_session_recovery", move |store| async move {
                        store::save_recovery(&store, &scope, &recovery).await
                    })
                    .await?;
                }
                for event in exit.undelivered {
                    match self.event_one(event, Some(&source), false).await {
                        Err(WorkerError::StaleVmActor) => self.retire_vm_actor_if_current(&source),
                        result => result?,
                    }
                }
                self.event_one(
                    SessionEvent {
                        journal: None,
                        outcomes: exit.outcomes,
                    },
                    Some(&source),
                    false,
                )
                .await?;

                // A final GenerationClosed normally retires the actor above.
                // Keep this recovery fallback for task exits whose final outcome
                // lane could not be applied; it remains scoped to this actor.
                if self
                    .vm_sessions
                    .get(&source.actor_id)
                    .is_some_and(|session| {
                        session.epoch == source.epoch && session.incarnation == source.incarnation
                    })
                {
                    let account = self.account.clone();
                    let generation = self.generation.0;
                    let disconnect_actor = source.actor_id.clone();
                    self.db("vm_actor_disconnected", move |store| async move {
                        store::disconnect_vm_actor(
                            &store,
                            &disconnect_actor,
                            source.epoch,
                            &account,
                            generation,
                        )
                        .await
                    })
                    .await?;
                    self.retire_vm_actor(&source.actor_id);
                }
                self.retired_vm_sessions
                    .remove(&(source.actor_id, source.incarnation));
                Ok(())
            }
        }
    }

    async fn run(mut self) -> Result<(), WorkerError> {
        #[cfg(test)]
        self.bridge.faults.mark_shutdown_phase("serving");
        let result = self.serve().await;
        #[cfg(test)]
        self.bridge.faults.mark_shutdown_phase("serve complete");
        #[cfg(test)]
        self.bridge
            .faults
            .mark_shutdown_phase("draining managed callbacks");
        let drain_result = self.drain_managed_callbacks().await;
        #[cfg(test)]
        self.bridge
            .faults
            .mark_shutdown_phase("managed callbacks drained");
        let result = result.and(drain_result);
        if let Some(recovery) = self.restart_recovery.take() {
            #[cfg(test)]
            self.bridge
                .faults
                .mark_shutdown_phase("joining restart recovery");
            recovery.abort();
            let _joined = recovery.await;
        }
        self.connection_recovery.abort_all();
        #[cfg(test)]
        self.bridge
            .faults
            .mark_shutdown_phase("joining connection recovery");
        while self.connection_recovery.join_next().await.is_some() {}
        self.control.cancel();
        #[cfg(test)]
        self.bridge
            .faults
            .mark_shutdown_phase("joining account session");
        let exit = self.session.completion().await;
        #[cfg(test)]
        self.bridge
            .faults
            .mark_shutdown_phase("account session joined");
        for session in self.vm_sessions.values() {
            session.control.cancel();
        }
        let mut vm_cleanup_error = None;
        let mut forwarders = std::mem::take(&mut self.vm_event_forwarders);
        while let Some(mut forwarder) = forwarders.pop() {
            #[cfg(test)]
            self.bridge
                .faults
                .mark_shutdown_phase("joining VM event forwarder");
            loop {
                tokio::select! {
                    _joined = &mut forwarder => break,
                    event = self.vm_events.recv() => match event {
                        Some(event) => {
                            let closing = self.closing;
                            self.closing = false;
                            if let Err(error) = self.handle_vm_message(event).await {
                                vm_cleanup_error.get_or_insert(error);
                            }
                            self.closing = closing;
                        }
                        None => break,
                    }
                }
            }
        }
        while let Ok(event) = self.vm_events.try_recv() {
            let closing = self.closing;
            self.closing = false;
            if let Err(error) = self.handle_vm_message(event).await {
                vm_cleanup_error.get_or_insert(error);
            }
            self.closing = closing;
        }
        // Completion has an independent lane. Drain bounded delivered and retained
        // evidence before persisting disconnect; abandoned cursors are never ACKed.
        #[cfg(test)]
        self.bridge
            .faults
            .mark_shutdown_phase("finishing account state");
        let cleanup = self.finish(exit).await;
        #[cfg(test)]
        self.bridge
            .faults
            .mark_shutdown_phase("account state finished");
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
                self.run_effect(effect).await?;
                continue;
            }
            #[cfg(test)]
            self.bridge.faults.cycle();
            let signal = self.next_serve_signal().await;
            if !self.dispatch_serve_signal(signal).await? {
                return Ok(());
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
    async fn run_effect(&mut self, effect: Effect) -> Result<(), WorkerError> {
        let durable = store::effect_key(&self.account, &effect);
        match effect {
            Effect::Callback(cursor, frame, actor, epoch) => {
                match self.answer_callback(cursor, frame, actor, epoch).await {
                    Err(WorkerError::StaleVmActor) => return Ok(()),
                    result => result?,
                }
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
            self.db("complete_effect", move |store| async move {
                store::complete_effect(&store, &key).await
            })
            .await?;
        }
        Ok(())
    }
    async fn next_serve_signal(&mut self) -> ServeSignal {
        tokio::select! {
            biased;
            _ = Shutdown::cancelled(&mut self.stop) => ServeSignal::Shutdown,
            _ = tokio::time::sleep_until(self.flush_at.unwrap_or_else(||tokio::time::Instant::now()+STREAM_CADENCE)), if self.flush_at.is_some() => ServeSignal::Flush,
            _ = tokio::time::sleep_until(self.publication_retry_at.unwrap_or_else(||tokio::time::Instant::now()+STREAM_CADENCE)), if self.publication_retry_at.is_some() => ServeSignal::PublicationRetry,
            _ = tokio::time::sleep_until(self.retry_at.unwrap_or_else(|| tokio::time::Instant::now() + PREPARATION_RETRY)), if self.retry_at.is_some() => ServeSignal::PreparationRetry,
            _ = tokio::time::sleep_until(self.auth_wait_at.unwrap_or_else(tokio::time::Instant::now)), if self.auth_wait_at.is_some() && self.session.status.borrow().initialization == InitializationStatus::Ready => ServeSignal::AuthWait,
            _ = tokio::time::sleep_until(self.vm_credential_refresh_at.unwrap_or_else(tokio::time::Instant::now)), if self.vm_credential_refresh_at.is_some() && self.session.status.borrow().initialization == InitializationStatus::Ready => ServeSignal::VmCredentialRefresh,
            changed = self.auth_wait_changes.changed(), if self.resources.claude_account_pin.get("provider") == Some(&json!("claude")) && self.session.status.borrow().initialization == InitializationStatus::Ready => ServeSignal::AuthWaitChanged(changed),
            command = async { match self.queued_commands.pop_front() { Some(command) => Some(command), None => self.inbox.recv().await } }, if self.session.status.borrow().initialization == InitializationStatus::Ready => ServeSignal::Command(command),
            event = self.events.recv() => ServeSignal::Event(event),
            vm_event = self.vm_events.recv() => ServeSignal::VmEvent(vm_event),
            _ = self.connection_recovery.join_next(), if !self.connection_recovery.is_empty() => ServeSignal::ConnectionRecovery,
            callback = self.managed_callbacks.join_next(), if !self.managed_callbacks.is_empty() => ServeSignal::ManagedCallback(callback),
            changed = self.session.status.changed() => ServeSignal::SessionStatusChanged(changed.is_ok()),
        }
    }
    async fn dispatch_serve_signal(&mut self, signal: ServeSignal) -> Result<bool, WorkerError> {
        match signal {
            ServeSignal::Shutdown => self.closing = true,
            ServeSignal::Flush => self.flush().await?,
            ServeSignal::PublicationRetry => self.drain_publications().await?,
            ServeSignal::PreparationRetry => self.retry_preparations().await?,
            ServeSignal::AuthWait => self.probe_auth_waits().await?,
            ServeSignal::VmCredentialRefresh => self.refresh_vm_claude_credentials().await?,
            ServeSignal::AuthWaitChanged(changed) => {
                changed.map_err(|_| WorkerError::TaskFailed)?;
                self.probe_auth_waits().await?;
            }
            ServeSignal::Command(Some(command)) => match command {
                #[cfg(test)]
                AccountCommand::Barrier(reply) => {
                    let _ignored = reply.send(
                        self.bridge
                            .faults
                            .journal_count
                            .load(std::sync::atomic::Ordering::SeqCst),
                    );
                }
                AccountCommand::VmNotification(command) => self.vm_notification(*command).await?,
                AccountCommand::CommittedFrame(command) => self.committed_frame(*command).await?,
                AccountCommand::RetainedRequest(command) => self.retained_request(*command).await?,
                AccountCommand::JournalBarrier(command) => self.retained_barrier(command).await?,
                AccountCommand::ConnectionRecovery { agent_id, reply } => {
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
                        };
                        let commands = self.commands.clone();
                        self.connection_recovery.spawn(async move {
                            let (response, streams) = match connection_recovery::run(request).await
                            {
                                Ok(completed) => (Ok(completed.receipt), completed.streams),
                                Err(_) => (Err(retained::PortError::Unknown), None),
                            };
                            let (done, acknowledged) = tokio::sync::oneshot::channel();
                            if commands
                                .send(AccountCommand::ConnectionRecoveryComplete {
                                    result: response,
                                    streams,
                                    reply: done,
                                })
                                .await
                                .is_ok()
                            {
                                let final_result = acknowledged
                                    .await
                                    .unwrap_or(Err(retained::PortError::Unknown));
                                let _sent = reply.send(final_result);
                            } else {
                                let _sent = reply.send(Err(retained::PortError::Unknown));
                            }
                        });
                    }
                }
                AccountCommand::ConnectionRecoveryComplete {
                    result,
                    streams,
                    reply,
                } => {
                    if let Some(streams) = streams {
                        self.streams = streams;
                        self.schedule_publication_drain();
                    }
                    let _sent = reply.send(result);
                }
                AccountCommand::Wake { agent_id, epoch } => self.wake(agent_id, epoch).await?,
                AccountCommand::Stop { agent_id } => self.stop_agent(agent_id).await?,
                AccountCommand::RadioInterrupt {
                    agent_id,
                    thread_id,
                    turn_id,
                } => self.interrupt(agent_id, thread_id, turn_id).await?,
                AccountCommand::RestartRecoveryDone => {
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
                }
                AccountCommand::RestartStreams(streams) => {
                    self.streams = *streams;
                    self.schedule_publication_drain();
                }
            },
            ServeSignal::Command(None) | ServeSignal::Event(None) => return Ok(false),
            ServeSignal::Event(Some(event)) => self.event(event).await?,
            ServeSignal::VmEvent(Some(event)) => self.handle_vm_message(event).await?,
            ServeSignal::VmEvent(None) => return Ok(false),
            ServeSignal::ConnectionRecovery => self.connection_recovery_active = false,
            ServeSignal::ManagedCallback(callback) => {
                let (cursor, actor, epoch, response) = callback
                    .ok_or(WorkerError::TaskFailed)?
                    .map_err(|_| WorkerError::TaskFailed)?;
                if let Some(response) = response {
                    self.push(Effect::Callback(cursor, response, actor, epoch))?;
                }
            }
            ServeSignal::SessionStatusChanged(false) => return Ok(false),
            ServeSignal::SessionStatusChanged(true) => {}
        }
        Ok(true)
    }
    async fn db<T, F, Fut>(&mut self, name: &'static str, operation: F) -> Result<T, WorkerError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<RuntimeStore>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, WorkerError>> + Send + 'static,
    {
        let bridge = self.bridge.clone();
        let job = bridge.worker_run(name, operation);
        tokio::pin!(job);
        tokio::select! { biased;
            _ = Shutdown::cancelled(&mut self.stop), if !self.closing => { self.closing = true; self.control.cancel(); job.await },
            result = &mut job => result,
        }
    }
    async fn db_identified<T, F, Fut>(
        &mut self,
        name: &'static str,
        identity: String,
        operation: F,
    ) -> Result<T, WorkerError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<RuntimeStore>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, WorkerError>> + Send + 'static,
    {
        let bridge = self.bridge.clone();
        let job = bridge.worker_run_identified(name, Some(identity), operation);
        tokio::pin!(job);
        tokio::select! { biased;
            _ = Shutdown::cancelled(&mut self.stop), if !self.closing => { self.closing = true; self.control.cancel(); job.await },
            result = &mut job => result,
        }
    }
    async fn prepare<T, F, Fut>(
        &mut self,
        name: &'static str,
        operation: F,
    ) -> Result<T, WorkerError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<RuntimeStore>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, WorkerError>> + Send + 'static,
    {
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
                    Some(event) => self.handle_vm_message(event).await?,
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
                    Some(event) => self.handle_vm_message(event).await
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
            .db("retry_preparations", move |store| async move {
                blocking_store_read(store, move |store| {
                    store::pending_preparations(store, &account)
                })
                .await
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
        self.auth_wait_at = None;
        let Some(driver) = self.resources.account_driver.clone() else {
            return Ok(());
        };
        if self.resources.claude_account_pin.get("provider") != Some(&json!("claude")) {
            return Ok(());
        }
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| WorkerError::TaskFailed)?
            .as_secs_f64();
        let account = self.account.clone();
        let waits = self
            .db("claude_auth_due_waits", move |store| async move {
                crate::claude_auth_runtime::due_waits(&store, &account, now)
                    .await
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
        let account = self.account.clone();
        let next_deadline = self
            .db(
                "claude_auth_earliest_wait_deadline",
                move |store| async move {
                    crate::claude_auth_runtime::earliest_wait_deadline(&store, &account)
                        .await
                        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
                },
            )
            .await?;
        self.auth_wait_at = next_deadline.map(monotonic_deadline).transpose()?;
        Ok(())
    }
    async fn refresh_vm_claude_credentials(&mut self) -> Result<(), WorkerError> {
        let actors = self
            .vm_sessions
            .values()
            .map(|session| session.launch_request.clone())
            .filter(|request| request.provider == "claude" && request.account_key == self.account)
            .collect::<Vec<_>>();
        let Some(resolver) = self.resources.vm_provider_launch.clone() else {
            self.vm_credential_refresh_at = None;
            return Ok(());
        };
        if actors.is_empty() {
            self.vm_credential_refresh_at = None;
            return Ok(());
        }
        self.vm_credential_refresh_at = Some(
            tokio::time::Instant::now()
                + crate::claude_auth_runtime::VM_CREDENTIAL_REFRESH_INTERVAL,
        );
        // The host resolver reads credentials once and fans the result out
        // to the active actor profiles without restarting their sessions.
        match self
            .drive(resolver.refresh_claude_credentials(actors.clone()))
            .await
        {
            Ok(()) => {}
            Err(error) => match vm_credential_maintenance_error(&error) {
                Some(class) => eprintln!(
                    "VM Claude credential maintenance deferred; class={class}; actors={}",
                    actors.len()
                ),
                None => return Err(error),
            },
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
                .db("claim", move |store| async move {
                    store::claim(store, &account, &claim_agent, epoch).await
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
                    self.db("hold_invalid_agent", move |store| async move {
                        store::hold_invalid_agent_record(
                            &store,
                            &invalid_id,
                            &pointer,
                            &reason,
                            &diagnostic,
                        )
                        .await
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
                .db("was_submitted", move |store| async move {
                    store::was_submitted(&store, &operation).await
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
                .prepare("preparation", move |s| async move {
                    if method == Method::TurnStart {
                        crate::account_transfer_executor::before_start(
                            Arc::clone(&s),
                            resources.account_session.clone(),
                            &resources.model_catalog,
                            &resources,
                            &i.agent_id,
                            now(),
                        )
                        .await
                        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                    }
                    store::preparation_step(
                        &s,
                        &i,
                        preparation::admit(Arc::clone(&s), &resources, &i),
                    )
                    .await
                })
                .await?
            else {
                self.retry_at = Some(tokio::time::Instant::now() + PREPARATION_RETRY);
                return Ok(());
            };
            let i = intent.clone();
            let resources = self.resources.clone();
            let Some(token) = self
                .prepare("workspace", move |s| async move {
                    store::preparation_step(
                        &s,
                        &i,
                        preparation::workspace(Arc::clone(&s), &resources, &i),
                    )
                    .await
                })
                .await?
            else {
                self.retry_at = Some(tokio::time::Instant::now() + PREPARATION_RETRY);
                return Ok(());
            };
            let i = intent.clone();
            let resources = self.resources.clone();
            let Some(prepared) = self
                .prepare("preparation", move |s| async move {
                    store::preparation_step(&s, &i, {
                        preparation::prepare_ready(
                            Arc::clone(&s),
                            &resources,
                            &i,
                            method,
                            generation,
                            &preparation_connection_id,
                            selection,
                            token,
                        )
                    })
                    .await
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
            .db("was_submitted", move |store| async move {
                store::was_submitted(&store, &prior).await
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
            match self.ensure_vm_session(&context.intent).await {
                Ok(session) => session,
                Err(WorkerError::PreparationRefused(reason)) if method != Method::TurnInterrupt => {
                    let blocked_intent = context.intent.clone();
                    let _settled = self
                        .db("vm-session-identity-blocked", move |store| async move {
                            store::preparation_step(&store, &blocked_intent, async move {
                                Err::<(), WorkerError>(WorkerError::PreparationRefused(reason))
                            })
                            .await
                        })
                        .await?;
                    return Ok(());
                }
                // Stop already committed the actor's new epoch and disabled
                // wake. A stale provider session must not receive an interrupt
                // under the replacement identity; retain the existing receipt
                // state for recovery instead of failing the account worker.
                Err(WorkerError::PreparationRefused(_)) => return Ok(()),
                Err(WorkerError::StaleVmActor) => return Ok(()),
                Err(error) => return Err(error),
            }
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
        let prepared = match self
            .drive(control.prepare(RequestSpec {
                id: id.clone(),
                operation_id: OperationId(operation),
                method,
                params,
                thread_id: context.intent.thread_id.clone(),
                lightweight: method == Method::TurnInterrupt,
                timeout_ms: self.timeout_ms,
            }))
            .await
        {
            Ok(prepared) => prepared,
            Err(WorkerError::Session(SessionTaskError::Closed)) if actor_session.is_some() => {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if self.closing {
            return Ok(());
        }
        let recovery = prepared.recovery_record();
        let save_context = context.clone();
        let account = self.account.clone();
        let authorized = self
            .db("submitted", move |store| async move {
                store::submitted(
                    &store,
                    save_context,
                    &account,
                    recovery,
                    store::SubmissionEvidence {
                        turn: interrupt_turn.as_ref().map(|(turn, _)| turn.clone()),
                        submission,
                        guard,
                        connection_id,
                    },
                )
                .await
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
            Err(WorkerError::Session(SessionTaskError::Closed)) if actor_session.is_some() => {
                return Ok(());
            }
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
        let intent_epoch = intent.epoch;
        let generation = self.generation.0;
        let launch_request = self
            .db("vm_launch_request", move |store| async move {
                blocking_store_read(store, move |store| {
                    let connection = store.read()?;
                    let Some(record) =
                        AgentRepository::get(&connection, &AgentId::from(lookup_id.as_str()))?
                    else {
                        return Err(WorkerError::PreparationRefused("vm-agent/missing".into()));
                    };
                    if record.extra.get("executionMode").and_then(Value::as_str) != Some("vm") {
                        return Ok(None);
                    }
                    if !crate::linux_vm_provider::vm_actor_matches(
                        &connection,
                        &record.id.0,
                        intent_epoch,
                        &account_key,
                        Some(&provider),
                    )? {
                        return Err(WorkerError::PreparationRefused(
                            "vm-agent/identity-changed".into(),
                        ));
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
                        generation,
                        account_key,
                        provider,
                        cwd: cwd.to_owned(),
                        guest_home: guest_home.to_owned(),
                    }))
                })
                .await
            })
            .await?;
        let Some(request) = launch_request else {
            return Ok(None);
        };
        if let Some(session) = self.vm_sessions.get(&request.agent_id) {
            if session.launch_request != request {
                return Err(WorkerError::PreparationRefused(
                    "vm-agent/session-identity-changed".into(),
                ));
            }
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
            let actor = request.agent_id.clone();
            let expected = request.clone();
            self.db("vm_launch_revalidate", move |store| async move {
                blocking_store_read(store, move |store| {
                    let connection = store.read()?;
                    if !crate::linux_vm_provider::vm_actor_matches(
                        &connection,
                        &actor,
                        expected.epoch,
                        &expected.account_key,
                        Some(&expected.provider),
                    )? {
                        return Err(WorkerError::PreparationRefused(
                            "vm-agent/identity-changed".into(),
                        ));
                    }
                    Ok(())
                })
                .await
            })
            .await?;
            let recovery_agent = request.agent_id.clone();
            let recovery_account = self.account.clone();
            let generation = self.generation.0;
            let recovered = self
                .db("vm_actor_recovery", move |store| async move {
                    blocking_store_read(store, move |store| {
                        store::recover_vm_actor_requests(
                            store,
                            &recovery_account,
                            &recovery_agent,
                            generation,
                        )
                    })
                    .await
                })
                .await?;
            let factory = Arc::new(crate::linux_vm_provider::GuestProviderTaskFactory::new(
                Arc::clone(&self.bridge.store),
                guest,
                crate::linux_vm_provider::GuestProviderIdentity {
                    agent_id: request.agent_id.clone(),
                    epoch: request.epoch,
                    generation: request.generation,
                    account_key: request.account_key.clone(),
                    provider: resolved.provider.clone(),
                },
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
            self.vm_incarnation_counter = self
                .vm_incarnation_counter
                .checked_add(1)
                .ok_or(WorkerError::TaskFailed)?;
            let actor_incarnation = self.vm_incarnation_counter;
            let source = VmActorIdentity {
                actor_id: request.agent_id.clone(),
                epoch: request.epoch,
                incarnation: actor_incarnation,
            };
            let sender = self.vm_event_tx.clone();
            let forwarder = tokio::spawn(async move {
                while let Some(event) = events.recv().await {
                    if sender
                        .send(VmForwardedMessage::Event {
                            source: source.clone(),
                            event,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                let _sent = sender.send(VmForwardedMessage::Exit { source }).await;
            });
            self.vm_event_forwarders.push(forwarder);
            let had_active_claude_vm = self.has_active_claude_vm_sessions();
            self.vm_sessions.insert(
                request.agent_id.clone(),
                ActorProviderSession {
                    session,
                    epoch: request.epoch,
                    incarnation: actor_incarnation,
                    control,
                    launch_request: request.clone(),
                    bootstrap: resolved.bootstrap,
                },
            );
            self.vm_credential_refresh_at = vm_refresh_deadline_after_session_change(
                had_active_claude_vm,
                self.has_active_claude_vm_sessions(),
                self.resources.vm_provider_launch.is_some(),
                self.vm_credential_refresh_at,
                tokio::time::Instant::now(),
            );
        }
        let mut status = self
            .vm_sessions
            .get(&request.agent_id)
            .ok_or(WorkerError::Closed)?
            .session
            .status
            .clone();
        let actor_incarnation = self
            .vm_sessions
            .get(&request.agent_id)
            .map(|session| session.incarnation)
            .ok_or(WorkerError::StaleVmActor)?;
        while status.borrow().initialization != InitializationStatus::Ready {
            tokio::select! {
                _ = Shutdown::cancelled(&mut self.stop) => return Err(WorkerError::Closed),
                changed = status.changed() => {
                    if changed.is_err() {
                        while self.vm_sessions.get(&request.agent_id).is_some_and(|session| {
                            session.epoch == request.epoch
                                && session.incarnation == actor_incarnation
                        }) {
                            let event = self.vm_events.recv().await.ok_or(WorkerError::Closed)?;
                            self.handle_vm_message(event).await?;
                        }
                        return Err(WorkerError::StaleVmActor);
                    }
                },
                event = self.vm_events.recv() => match event {
                    Some(event) => self.handle_vm_message(event).await?,
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

    /// Confirm that a retained VM session still represents the current durable
    /// actor identity before selecting it for any RPC lane.
    pub(super) async fn vm_session_matches_actor(
        &mut self,
        agent_id: &str,
        account_key: &str,
        epoch: i64,
    ) -> Result<bool, WorkerError> {
        let agent_id = agent_id.to_owned();
        let lookup_id = agent_id.clone();
        let account_key = account_key.to_owned();
        let expected_account_key = account_key.clone();
        let generation = self.generation.0;
        let expected = self
            .db("retained_vm_session_identity", move |store| async move {
                blocking_store_read(store, move |store| {
                    let connection = store.read()?;
                    let Some(record) =
                        AgentRepository::get(&connection, &AgentId::from(lookup_id.as_str()))?
                    else {
                        return Ok(None);
                    };
                    let provider = record
                        .extra
                        .get("provider")
                        .and_then(Value::as_str)
                        .unwrap_or("codex");
                    if !crate::linux_vm_provider::vm_actor_matches(
                        &connection,
                        &record.id.0,
                        epoch,
                        &account_key,
                        Some(provider),
                    )? {
                        return Ok(None);
                    }
                    let cwd = record.extra.get("cwd").and_then(Value::as_str);
                    if cwd.is_some_and(|value| !value.starts_with('/')) {
                        return Ok(None);
                    }
                    let guest_home = record
                        .extra
                        .get("layrHome")
                        .and_then(Value::as_str)
                        .filter(|value| value.starts_with('/'))
                        .unwrap_or("/home/studio");
                    Ok(Some((
                        record.id.0,
                        provider.to_owned(),
                        cwd.map(str::to_owned),
                        guest_home.to_owned(),
                    )))
                })
                .await
            })
            .await?;
        let Some((id, provider, cwd, guest_home)) = expected else {
            return Ok(false);
        };
        let Some(session) = self.vm_sessions.get(agent_id.as_str()) else {
            return Ok(false);
        };
        let expected = preparation::VmProviderLaunchRequest {
            agent_id: id,
            epoch,
            generation,
            account_key: expected_account_key,
            provider,
            cwd: cwd.unwrap_or_else(|| session.launch_request.cwd.clone()),
            guest_home,
        };
        Ok(session.launch_request == expected)
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
            .db("vm_auth_bootstrap_admission", move |store| async move {
                store::vm_bootstrap_admission(&store, &admission_operation, &admission_signature)
                    .await
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
                    Some(event) => self.handle_vm_message(event).await?,
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
                self.db("vm_auth_bootstrap_settled", move |store| async move {
                    store::vm_bootstrap_settled(&store, &settled_operation, &settled_signature)
                        .await
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
            .db("stop", move |store| async move {
                store::stop(&store, &account, &agent).await
            })
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
            .db("interrupt_intent", move |store| async move {
                blocking_store_read(store, move |store| {
                    store::interrupt_intent(store, &agent, &thread)
                })
                .await
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
            if event.outcomes.is_empty()
                && let Some(journal) = event.journal
            {
                let mut entries = vec![journal];
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
                self.journal_batch(entries, None, true).await?;
                self.complete_retained().await?;
            } else {
                self.event_one(event, None, true).await?;
            }
        }
        Ok(())
    }

    async fn event_from(
        &mut self,
        event: SessionEvent,
        source: &VmActorIdentity,
    ) -> Result<(), WorkerError> {
        let Some(session) = self.vm_sessions.get(&source.actor_id) else {
            return Ok(());
        };
        if session.epoch != source.epoch || session.incarnation != source.incarnation {
            return Ok(());
        }
        match self.event_one(event, Some(source), true).await {
            Err(WorkerError::StaleVmActor) => {
                self.retire_vm_actor_if_current(source);
                Ok(())
            }
            result => result,
        }
    }

    async fn event_one(
        &mut self,
        event: SessionEvent,
        source: Option<&VmActorIdentity>,
        settle: bool,
    ) -> Result<(), WorkerError> {
        let actor_id = source.map(|source| source.actor_id.as_str());
        let actor_epoch = source.map(|source| source.epoch);
        let reclassify = !event.outcomes.is_empty();
        for outcome in event.outcomes {
            if actor_id.is_some() && matches!(&outcome, SessionOutcome::GenerationClosed { .. }) {
                let source_is_current = source.is_some_and(|source| {
                    self.vm_sessions
                        .get(&source.actor_id)
                        .is_some_and(|session| {
                            session.epoch == source.epoch
                                && session.incarnation == source.incarnation
                        })
                });
                let replacement_is_active = source.is_some_and(|source| {
                    self.vm_sessions.contains_key(&source.actor_id) && !source_is_current
                });
                if !replacement_is_active && let Some(source) = source {
                    let actor = source.actor_id.clone();
                    let account = self.account.clone();
                    // Scheduler attempts are fenced by the account worker
                    // generation. The VM guest's generation is local to its
                    // retained provider handle and must not be used as that
                    // durable attempt identity.
                    let epoch = source.epoch;
                    let generation = self.generation.0;
                    self.db("vm_actor_disconnected", move |store| async move {
                        store::disconnect_vm_actor(&store, &actor, epoch, &account, generation)
                            .await
                    })
                    .await?;
                }
                if source_is_current && let Some(source) = source {
                    self.retire_vm_actor_if_current(source);
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
                    actor_id,
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
                && let Some((cursor, callback_actor, callback_epoch)) =
                    self.callbacks.remove(&operation_id.0)
            {
                let operation = operation_id.0.clone();
                let written = *outcome == WriteOutcome::Written;
                let write_outcome = *outcome;
                let identity = operation.clone();
                self.db_identified("callback_written", identity, move |store| async move {
                    store::callback_written(&store, &operation, write_outcome).await
                })
                .await?;
                if !written {
                    return Err(WorkerError::CallbackUnknown);
                }
                let journal_scope = self.journal_scope(callback_actor.as_deref());
                let generation = self.generation.0;
                self.db("callback_settled", move |store| async move {
                    store::callback_settled(&store, &journal_scope, generation, cursor.0).await
                })
                .await?;
                self.managed_callback_cursors.remove(&(
                    callback_actor.clone(),
                    callback_epoch,
                    cursor.0,
                ));
                let callback_is_current = callback_actor.as_deref().is_none_or(|actor| {
                    self.vm_sessions
                        .get(actor)
                        .is_some_and(|session| Some(session.epoch) == callback_epoch)
                });
                if !self.closing && callback_is_current {
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
            if let Some(actor) = actor_id {
                match (ticket, pending.as_ref()) {
                    (Some(_), Some(pending))
                        if pending.intent.agent_id == actor
                            && Some(pending.intent.epoch) == actor_epoch
                            && pending.intent.account_key == self.account => {}
                    (Some(_), None) => continue,
                    (None, _) => continue,
                    _ => {
                        return Err(WorkerError::PreparationRefused(
                            "account-worker/vm-notification-actor-changed".into(),
                        ));
                    }
                }
            }
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
            let resources = Arc::new(self.resources.clone());
            let (retire, effect) = self
                .db_identified("outcome", identity, move |store| async move {
                    store::outcome(store, account, resources, pending, outcome).await
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
            self.journal(entry, source, settle).await?;
        }
        if reclassify {
            for _ in 0..self.deferred.len() {
                let Some((entry, deferred_source)) = self.deferred.pop_front() else {
                    continue;
                };
                if let Some(source) = deferred_source.as_ref() {
                    let Some(session) = self.vm_sessions.get(&source.actor_id) else {
                        continue;
                    };
                    if session.epoch != source.epoch || session.incarnation != source.incarnation {
                        continue;
                    }
                }
                match self.journal(entry, deferred_source.as_ref(), settle).await {
                    Err(WorkerError::StaleVmActor) => {
                        if let Some(source) = deferred_source.as_ref() {
                            self.retire_vm_actor_if_current(source);
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
        let had_active_claude_vm = self.has_active_claude_vm_sessions();
        if let Some(session) = self.vm_sessions.remove(actor_id) {
            session.control.cancel();
            let key = (actor_id.to_owned(), session.incarnation);
            self.retired_vm_sessions.insert(key, session);
        }
        self.vm_credential_refresh_at = vm_refresh_deadline_after_session_change(
            had_active_claude_vm,
            self.has_active_claude_vm_sessions(),
            self.resources.vm_provider_launch.is_some(),
            self.vm_credential_refresh_at,
            tokio::time::Instant::now(),
        );
        let stale_waiters: Vec<_> = self
            .vm_notification_waiters
            .keys()
            .filter(|(actor, _)| actor == actor_id)
            .cloned()
            .collect();
        for key in stale_waiters {
            if let Some(waiter) = self.vm_notification_waiters.remove(&key) {
                let _sent = waiter.send(Err(retained::PortError::Unknown));
            }
        }
        self.deferred.retain(|(_, source)| {
            source
                .as_ref()
                .is_none_or(|source| source.actor_id != actor_id)
        });
    }

    fn has_active_claude_vm_sessions(&self) -> bool {
        self.vm_sessions.values().any(|session| {
            session.launch_request.provider == "claude"
                && session.launch_request.account_key == self.account
        })
    }

    fn retire_vm_actor_if_current(&mut self, source: &VmActorIdentity) {
        if self
            .vm_sessions
            .get(&source.actor_id)
            .is_some_and(|session| {
                session.epoch == source.epoch && session.incarnation == source.incarnation
            })
        {
            self.retire_vm_actor(&source.actor_id);
        }
    }

    fn current_vm_source(&self, actor_id: &str, epoch: i64) -> Option<VmActorIdentity> {
        self.vm_sessions.get(actor_id).and_then(|session| {
            (session.epoch == epoch).then(|| VmActorIdentity {
                actor_id: actor_id.to_owned(),
                epoch,
                incarnation: session.incarnation,
            })
        })
    }

    fn stale_vm_actor(&mut self, _actor_id: Option<&str>) -> WorkerError {
        WorkerError::StaleVmActor
    }

    async fn retire_closed_vm_actor(
        &mut self,
        actor_id: &str,
        source_epoch: Option<i64>,
    ) -> Result<(), WorkerError> {
        if let Some(source) = source_epoch.and_then(|epoch| self.current_vm_source(actor_id, epoch))
        {
            let account = self.account.clone();
            let generation = self.generation.0;
            let actor = source.actor_id.clone();
            self.db("vm_actor_disconnected", move |store| async move {
                store::disconnect_vm_actor(&store, &actor, source.epoch, &account, generation).await
            })
            .await?;
            self.retire_vm_actor_if_current(&source);
        }
        Ok(())
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
        source: &VmActorIdentity,
        settle: bool,
    ) -> Result<bool, WorkerError> {
        self.journal_batch(vec![entry], Some(source), settle).await
    }
    async fn journal(
        &mut self,
        entry: JournalEntry,
        source: Option<&VmActorIdentity>,
        settle: bool,
    ) -> Result<(), WorkerError> {
        self.journal_batch(vec![entry], source, settle)
            .await
            .map(|_| ())
    }
    async fn journal_batch(
        &mut self,
        entries: Vec<JournalEntry>,
        source: Option<&VmActorIdentity>,
        settle: bool,
    ) -> Result<bool, WorkerError> {
        #[cfg(test)]
        self.bridge
            .faults
            .journal_transactions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let account = self.account.clone();
        let actor_id = source.map(|source| source.actor_id.as_str());
        let actor_epoch = source.map(|source| source.epoch);
        let journal_scope = self.journal_scope(actor_id);
        let streams = std::mem::take(&mut self.streams);
        let claude_account_pin = self.resources.claude_account_pin.clone();
        let expected_vm_actor = actor_id
            .zip(actor_epoch)
            .map(|(actor, epoch)| (actor.to_owned(), epoch));
        let (previous, committed) = self
            .db("journal", move |store| async move {
                let result = store::journal_batch(
                    store,
                    &account,
                    &journal_scope,
                    &streams,
                    entries,
                    &claude_account_pin,
                    expected_vm_actor
                        .as_ref()
                        .map(|(actor, epoch)| (actor.as_str(), *epoch)),
                )
                .await;
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
        self.deferred
            .extend(deferred.into_iter().map(|entry| (entry, source.cloned())));
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
        for (index, (entry, effect)) in committed.into_iter().enumerate() {
            #[cfg(not(test))]
            let _ = index;
            #[cfg(test)]
            if self
                .bridge
                .faults
                .fail_journal_ack_at
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take_if(|fail_at| *fail_at == index)
                .is_some()
            {
                return Err(WorkerError::TaskFailed);
            }
            self.journal_committed(entry, effect, source, settle)
                .await?;
        }
        Ok(committed_any && !had_deferred)
    }
    async fn journal_committed(
        &mut self,
        entry: JournalEntry,
        effect: Option<Effect>,
        source: Option<&VmActorIdentity>,
        settle: bool,
    ) -> Result<(), WorkerError> {
        let actor_id = source.map(|source| source.actor_id.as_str());
        let actor_epoch = source.map(|source| source.epoch);
        let actor_incarnation = source.map(|source| source.incarnation);
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
                .insert((actor.clone(), actor_epoch, cursor.0));
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
                        return (cursor, actor, actor_epoch, None);
                    };
                    return (
                        cursor,
                        actor,
                        actor_epoch,
                        Some(callback::tool_call_failure(request, dependency)),
                    );
                }
                let response = callback::managed_response(
                    &store,
                    &account,
                    generation,
                    actor.as_deref(),
                    &frame,
                    now,
                    callback::ManagedResponseDependencies {
                        wake,
                        monitor_dispatch_wake,
                        managed_runtime: managed_tool_runtime,
                    },
                )
                .await;
                (cursor, actor, actor_epoch, response)
            });
        } else if let JournalPayload::Frame(frame) = &entry.payload
            && callback::is_vm_codex_refresh_request(frame)
        {
            let account = self.account.clone();
            let actor = actor_id.map(str::to_owned);
            let actor_epoch = actor_epoch.or_else(|| {
                actor.as_ref().and_then(|actor| {
                    self.vm_sessions
                        .get(actor)
                        .map(|session| session.launch_request.epoch)
                })
            });
            let generation = self.generation.0;
            let frame = frame.clone();
            let resolver = self.resources.vm_provider_launch.clone();
            if self.callbacks.len() + self.managed_callbacks.len() >= MAX_PENDING_RESPONSES {
                return Err(WorkerError::Capacity);
            }
            self.managed_callback_cursors
                .insert((actor.clone(), actor_epoch, cursor.0));
            self.managed_callbacks.spawn(async move {
                let response = callback::vm_codex_refresh_response(
                    &account,
                    actor.as_deref(),
                    actor_epoch,
                    generation,
                    &frame,
                    resolver,
                )
                .await;
                (cursor, actor, actor_epoch, response)
            });
        } else if let JournalPayload::Frame(frame) = &entry.payload
            && let Some(response) = callback::response(frame)
        {
            self.push(Effect::Callback(
                cursor,
                response,
                actor_id.map(str::to_owned),
                actor_epoch,
            ))?;
        } else if settle && !self.closing {
            if let Some(actor) = actor_id
                && !self.vm_sessions.get(actor).is_some_and(|session| {
                    Some(session.epoch) == actor_epoch
                        && Some(session.incarnation) == actor_incarnation
                })
            {
                return Err(WorkerError::StaleVmActor);
            }
            let control = match self.control_for_actor(actor_id) {
                Ok(control) => control,
                Err(WorkerError::Closed) if actor_id.is_some() => {
                    if let Some(actor) = actor_id {
                        self.retire_vm_actor(actor);
                    }
                    return Err(self.stale_vm_actor(actor_id));
                }
                Err(error) => return Err(error),
            };
            if let Err(error) = control.settled(cursor).await {
                if actor_id.is_some() && matches!(error, SessionTaskError::Closed) {
                    if let Some(actor) = actor_id {
                        self.retire_closed_vm_actor(actor, actor_epoch).await?;
                    }
                    return Err(self.stale_vm_actor(actor_id));
                }
                if actor_id.is_some() {
                    return Err(error.into());
                }
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
        let Some(source) = self
            .vm_sessions
            .get(&agent_id)
            .filter(|session| session.epoch == epoch)
            .map(|session| VmActorIdentity {
                actor_id: agent_id.clone(),
                epoch,
                incarnation: session.incarnation,
            })
        else {
            let _sent = reply.send(Err(retained::PortError::Unknown));
            return Ok(());
        };
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
        match self.journal_with_policy(entry, &source, false).await {
            Ok(true) => {
                let _sent = reply.send(Ok(()));
            }
            Ok(false) => {
                if let std::collections::btree_map::Entry::Vacant(entry) =
                    self.vm_notification_waiters.entry(key)
                {
                    entry.insert(reply);
                } else {
                    let _sent = reply.send(Err(retained::PortError::Capacity));
                }
            }
            Err(_) => {
                let _sent = reply.send(Err(retained::PortError::Unknown));
            }
        }
        Ok(())
    }
    async fn answer_callback(
        &mut self,
        cursor: studio_runtime_process::EventCursor,
        frame: Frame,
        actor_id: Option<String>,
        actor_epoch: Option<i64>,
    ) -> Result<(), WorkerError> {
        if let Some(actor) = actor_id.as_deref() {
            #[cfg(test)]
            self.bridge
                .faults
                .mark_callback_phase("validating callback actor identity");
            let Some(expected) = self
                .vm_sessions
                .get(actor)
                .filter(|session| Some(session.epoch) == actor_epoch)
                .map(|session| session.launch_request.clone())
            else {
                return Err(WorkerError::StaleVmActor);
            };
            if expected.epoch != actor_epoch.unwrap_or(-1)
                || expected.account_key != self.account
                || expected.generation != self.generation.0
                || !vm_account_generation_matches(&self.resources, &expected)
            {
                if let Some(source) =
                    actor_epoch.and_then(|epoch| self.current_vm_source(actor, epoch))
                {
                    self.retire_vm_actor_if_current(&source);
                }
                return Err(WorkerError::StaleVmActor);
            }
            let actor = actor.to_owned();
            let account = expected.account_key.clone();
            let epoch = expected.epoch;
            let provider = expected.provider.clone();
            if !self
                .db("vm_callback_revalidate", move |store| async move {
                    blocking_store_read(store, move |store| {
                        let connection = store.read()?;
                        crate::linux_vm_provider::vm_actor_matches(
                            &connection,
                            &actor,
                            epoch,
                            &account,
                            Some(&provider),
                        )
                        .map_err(WorkerError::from)
                    })
                    .await
                })
                .await?
            {
                if let Some(actor) = actor_id
                    && let Some(source) =
                        actor_epoch.and_then(|epoch| self.current_vm_source(&actor, epoch))
                {
                    self.retire_vm_actor_if_current(&source);
                }
                return Err(WorkerError::StaleVmActor);
            }
        }
        loop {
            let status = match self.control_status(actor_id.as_deref()) {
                Ok(status) => status,
                Err(WorkerError::Closed) if actor_id.is_some() => {
                    return Err(self.stale_vm_actor(actor_id.as_deref()));
                }
                Err(error) => return Err(error),
            };
            if status == InitializationStatus::Ready {
                break;
            }
            tokio::select! { biased;
                _ = Shutdown::cancelled(&mut self.stop) => { self.closing = true; return Ok(()); },
                event = self.events.recv(), if actor_id.is_none() => { self.event(event.ok_or(WorkerError::Closed)?).await?; },
                event = self.vm_events.recv(), if actor_id.is_some() => {
                    match event { Some(event) => self.handle_vm_message(event).await?, None => return Err(WorkerError::Closed) }
                },
                command = self.inbox.recv(), if self.queued_commands.len() < COMMAND_CAPACITY => match command {
                    Some(AccountCommand::VmNotification(command)) => self.vm_notification(*command).await?,
                    Some(command) => self.queued_commands.push_back(command),
                    None => return Err(WorkerError::Closed),
                },
            }
        }
        let control = match self.control_for_actor(actor_id.as_deref()) {
            Ok(control) => control,
            Err(WorkerError::Closed) if actor_id.is_some() => {
                return Err(self.stale_vm_actor(actor_id.as_deref()));
            }
            Err(error) => return Err(error),
        };
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
        let prepared = match control.prepare_frame(OperationId(operation.clone()), &frame) {
            Ok(prepared) => prepared,
            Err(SessionTaskError::Closed) if actor_id.is_some() => {
                if let Some(actor) = actor_id.as_deref() {
                    self.retire_closed_vm_actor(actor, actor_epoch).await?;
                }
                return Err(self.stale_vm_actor(actor_id.as_deref()));
            }
            Err(error) => return Err(error.into()),
        };
        let key = operation.clone();
        let generation = self.generation.0;
        let admission = self
            .db("callback_admission", move |store| async move {
                store::callback_admission(&store, &key, &journal_scope, generation, cursor.0).await
            })
            .await?;
        if admission {
            match control.settled(cursor).await {
                Ok(()) => {}
                Err(SessionTaskError::Closed) if actor_id.is_some() => {
                    if let Some(actor) = actor_id.as_deref() {
                        self.retire_closed_vm_actor(actor, actor_epoch).await?;
                    }
                    return Err(self.stale_vm_actor(actor_id.as_deref()));
                }
                Err(error) => return Err(error.into()),
            }
            self.managed_callback_cursors
                .remove(&(actor_id.clone(), actor_epoch, cursor.0));
            return Ok(());
        }
        if self.callbacks.len() >= MAX_PENDING_RESPONSES {
            return Err(WorkerError::Capacity);
        }
        // A callback may have waited while this actor was resolving. Fence it
        // again immediately before the provider response can leave the host.
        if let Some(actor) = actor_id.as_deref() {
            #[cfg(test)]
            self.bridge
                .faults
                .mark_callback_phase("checking final callback generation fence");
            let Some(expected) = self
                .vm_sessions
                .get(actor)
                .filter(|session| Some(session.epoch) == actor_epoch)
                .map(|session| session.launch_request.clone())
            else {
                return Err(WorkerError::StaleVmActor);
            };
            if expected.generation != self.generation.0
                || !vm_account_generation_matches(&self.resources, &expected)
            {
                #[cfg(test)]
                self.bridge
                    .faults
                    .mark_callback_phase("retiring stale callback actor");
                if let Some(source) =
                    actor_epoch.and_then(|epoch| self.current_vm_source(actor, epoch))
                {
                    self.retire_vm_actor_if_current(&source);
                }
                #[cfg(test)]
                self.bridge
                    .faults
                    .mark_callback_phase("stale callback actor retired");
                return Err(WorkerError::StaleVmActor);
            }
            let actor = actor.to_owned();
            let revalidated = self
                .db("vm_callback_submit_revalidate", move |store| async move {
                    blocking_store_read(store, move |store| {
                        let connection = store.read()?;
                        if !crate::linux_vm_provider::vm_actor_matches(
                            &connection,
                            &actor,
                            expected.epoch,
                            &expected.account_key,
                            Some(&expected.provider),
                        )? {
                            return Err(WorkerError::PreparationRefused(
                                "account-worker/vm-notification-actor-changed".into(),
                            ));
                        }
                        Ok(())
                    })
                    .await
                })
                .await;
            match revalidated {
                Ok(()) => {}
                Err(WorkerError::PreparationRefused(reason))
                    if reason == "account-worker/vm-notification-actor-changed" =>
                {
                    if let Some(actor) = actor_id
                        && let Some(source) =
                            actor_epoch.and_then(|epoch| self.current_vm_source(&actor, epoch))
                    {
                        self.retire_vm_actor_if_current(&source);
                    }
                    return Err(WorkerError::StaleVmActor);
                }
                Err(error) => return Err(error),
            }
        }
        let is_vm_actor = actor_id.is_some();
        self.callbacks
            .insert(operation.clone(), (cursor, actor_id.clone(), actor_epoch));
        if let Err(error) = self.drive(control.submit_frame(prepared)).await {
            if is_vm_actor && matches!(error, WorkerError::Session(SessionTaskError::Closed)) {
                self.callbacks.remove(&operation);
                if let Some(actor) = actor_id.as_deref() {
                    self.retire_closed_vm_actor(actor, actor_epoch).await?;
                }
                return Err(self.stale_vm_actor(actor_id.as_deref()));
            }
            if definite_rejection(&error) {
                self.callbacks.remove(&operation);
                let identity = operation.clone();
                self.db_identified("callback_written", identity, move |store| async move {
                    store::callback_written(&store, &operation, WriteOutcome::NotSent).await
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
        let flush = move |store: Arc<RuntimeStore>| async move {
            store::flush_streams(store, &account, streams).await
        };
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
            .db("drain_publications", move |store| async move {
                store::drain_publications(store, account, sink).await
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
                    Ok((cursor, actor, epoch, Some(response))) => {
                        self.push(Effect::Callback(cursor, response, actor, epoch))?;
                    }
                    Ok((_, _, _, None)) => {}
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
                Effect::Callback(cursor, response, actor, epoch) => {
                    match self.answer_callback(cursor, response, actor, epoch).await {
                        Err(WorkerError::StaleVmActor) => {}
                        result => result?,
                    }
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
        self.db_identified("finish", identity, move |store| async move {
            if let Some(recovery) = recovery {
                store::save_recovery(&store, &account, &recovery).await?;
            }
            scheduler::lifecycle_transaction(&store, move |tx| {
                SchedulerRepository::disconnect_account(tx, &account, generation, now(), false)
            })
            .await?;
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
    match error {
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
