//! One Tokio task owns an account. SQLite writes are submitted to RuntimeStore's
//! bounded actor; dropping a handle signals shutdown and drains an in-flight job.
use crate::{
    notification_dispatch::NotificationStreamBuffer,
    provider_session_task::{
        RecoveredRequest, RequestSpec, SessionControl, SessionEvent, SessionExit, SessionHandle,
        SessionOutcome, SessionTaskConfig, SessionTaskError, WriteOutcome,
    },
    scheduler::{self, TurnIntent, TurnStart},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    pin::Pin,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use studio_runtime_process::{
    Generation, OperationId, ProviderHandle,
    provider_seam::{
        InitializationStatus, JournalEntry, JournalPayload, OpenOptions, ProviderTaskFactory,
        Shutdown,
    },
};
use studio_runtime_providers::{Frame, Method, RequestId, RequestOutcome, ResponseBody};
use studio_runtime_store::{
    AgentId, AgentRepository, ObservedTable, RuntimeStore, SchedulerRepository, StoreError,
};
use tokio::{
    sync::{Notify, mpsc, watch},
    task::{JoinHandle, JoinSet},
};
mod agent_fault;
mod agent_wakes;
mod background_commands;
pub mod callback;
mod callback_record;
mod connection_recovery;
pub(crate) mod history;
mod idle_unload;
pub mod preparation;
pub(crate) mod restart_recovery;
pub mod retained;
pub mod retained_frame;
mod steer;
pub(crate) mod store;
mod submission;
mod turn_preparation;

impl From<rusqlite::Error> for WorkerError {
    fn from(error: rusqlite::Error) -> Self {
        StoreError::from(error).into()
    }
}

pub const COMMAND_CAPACITY: usize = 16;
/// Canvas tables whose committed changes may alter idle-unload eligibility.
const IDLE_STATE_OBSERVED_TABLES: [ObservedTable; 6] = [
    ObservedTable::Agents,
    ObservedTable::Events,
    ObservedTable::Monitors,
    ObservedTable::AccountTransfers,
    ObservedTable::RuntimeRequests,
    ObservedTable::RuntimeToolRequests,
];
// Maximum provider events per journal transaction (count); bounds transaction work and ACK delay.
// This journal transaction budget is independent of queued turn-input batching.
const JOURNAL_BATCH_MAX_EVENTS: usize = 32;
/// Most provider callback handlers running at once; unit: handlers. A callback
/// beyond it stays durably accepted and starts when a handler finishes. The
/// worker keeps reading provider events meanwhile, because callback answers
/// need the session and the session needs the worker to read.
const MAX_RUNNING_CALLBACKS: usize = 64;
/// Most handled callback answers waiting for their provider write; unit: answers.
/// Further callbacks start only when answers drain, never by pausing the intake.
const MAX_QUEUED_CALLBACK_ANSWERS: usize = 64;
const STREAM_CADENCE: std::time::Duration = std::time::Duration::from_millis(750);
const PUBLICATION_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(1);
/// Wall time before the worker journals again the entries that a full monitor
/// output queue refused. The queue consumer frees room independently of the
/// worker; provider events wait in the session meanwhile.
const STREAM_CAPACITY_RETRY: std::time::Duration = std::time::Duration::from_millis(250);
// A worker can be finishing a serialized provider operation when shutdown is
// requested. Keep shutdown bounded while allowing ordinary host contention to
// clear before reporting an unconfirmed stop.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

/// Active VM Claude credential refresh cadence; unit is seconds.
pub(crate) const VM_CREDENTIAL_REFRESH_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(180);

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
    /// Pending on a blocker this preparation cannot name: any agent change may clear it.
    #[error("{0}")]
    PreparationPending(String),
    /// Pending until the agent's own row changes (its workspace or VM readiness).
    #[error("{0}")]
    PreparationAwaitsAgent(String),
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
        WorkerError::PreparationPending(_) | WorkerError::PreparationAwaitsAgent(_) => {
            Some("pending")
        }
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

/// Share one bridge among all account workers of a runtime. Each account
/// worker runs its store operations one at a time; SQLite admission, writer
/// order and callback lifetime are owned by RuntimeStore's actor.
#[derive(Clone)]
pub struct StoreBridge {
    store: Arc<RuntimeStore>,
    accounts: Arc<std::sync::Mutex<BTreeSet<String>>>,
    /// Accounts whose worker already recovered in this backend process. A later
    /// worker for the same account replaces only its provider process, so that
    /// recovery is a provider disconnect, not a server restart.
    recovered_accounts: Arc<std::sync::Mutex<BTreeSet<String>>>,
    event_wake: Arc<std::sync::RwLock<Arc<Notify>>>,
    continuation_wake: Arc<std::sync::RwLock<Arc<Notify>>>,
    monitor_dispatch_wake: Arc<std::sync::RwLock<Arc<Notify>>>,
    idle_state_generation: Arc<watch::Sender<u64>>,
    idle_setting_generation: Arc<watch::Sender<u64>>,
    /// Advances on every committed `runtime_agents` change; the start gate
    /// waits on it for a freed running slot.
    agent_generation: Arc<watch::Sender<u64>>,
    /// Pending preparations wait here for the agent rows they depend on.
    agent_wakes: Arc<agent_wakes::AgentWakes>,
    /// Turn start admission per account (see `account_start_gate`); shared by
    /// every worker of this process, so a replaced worker keeps the counts.
    start_gates: Arc<crate::account_start_gate::StartGates>,
    managed_tool_runtime:
        Arc<std::sync::RwLock<Option<Arc<crate::provider_callbacks::ManagedToolRuntime>>>>,
    /// Per-bridge fixture samples; DiskGate still owns every admission rule.
    #[cfg(test)]
    disk_gate: Option<Arc<crate::disk_space::DiskGate>>,
    #[cfg(test)]
    faults: Arc<tests::Faults>,
    #[cfg(test)]
    account_released: Arc<tokio::sync::Notify>,
}
impl StoreBridge {
    pub async fn new(store: Arc<RuntimeStore>) -> Result<Self, WorkerError> {
        let (idle_state_generation, _) = watch::channel(0_u64);
        let idle_state_generation = Arc::new(idle_state_generation);
        let observed_idle_generation = Arc::downgrade(&idle_state_generation);
        for table in IDLE_STATE_OBSERVED_TABLES {
            let observed = observed_idle_generation.clone();
            store.add_table_change_observer(table, move || {
                if let Some(generation) = observed.upgrade() {
                    generation.send_modify(|value| *value = value.wrapping_add(1));
                }
            });
        }
        let (idle_setting_generation, _) = watch::channel(0_u64);
        let idle_setting_generation = Arc::new(idle_setting_generation);
        let observed_idle_setting = Arc::downgrade(&idle_setting_generation);
        store.add_table_change_observer(ObservedTable::OrchestrationSettings, move || {
            if let Some(generation) = observed_idle_setting.upgrade() {
                generation.send_modify(|value| *value = value.wrapping_add(1));
            }
        });
        let (agent_generation, _) = watch::channel(0_u64);
        let agent_generation = Arc::new(agent_generation);
        let observed_agents = Arc::downgrade(&agent_generation);
        store.add_table_change_observer(ObservedTable::Agents, move || {
            if let Some(generation) = observed_agents.upgrade() {
                generation.send_modify(|value| *value = value.wrapping_add(1));
            }
        });
        let agent_wakes = Arc::new(agent_wakes::AgentWakes::default());
        let observed_wakes = Arc::downgrade(&agent_wakes);
        store.add_table_row_change_observer(ObservedTable::Agents, move |change| {
            if let Some(wakes) = observed_wakes.upgrade() {
                wakes.committed(change);
            }
        });
        let bridge = Self {
            store,
            accounts: Arc::default(),
            recovered_accounts: Arc::default(),
            event_wake: Arc::new(std::sync::RwLock::new(Arc::new(Notify::new()))),
            continuation_wake: Arc::new(std::sync::RwLock::new(Arc::new(Notify::new()))),
            monitor_dispatch_wake: Arc::new(std::sync::RwLock::new(Arc::new(Notify::new()))),
            idle_state_generation,
            idle_setting_generation,
            agent_generation,
            agent_wakes,
            start_gates: Arc::default(),
            managed_tool_runtime: Arc::new(std::sync::RwLock::new(None)),
            #[cfg(test)]
            disk_gate: None,
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
    fn idle_state_changes(&self) -> watch::Receiver<u64> {
        self.idle_state_generation.subscribe()
    }
    fn idle_setting_changes(&self) -> watch::Receiver<u64> {
        self.idle_setting_generation.subscribe()
    }
    fn agent_changes(&self) -> watch::Receiver<u64> {
        self.agent_generation.subscribe()
    }
    async fn run<T, F, Fut>(&self, operation: F) -> Result<T, WorkerError>
    where
        T: Send + 'static,
        F: FnOnce(Arc<RuntimeStore>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, WorkerError>> + Send + 'static,
    {
        // Store operation futures are large; keep them off the worker stack.
        Box::pin(operation(self.store.clone())).await
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
        let result = operation(self.store.clone()).await;
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
        #[cfg(test)]
        drop(self.faults.operation_gate.read().await);
        let result = self.run(operation).await;
        #[cfg(test)]
        self.faults.pause_after(index).await;
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
    /// Observe command dispatch without waiting for detached preparations.
    #[cfg(test)]
    DispatchBarrier(tokio::sync::oneshot::Sender<usize>),
    /// Re-evaluate idle candidates as if a durable blocker commit notified the worker.
    #[cfg(test)]
    IdleStateChange(tokio::time::Instant),
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
    idle_exit: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(test)]
    faults: Arc<tests::Faults>,
    #[cfg(test)]
    session_control: SessionControl,
}
impl AccountHandle {
    pub fn completion_signal(&self) -> watch::Receiver<bool> {
        self.finished.clone()
    }
    /// Whether the account worker closed normally after its idle deadline.
    pub fn planned_idle_exit(&self) -> bool {
        self.idle_exit.load(std::sync::atomic::Ordering::Acquire)
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
    let restart = !bridge
        .recovered_accounts
        .lock()
        .map_err(|_| WorkerError::TaskFailed)?
        .contains(&account);
    let recovered = loop {
        let recovery_account = account.clone();
        match bridge
            .worker_run("recover", move |store| async move {
                store::recover(&store, &recovery_account, generation, restart).await
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
    bridge
        .recovered_accounts
        .lock()
        .map_err(|_| WorkerError::TaskFailed)?
        .insert(account.clone());
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
    let (effects, callback_scan, streams) = bridge
        .worker_run("restore_effects_streams", move |store| async move {
            let read_account = effect_account.clone();
            let (effects, callback_scan) = store
                .read_async("runtime-services/account_worker_read", move |connection| {
                    Ok::<_, WorkerError>((
                        store::recover_effects(connection, &read_account)?,
                        callback_record::any_unsent(connection, &read_account, generation.0)?,
                    ))
                })
                .await?;
            let streams = store::restore_streams(&store, &effect_account).await?;
            Ok((effects, callback_scan, streams))
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
    let vm_credential_refresh_at = None;
    let idle_state_changes = bridge.idle_state_changes();
    let idle_setting_changes = bridge.idle_setting_changes();
    let idle_exit = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker = Worker {
        resources: Arc::new(resources),
        bridge: bridge.clone(),
        account: account.clone(),
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
        request_waiters: BTreeMap::new(),
        browser_skill_roots: None,
        idle_clocks: idle_unload::IdleClocks::default(),
        idle_delay: None,
        idle_deadline: None,
        idle_initialized: false,
        idle_state_changes,
        idle_setting_changes,
        idle_exit: Arc::clone(&idle_exit),
        stop: stop.subscribe(),
        inbox,
        commands: commands.clone(),
        retained_session: retained::RetainedSessionPort {
            scope: scope.clone(),
            commands: commands.clone(),
            stop: stop.clone(),
        },
        queued_commands: VecDeque::new(),
        resume_preparations: true,
        preparation_waits: BTreeMap::new(),
        preparations: JoinSet::new(),
        preparation_aborts: Vec::new(),
        preparing: BTreeMap::new(),
        thread_proofs: BTreeMap::new(),
        thread_tokens: BTreeMap::new(),
        effect_in_hand: None,
        idle_snapshot: None,
        #[cfg(test)]
        barriers: Vec::new(),
        pending: recovered
            .into_iter()
            .map(|(context, record)| (record.ticket.id, Arc::new(context)))
            .collect(),
        flush_at: (streams.pending_bytes() > 0)
            .then(|| tokio::time::Instant::now() + STREAM_CADENCE),
        streams,
        deferred: VecDeque::new(),
        capacity_blocked: VecDeque::new(),
        capacity_retry_at: None,
        reported_faults: BTreeSet::new(),
        vm_notification_waiters: BTreeMap::new(),
        callbacks: BTreeMap::new(),
        managed_callback_cursors: BTreeSet::new(),
        callback_parked: BTreeSet::new(),
        callback_scan,
        retained_requests: retained
            .into_iter()
            .map(|(pending, request)| (request.ticket.id, pending))
            .collect(),
        retained_barriers: Vec::new(),
        retained_frames: BTreeMap::new(),
        effects,
        publication_sink,
        publication_retry_at: Some(tokio::time::Instant::now()),
        publication_drain: None,
        delivered_publications: Vec::new(),
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
        idle_exit,
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
    /// The cursor and provider generation of the request this frame answers.
    #[serde(skip)]
    Callback(
        studio_runtime_process::EventCursor,
        Generation,
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
    Generation,
    Option<String>,
    Option<i64>,
    Option<Frame>,
);
/// A callback record held by this worker: provider generation, emitting VM
/// actor, its epoch and the journal cursor.
type CallbackKey = (u64, Option<String>, Option<i64>, u64);
type CallbackCursor = (
    studio_runtime_process::EventCursor,
    Option<String>,
    Option<i64>,
);
struct Worker {
    retained_requests: BTreeMap<RequestId, retained::PendingRequest>,
    retained_frames: BTreeMap<String, retained_frame::PendingFrame>,
    retained_barriers: Vec<retained::PendingBarrier>,
    /// Shared with every preparation and outcome task; never deep-copied per request.
    resources: Arc<preparation::PreparationResources>,
    bridge: StoreBridge,
    account: String,
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
    /// Replies to requests the worker sends for itself, by operation.
    request_waiters: BTreeMap<String, tokio::sync::oneshot::Sender<RequestOutcome>>,
    /// Skill roots this connection confirmed for `skills/extraRoots/set`;
    /// `None` until a reply confirms them.
    browser_skill_roots: Option<Vec<String>>,
    idle_clocks: idle_unload::IdleClocks,
    idle_delay: Option<std::time::Duration>,
    idle_deadline: Option<tokio::time::Instant>,
    idle_initialized: bool,
    idle_state_changes: watch::Receiver<u64>,
    idle_setting_changes: watch::Receiver<u64>,
    idle_exit: Arc<std::sync::atomic::AtomicBool>,
    stop: watch::Receiver<bool>,
    inbox: mpsc::Receiver<AccountCommand>,
    commands: mpsc::Sender<AccountCommand>,
    retained_session: retained::RetainedSessionPort,
    queued_commands: VecDeque<AccountCommand>,
    /// Resume durable pending preparations once, after the session is ready.
    resume_preparations: bool,
    /// One event-driven wake per agent whose preparation is pending.
    preparation_waits: BTreeMap<String, tokio::task::AbortHandle>,
    /// Concurrent per-agent turn preparations (see `turn_preparation`).
    preparations: JoinSet<Box<turn_preparation::Done>>,
    /// The preparations in `preparations` that shutdown aborts: claims and
    /// preparations, never a commit.
    preparation_aborts: Vec<tokio::task::AbortHandle>,
    /// Agents with a preparation in flight, and the epoch of a wake that
    /// arrived meanwhile (re-run when that preparation is handled).
    preparing: BTreeMap<String, Option<i64>>,
    /// Threads this connection loaded, by agent. A proof is published after
    /// its acceptance commits and dropped when the thread may be unloaded.
    thread_proofs: BTreeMap<String, crate::thread_prepare::LoadedIdentity>,
    /// Workspace token of each agent's last submitted thread request, by
    /// attempt; that attempt's turn reuses it (see `turn_preparation::Run`).
    thread_tokens: BTreeMap<String, (String, crate::workspace_ready::ReadyToken)>,
    /// Durable key of the effect being run; a spawned preparation takes it and
    /// completes it once handled.
    effect_in_hand: Option<String>,
    /// In-flight durable idle re-check (see `idle_unload::IdleSnapshotTask`).
    idle_snapshot: Option<idle_unload::IdleSnapshotTask>,
    #[cfg(test)]
    barriers: Vec<tokio::sync::oneshot::Sender<usize>>,
    /// Written requests by ticket; shared with their outcome tasks, never deep-copied.
    pending: BTreeMap<RequestId, Arc<Pending>>,
    streams: NotificationStreamBuffer,
    flush_at: Option<tokio::time::Instant>,
    /// Journal entries that wait for a reply to bind them, with their VM
    /// source and the agent an unbound completion belongs to.
    deferred: VecDeque<(JournalEntry, Option<VmActorIdentity>, Option<String>)>,
    /// Journal entries that a full monitor output queue refused, and every
    /// entry that arrived after them, in journal order. While it is not empty
    /// the worker reads no provider event, so the journal order holds.
    capacity_blocked: VecDeque<CapacityBlocked>,
    /// When `capacity_blocked` is journaled again.
    capacity_retry_at: Option<tokio::time::Instant>,
    /// Agents whose isolated data fault this worker already logged.
    reported_faults: BTreeSet<String>,
    vm_notification_waiters:
        BTreeMap<(String, u64), tokio::sync::oneshot::Sender<Result<(), retained::PortError>>>,
    effects: VecDeque<Effect>,
    publication_sink: Arc<dyn crate::notification_runtime_effects::NotificationPostCommitSink>,
    publication_retry_at: Option<tokio::time::Instant>,
    /// The single in-flight drain of this account's durable publication receipts.
    publication_drain: Option<tokio::task::JoinHandle<Result<store::PublicationPass, WorkerError>>>,
    /// Delivered receipts the next journal or stream flush transaction deletes.
    delivered_publications: Vec<store::DeliveredPublication>,
    closing: bool,
    callbacks: BTreeMap<String, CallbackCursor>,
    /// Accepted callback records this worker already started or answers.
    managed_callback_cursors: BTreeSet<CallbackKey>,
    /// Records of VM actors that have not reattached since this worker started.
    callback_parked: BTreeSet<CallbackKey>,
    /// Whether accepted callback records may be waiting to start: set by a
    /// journal commit, a finished handler or answer, and at startup.
    callback_scan: bool,
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

/// One journal entry that waits for monitor output queue room, with the
/// arguments of the journal call that refused it.
struct CapacityBlocked {
    entry: JournalEntry,
    source: Option<VmActorIdentity>,
    settle: bool,
    park_unbound: bool,
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
    PublicationDrained(Result<Result<store::PublicationPass, WorkerError>, tokio::task::JoinError>),
    VmCredentialRefresh,
    Command(Option<AccountCommand>),
    Event(Option<SessionEvent>),
    VmEvent(Option<VmForwardedMessage>),
    ConnectionRecovery,
    ManagedCallback(Option<Result<ManagedCallbackResult, tokio::task::JoinError>>),
    Prepared(Option<Result<Box<turn_preparation::Done>, tokio::task::JoinError>>),
    SessionStatusChanged(bool),
    IdleSweep,
    IdleStateChanged(Result<(), watch::error::RecvError>),
    IdleSnapshot(Result<idle_unload::IdleSnapshotResult, tokio::task::JoinError>),
    IdleSettingChanged(Result<(), watch::error::RecvError>),
    CapacityRetry,
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
        if let Some(drain) = self.publication_drain.take() {
            // Receipts are deleted only after delivery, so a cut-off pass replays.
            drain.abort();
            let _joined = drain.await;
        }
        self.connection_recovery.abort_all();
        for wait in std::mem::take(&mut self.preparation_waits).into_values() {
            wait.abort();
        }
        // An unsubmitted preparation holds no provider evidence; its claim is
        // recovered like any other unsubmitted attempt.
        if let Some(snapshot) = self.idle_snapshot.take() {
            snapshot.abort();
        }
        for preparation in std::mem::take(&mut self.preparation_aborts) {
            preparation.abort();
        }
        // A commit is not aborted: its FULL `submitted` commit may already
        // have returned, or still run in the writer after its caller is gone.
        // Each finished commit settles the request it never wrote.
        let closing = std::mem::replace(&mut self.closing, true);
        let mut commit_result = Ok(());
        while let Some(done) = self.preparations.join_next().await {
            if let Ok(done) = done
                && let Ok(turn_preparation::Outcome::Committed(submission::Committed::Ready(ready))) =
                    done.outcome
            {
                let submission::Ready {
                    id,
                    context,
                    method,
                    ..
                } = *ready;
                if let Err(error) = self.settle_unwritten(id, context, method).await {
                    commit_result = commit_result.and(Err(error));
                }
            }
        }
        self.closing = closing;
        let result = result.and(commit_result);
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
            Err(WorkerError::Session(SessionTaskError::Closed))
                if *self.stop.borrow()
                    || self.idle_exit.load(std::sync::atomic::Ordering::Acquire) =>
            {
                Ok(())
            }
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
                .flush_at
                .is_some_and(|at| at <= tokio::time::Instant::now())
            {
                self.flush().await?;
            }
            if self.resume_preparations
                && self.session.status.borrow().initialization == InitializationStatus::Ready
            {
                self.resume_preparations = false;
                self.resume_pending_preparations().await?;
                continue;
            }
            if self.callback_scan {
                self.start_callbacks().await?;
            }
            if self.session.status.borrow().initialization == InitializationStatus::Ready
                && let Some(effect) = self.effects.pop_front()
            {
                self.run_effect(effect).await?;
                continue;
            }
            if self.idle_delay.is_some()
                && self.session.status.borrow().initialization == InitializationStatus::Ready
                && self.idle_clocks.has_local_blockers()
            {
                self.idle_local_work_completed().await?;
                if self.closing {
                    continue;
                }
            }
            if self.session.status.borrow().initialization == InitializationStatus::Ready
                && !self.idle_initialized
                && self.idle_deadline.is_none()
                && self.pending.is_empty()
                && self.retained_requests.is_empty()
                && self.effects.is_empty()
                && !self.resume_preparations
                && self.preparation_waits.is_empty()
                && self.preparations.is_empty()
                && self.queued_commands.is_empty()
                && self.inbox.is_empty()
            {
                self.idle_initialized = true;
                self.idle_initialize(tokio::time::Instant::now()).await?;
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
        self.effect_in_hand = store::effect_key(&self.account, &effect);
        match effect {
            Effect::Callback(cursor, generation, frame, actor, epoch) => {
                let emitter = actor.clone();
                match self
                    .answer_callback(cursor, generation, frame, actor, epoch)
                    .await
                {
                    Err(WorkerError::StaleVmActor) => {
                        // No answer can reach the retired actor.
                        self.abandon_callback(cursor, generation, emitter.as_deref(), epoch)
                            .await?;
                        return Ok(());
                    }
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
        if let Some(key) = self.effect_in_hand.take() {
            self.complete_effect(key).await?;
        }
        Ok(())
    }
    async fn complete_effect(&mut self, key: String) -> Result<(), WorkerError> {
        if !self.closing {
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
            // Commands are bounded (COMMAND_CAPACITY) and a wake only spawns its
            // claim, so a wake never waits behind other agents' submissions.
            command = async { match self.queued_commands.pop_front() { Some(command) => Some(command), None => self.inbox.recv().await } }, if self.session.status.borrow().initialization == InitializationStatus::Ready => ServeSignal::Command(command),
            // A finished preparation is a one-shot result, so it cannot starve the
            // unbounded provider event stream below; the reverse order did.
            prepared = self.preparations.join_next(), if !self.preparations.is_empty() => ServeSignal::Prepared(prepared),
            _ = tokio::time::sleep_until(self.flush_at.unwrap_or_else(||tokio::time::Instant::now()+STREAM_CADENCE)), if self.flush_at.is_some() => ServeSignal::Flush,
            _ = tokio::time::sleep_until(self.publication_retry_at.unwrap_or_else(||tokio::time::Instant::now()+STREAM_CADENCE)), if self.publication_retry_at.is_some() && self.publication_drain.is_none() => ServeSignal::PublicationRetry,
            done = async { match self.publication_drain.as_mut() { Some(task) => task.await, None => std::future::pending().await } }, if self.publication_drain.is_some() => ServeSignal::PublicationDrained(done),
            _ = tokio::time::sleep_until(self.vm_credential_refresh_at.unwrap_or_else(tokio::time::Instant::now)), if self.vm_credential_refresh_at.is_some() && self.session.status.borrow().initialization == InitializationStatus::Ready => ServeSignal::VmCredentialRefresh,
            _ = tokio::time::sleep_until(self.capacity_retry_at.unwrap_or_else(tokio::time::Instant::now)), if self.capacity_retry_at.is_some() => ServeSignal::CapacityRetry,
            // Refused entries keep their journal position: read nothing behind them.
            event = self.events.recv(), if self.capacity_blocked.is_empty() => ServeSignal::Event(event),
            vm_event = self.vm_events.recv(), if self.capacity_blocked.is_empty() => ServeSignal::VmEvent(vm_event),
            _ = self.connection_recovery.join_next(), if !self.connection_recovery.is_empty() => ServeSignal::ConnectionRecovery,
            callback = self.managed_callbacks.join_next(), if !self.managed_callbacks.is_empty() => ServeSignal::ManagedCallback(callback),
            changed = self.session.status.changed() => ServeSignal::SessionStatusChanged(changed.is_ok()),
            _ = tokio::time::sleep_until(self.idle_deadline.unwrap_or_else(tokio::time::Instant::now)), if self.idle_deadline.is_some() => ServeSignal::IdleSweep,
            changed = self.idle_state_changes.changed(), if self.idle_snapshot.is_none() && self.idle_clocks.has_recheck_candidates() => ServeSignal::IdleStateChanged(changed),
            snapshot = async { match self.idle_snapshot.as_mut() { Some(task) => task.await, None => std::future::pending().await } }, if self.idle_snapshot.is_some() => ServeSignal::IdleSnapshot(snapshot),
            changed = self.idle_setting_changes.changed() => ServeSignal::IdleSettingChanged(changed),
        }
    }
    async fn dispatch_serve_signal(&mut self, signal: ServeSignal) -> Result<bool, WorkerError> {
        match signal {
            ServeSignal::Shutdown => self.closing = true,
            ServeSignal::Flush => self.flush().await?,
            ServeSignal::PublicationRetry => self.start_publication_drain(),
            ServeSignal::PublicationDrained(done) => self.publication_drained(done)?,
            ServeSignal::VmCredentialRefresh => self.refresh_vm_claude_credentials().await?,
            ServeSignal::Command(Some(command)) => {
                self.dispatch_command(command).await?;
            }
            ServeSignal::Command(None) | ServeSignal::Event(None) => return Ok(false),
            ServeSignal::Event(Some(event)) => self.event(event).await?,
            ServeSignal::VmEvent(Some(event)) => self.handle_vm_message(event).await?,
            ServeSignal::VmEvent(None) => return Ok(false),
            ServeSignal::ConnectionRecovery => self.connection_recovery_active = false,
            ServeSignal::ManagedCallback(callback) => {
                let (cursor, generation, actor, epoch, response) = callback
                    .ok_or(WorkerError::TaskFailed)?
                    .map_err(|_| WorkerError::TaskFailed)?;
                if let Some(response) = response {
                    self.push(Effect::Callback(cursor, generation, response, actor, epoch));
                }
                self.callback_scan = true;
            }
            ServeSignal::Prepared(done) => {
                // A preparation is part of this worker: its panic is the worker's.
                let done = match done.ok_or(WorkerError::TaskFailed)? {
                    Ok(done) => done,
                    Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                    Err(_) => return Err(WorkerError::TaskFailed),
                };
                Box::pin(self.prepared(*done)).await?;
                #[cfg(test)]
                self.release_barriers();
            }
            ServeSignal::SessionStatusChanged(false) => return Ok(false),
            ServeSignal::SessionStatusChanged(true) => {
                if idle_unload::session_status_invalidates_snapshot(
                    &self.session.status.borrow(),
                    self.generation,
                ) {
                    self.idle_initialized = false;
                    self.idle_clocks = idle_unload::IdleClocks::default();
                    self.idle_deadline = None;
                    if let Some(snapshot) = self.idle_snapshot.take() {
                        snapshot.abort();
                    }
                }
            }
            ServeSignal::IdleSweep => {
                if self.idle_initialized {
                    self.idle_sweep(tokio::time::Instant::now()).await?;
                } else {
                    self.idle_initialize(tokio::time::Instant::now()).await?;
                }
            }
            ServeSignal::IdleStateChanged(changed) => {
                changed.map_err(|_| WorkerError::TaskFailed)?;
                self.idle_state_changed(tokio::time::Instant::now());
            }
            ServeSignal::IdleSnapshot(snapshot) => {
                self.idle_snapshot = None;
                let (now, blocked, usage) = snapshot.map_err(|_| WorkerError::TaskFailed)??;
                self.idle_snapshot_ready(now, blocked, usage).await?;
                #[cfg(test)]
                self.release_barriers();
            }
            ServeSignal::IdleSettingChanged(changed) => {
                changed.map_err(|_| WorkerError::TaskFailed)?;
                let _ = self.idle_setting_changes.borrow_and_update();
                self.idle_setting_changed(tokio::time::Instant::now())
                    .await?;
            }
            ServeSignal::CapacityRetry => {
                self.capacity_retry_at = None;
                self.journal_capacity_blocked().await?;
            }
        }
        Ok(true)
    }
    async fn dispatch_command(&mut self, command: AccountCommand) -> Result<(), WorkerError> {
        match command {
            #[cfg(test)]
            AccountCommand::DispatchBarrier(reply) => {
                let _sent = reply.send(
                    self.bridge
                        .faults
                        .journal_count
                        .load(std::sync::atomic::Ordering::SeqCst),
                );
            }
            #[cfg(test)]
            AccountCommand::Barrier(reply) => {
                // A test barrier also waits for in-flight preparations.
                self.barriers.push(reply);
                self.release_barriers();
            }
            #[cfg(test)]
            AccountCommand::IdleStateChange(now) => self.idle_state_changed(now),
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
                        let (response, streams) = match connection_recovery::run(request).await {
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
                    self.schedule_publication_drain()?;
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
                self.schedule_publication_drain()?;
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
                self.schedule_publication_drain()?;
            }
        }
        Ok(())
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
    /// Commit provider events while the worker waits for its own request.
    /// Other commands wait in the queue, so none of them can send a request
    /// to the provider before `future` completes.
    async fn pump_until<T>(&mut self, future: impl Future<Output = T>) -> Result<T, WorkerError> {
        tokio::pin!(future);
        loop {
            tokio::select! {
                _ = Shutdown::cancelled(&mut self.stop) => return Err(WorkerError::Closed),
                output = &mut future => return Ok(output),
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
        }
    }
    /// Durable pending preparations survive a worker restart; wake each once.
    /// Each one that is still blocked registers its own event-driven wake.
    async fn resume_pending_preparations(&mut self) -> Result<(), WorkerError> {
        let account = self.account.clone();
        let pending = self
            .db("pending_preparations", move |store| async move {
                store
                    .read_async("runtime-services/account_worker_read", move |connection| {
                        store::pending_preparations(connection, &account)
                    })
                    .await
            })
            .await?;
        for (agent, epoch) in pending {
            self.wake(agent, epoch).await?;
        }
        Ok(())
    }
    /// Wake a pending preparation when the event that can unblock it happens:
    /// a published catalog refresh, or a committed change of the agent rows it
    /// waits for. `since` was read before the attempt, so a change committed
    /// while it ran is not missed.
    fn wait_for_preparation(
        &mut self,
        intent: &TurnIntent,
        pending: &store::PendingWait,
        since: u64,
    ) {
        let commands = self.commands.clone();
        let agent_id = intent.agent_id.clone();
        let epoch = intent.epoch;
        let catalog = (pending.reason == preparation::CATALOG_PENDING_REASON).then(|| {
            (
                self.resources.model_catalog.clone(),
                self.account.clone(),
                self.resources.account_home.clone(),
                intent.provider.key().to_owned(),
            )
        });
        let agents = catalog
            .is_none()
            .then(|| self.bridge.agent_wakes.wait(since, pending.rows));
        let task = tokio::spawn(async move {
            match (catalog, agents) {
                (Some((catalog, account, home, provider)), _) => {
                    catalog.refreshed(&account, &home, &provider).await;
                }
                (None, Some(agents)) => {
                    if agents.await.is_err() {
                        return;
                    }
                }
                (None, None) => return,
            }
            let _closed = commands
                .send(AccountCommand::Wake { agent_id, epoch })
                .await;
        });
        if let Some(previous) = self
            .preparation_waits
            .insert(intent.agent_id.clone(), task.abort_handle())
        {
            previous.abort();
        }
    }
    /// Wake a queued agent again after the disk-full recheck interval.
    fn wait_for_disk_space(&mut self, agent_id: String, epoch: i64) {
        let commands = self.commands.clone();
        let key = agent_id.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(crate::disk_space::RECHECK).await;
            let _closed = commands
                .send(AccountCommand::Wake { agent_id, epoch })
                .await;
        });
        if let Some(previous) = self.preparation_waits.insert(key, task.abort_handle()) {
            previous.abort();
        }
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
        self.vm_credential_refresh_at =
            Some(tokio::time::Instant::now() + VM_CREDENTIAL_REFRESH_INTERVAL);
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
    /// A pending step registers its wake; a settled (refused) step is final.
    fn preparation_ready<T>(
        &mut self,
        step: store::PreparationStep<T>,
        intent: &TurnIntent,
        since: u64,
    ) -> Option<T> {
        match step {
            store::PreparationStep::Ready(value) => Some(value),
            store::PreparationStep::Pending(pending) => {
                self.wait_for_preparation(intent, &pending, since);
                None
            }
            store::PreparationStep::Settled | store::PreparationStep::Retry => None,
        }
    }
    async fn wake(&mut self, agent: String, epoch: i64) -> Result<(), WorkerError> {
        if let Some(wait) = self.preparation_waits.remove(&agent) {
            wait.abort();
        }
        if self.closing {
            return Ok(());
        }
        // One preparation per agent; a wake meanwhile re-runs after it.
        if let Some(rewake) = self.preparing.get_mut(&agent) {
            *rewake = Some(epoch);
            return Ok(());
        }
        self.preparing.insert(agent.clone(), None);
        let start = self.preparation(&agent, Method::ThreadResume);
        let wake = turn_preparation::Wake {
            bridge: self.bridge.clone(),
            driver: self.resources.account_driver.clone(),
            account: self.account.clone(),
            agent_id: agent,
            epoch,
            effect: self.effect_in_hand.take(),
            resources: self.resources.clone(),
        };
        let preparation = self.preparations.spawn(turn_preparation::wake(wake, start));
        self.track_preparation(preparation);
        Ok(())
    }
    fn submit<'a>(
        &'a mut self,
        intent: TurnIntent,
        method: Method,
        interrupt_turn: Option<(String, i64)>,
    ) -> Pin<Box<impl Future<Output = Result<(), WorkerError>> + Send + 'a>> {
        Box::pin(self.submit_inner(intent, method, interrupt_turn))
    }
    async fn submit_inner(
        &mut self,
        intent: TurnIntent,
        method: Method,
        interrupt_turn: Option<(String, i64)>,
    ) -> Result<(), WorkerError> {
        if self.closing {
            return Ok(());
        }
        if let Some(thread) = intent.thread_id.as_deref() {
            self.idle_thread_touched(thread);
        }
        if method == Method::TurnInterrupt {
            let commit = self.commit(intent, method, interrupt_turn, None, false);
            let committed = Box::pin(self.drive(commit.run())).await?;
            return Box::pin(self.committed(committed)).await;
        }
        // A recovered durable effect may outlive its submitted request. Check
        // that receipt before later events can settle it, and before
        // preparation, which requires an unsubmitted owner.
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
        self.preparing.insert(intent.agent_id.clone(), None);
        let start = self.preparation(&intent.agent_id, method);
        let run = start(intent, self.effect_in_hand.take());
        let preparation = self.preparations.spawn(turn_preparation::run(run));
        self.track_preparation(preparation);
        Ok(())
    }
    fn track_preparation(&mut self, preparation: tokio::task::AbortHandle) {
        self.preparation_aborts
            .retain(|preparation| !preparation.is_finished());
        self.preparation_aborts.push(preparation);
    }
    /// The preparation of `method` for one agent. The worker state it needs is
    /// taken now; the intent may come later, from a wake's claim.
    fn preparation(
        &mut self,
        agent_id: &str,
        method: Method,
    ) -> impl FnOnce(TurnIntent, Option<String>) -> turn_preparation::Run + Send + use<> {
        let bridge = self.bridge.clone();
        let resources = self.resources.clone();
        let generation = self.generation.0;
        let connection_id = self.connection_id.clone();
        let since = self.bridge.agent_wakes.sequence();
        let loaded = (method == Method::ThreadResume)
            .then(|| self.thread_proofs.get(agent_id).cloned())
            .flatten();
        let carried = self.thread_tokens.remove(agent_id);
        move |intent, effect| turn_preparation::Run {
            bridge,
            resources,
            generation,
            connection_id,
            since,
            effect,
            loaded,
            carried: carried
                .filter(|(attempt, _)| method == Method::TurnStart && *attempt == intent.attempt_id)
                .map(|(_, token)| token),
            intent,
            method,
        }
    }
    #[cfg(test)]
    fn release_barriers(&mut self) {
        if self.preparations.is_empty() && self.idle_snapshot.is_none() {
            for reply in std::mem::take(&mut self.barriers) {
                let _ignored = reply.send(
                    self.bridge
                        .faults
                        .journal_count
                        .load(std::sync::atomic::Ordering::SeqCst),
                );
            }
        }
    }
    /// The provider may have unloaded `thread` or changed its settings.
    fn forget_thread_proofs(&mut self, thread: &str) {
        self.thread_proofs
            .retain(|_, proof| proof.thread_id != thread);
    }
    /// Handle one finished preparation on the serialized worker.
    async fn prepared(&mut self, done: turn_preparation::Done) -> Result<(), WorkerError> {
        let turn_preparation::Done {
            agent_id,
            effect,
            outcome,
        } = done;
        let mut rewake = self.preparing.remove(&agent_id).flatten();
        let outcome = match outcome {
            // The agent's own data failed its preparation or commit, whose
            // transaction rolled back: nothing was submitted. Hold the agent,
            // and settle its effect so a restart does not run it again.
            Err(error) if let Some(record) = agent_fault::data_fault(&error) => {
                let detail = error.to_string();
                let Some(fault) = self
                    .db("hold_after_fault", move |store| async move {
                        agent_fault::hold_after(
                            &store,
                            agent_id,
                            agent_fault::FaultStage::TurnPreparation,
                            record,
                            detail,
                        )
                        .await
                    })
                    .await?
                else {
                    return Err(error);
                };
                fault.report(&self.account, &mut self.reported_faults);
                if let Some(key) = effect {
                    self.complete_effect(key).await?;
                }
                return Ok(());
            }
            outcome => outcome?,
        };
        match outcome {
            turn_preparation::Outcome::Prepared(prepared) => {
                let turn_preparation::Prepared {
                    intent,
                    method: requested,
                    since,
                    loaded,
                    step,
                } = *prepared;
                // A wake's claim ran in its task; its thread is in use from here.
                if let Some(thread) = intent.thread_id.as_deref() {
                    self.idle_thread_touched(thread);
                }
                if matches!(step, store::PreparationStep::Retry) {
                    rewake = Some(intent.epoch);
                }
                if let Some(prepared) = self.preparation_ready(step, &intent, since) {
                    let method = prepared.method;
                    // A proof forgotten while this preparation ran (an unload or
                    // thread/closed) no longer shows the thread loaded: prepare
                    // again from the current proofs. Nothing was committed yet.
                    if requested == Method::ThreadResume
                        && method == Method::TurnStart
                        && loaded.as_ref() != self.thread_proofs.get(&agent_id)
                    {
                        self.effect_in_hand = effect;
                        Box::pin(self.submit(intent, Method::ThreadResume, None)).await?;
                        if let Some(epoch) = rewake {
                            self.wake(agent_id, epoch).await?;
                        }
                        return Ok(());
                    }
                    if prepared.thread.is_some() {
                        self.thread_tokens.insert(
                            prepared.intent.agent_id.clone(),
                            (
                                prepared.intent.attempt_id.clone(),
                                prepared.workspace_token(),
                            ),
                        );
                    }
                    // `submit_inner` already read this turn/start receipt before preparing.
                    let receipt_checked =
                        requested == Method::TurnStart && method == Method::TurnStart;
                    // A closing worker commits nothing new.
                    if !self.closing {
                        // The FULL submitted commit runs in this agent's task; the
                        // agent stays in preparation until its write is handled.
                        let commit = self.commit(
                            prepared.intent.clone(),
                            method,
                            None,
                            Some(prepared),
                            receipt_checked,
                        );
                        // A host thread request with browser guidance needs the
                        // skill root registered on this connection first; VM
                        // and Claude agents get no browser guidance.
                        if let Some(browser) = commit
                            .context
                            .thread
                            .as_ref()
                            .and_then(|attempt| attempt.snapshot.guidance.browser.as_ref())
                        {
                            self.ensure_browser_skill(browser.skill_roots.clone())
                                .await?;
                        }
                        if !self.closing {
                            self.preparing.insert(agent_id.clone(), rewake);
                            self.preparations
                                .spawn(turn_preparation::commit(agent_id, effect, commit));
                            return Ok(());
                        }
                    }
                }
            }
            turn_preparation::Outcome::Unclaimed(turn_preparation::Unclaimed::Steer(
                intent,
                turn,
                prepared,
            )) => Box::pin(self.steer(*intent, turn, *prepared)).await?,
            turn_preparation::Outcome::Unclaimed(turn_preparation::Unclaimed::DiskFull(epoch)) => {
                self.wait_for_disk_space(agent_id.clone(), epoch);
            }
            turn_preparation::Outcome::Unclaimed(
                turn_preparation::Unclaimed::Nothing | turn_preparation::Unclaimed::NativeReview,
            ) => {}
            turn_preparation::Outcome::Committed(committed) => {
                // Its submitted commit may already bind a turn completion
                // that arrives before `write` records the request as pending.
                self.preparing.insert(agent_id.clone(), None);
                let written = Box::pin(self.committed(committed)).await;
                rewake = rewake.or(self.preparing.remove(&agent_id).flatten());
                written?;
            }
        }
        if let Some(key) = effect {
            self.complete_effect(key).await?;
        }
        if let Some(epoch) = rewake {
            self.wake(agent_id, epoch).await?;
        }
        Ok(())
    }
    /// Build the commit of one submission. `receipt_checked` means this
    /// worker already found no submitted receipt for the operation; only this
    /// agent's single in-flight preparation can submit it in between.
    fn commit(
        &self,
        intent: TurnIntent,
        method: Method,
        interrupt_turn: Option<(String, i64)>,
        prepared: Option<preparation::RequestPreparation>,
        receipt_checked: bool,
    ) -> submission::Commit {
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
            let params =
                json!({"threadId":intent.thread_id,"turnId":interrupt_turn.as_ref().map(|(t,_)|t)});
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
        submission::Commit {
            bridge: self.bridge.clone(),
            account: self.account.clone(),
            connection_id: self.connection_id.clone(),
            generation: self.generation.0,
            timeout_ms: self.timeout_ms,
            control: self.control.clone(),
            id: RequestId::String(operation.clone()),
            context: Pending {
                thread,
                compiled,
                intent,
                method: method.as_str().into(),
                operation,
            },
            method,
            params,
            interrupt_turn,
            submission,
            guard,
            receipt_checked,
            effect: None,
        }
    }
    /// Finish a commit on the worker: a VM actor first opens its own session
    /// and commits on it here; every committed request is then written.
    async fn committed(&mut self, committed: submission::Committed) -> Result<(), WorkerError> {
        let ready = match committed {
            submission::Committed::Nothing => return Ok(()),
            submission::Committed::Retry(agent, epoch) => return self.wake(agent, epoch).await,
            submission::Committed::Ready(ready) => Some(ready),
            // Boxed: the VM path stays out of this future, which every
            // submission polls on the worker's stack.
            submission::Committed::Vm(commit, launch) => {
                Box::pin(self.committed_vm(*commit, launch)).await?
            }
        };
        match ready {
            Some(ready) => Box::pin(self.write(*ready)).await,
            None => Ok(()),
        }
    }
    /// Open the VM actor's session and finish its commit; the request to
    /// write, if any.
    async fn committed_vm(
        &mut self,
        commit: submission::Commit,
        launch: Result<Option<preparation::VmProviderLaunchRequest>, WorkerError>,
    ) -> Result<Option<Box<submission::Ready>>, WorkerError> {
        let method = commit.method;
        let actor_session = match self.ensure_vm_session(launch).await {
            Ok(session) => session,
            Err(WorkerError::PreparationRefused(reason)) if method != Method::TurnInterrupt => {
                let blocked_intent = commit.context.intent.clone();
                let resources = self.resources.clone();
                let _settled = self
                    .db("vm-session-identity-blocked", move |store| async move {
                        store::preparation_step(&store, &resources, &blocked_intent, async move {
                            Err::<(), WorkerError>(WorkerError::PreparationRefused(reason))
                        })
                        .await
                    })
                    .await?;
                return Ok(None);
            }
            // Stop already committed the actor's new epoch and disabled
            // wake. A stale provider session must not receive an interrupt
            // under the replacement identity; retain the existing receipt
            // state for recovery instead of failing the account worker.
            Err(WorkerError::PreparationRefused(_)) => return Ok(None),
            Err(WorkerError::StaleVmActor) => return Ok(None),
            Err(error) => return Err(error),
        };
        let control = actor_session
            .as_deref()
            .and_then(|agent| {
                self.vm_sessions
                    .get(agent)
                    .map(|session| session.control.clone())
            })
            .unwrap_or_else(|| self.control.clone());
        let vm = actor_session.is_some();
        Ok(
            match Box::pin(self.drive(commit.finish(control, vm))).await? {
                submission::Committed::Ready(ready) => Some(ready),
                _ => None,
            },
        )
    }
    /// The only serialized step of a submission: the provider write of a
    /// request whose FULL submitted commit has already returned.
    async fn write(&mut self, ready: submission::Ready) -> Result<(), WorkerError> {
        let submission::Ready {
            id,
            context,
            method,
            prepared,
            control,
            vm,
        } = ready;
        if self.closing {
            // Boxed: keeps the rare closing path out of the hot `write` future.
            return Box::pin(self.settle_unwritten(id, context, method)).await;
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
        match Box::pin(crate::diagnostics::measure_runtime_stage_async(
            "turn_submission.admission",
            self.drive(admission),
        ))
        .await
        {
            Ok(_ticket) => {}
            Err(WorkerError::Session(SessionTaskError::Closed)) if vm => {}
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

    /// Settle a request whose `submitted` commit returned but which this
    /// worker never handed to its session: that proves it not sent. Without
    /// this outcome its receipt stays unresolved and holds the agent forever.
    async fn settle_unwritten(
        &mut self,
        id: RequestId,
        context: Arc<Pending>,
        method: Method,
    ) -> Result<(), WorkerError> {
        self.pending.insert(id.clone(), context);
        self.event(SessionEvent {
            journal: None,
            outcomes: vec![SessionOutcome::Write {
                operation_id: OperationId(String::new()),
                ticket: Some(studio_runtime_providers::RequestTicket {
                    id,
                    generation: self.generation.0,
                    method,
                }),
                outcome: WriteOutcome::NotSent,
                recovery: None,
            }],
        })
        .await
    }

    /// Write one claimed busy-turn input batch to the running turn. The
    /// durable `submitted` boundary commits before the write; delivery waits
    /// for the bridge acknowledgement (see `steer`).
    async fn steer(
        &mut self,
        intent: TurnIntent,
        turn: String,
        compiled: preparation::PreparedSteer,
    ) -> Result<(), WorkerError> {
        let operation = steer::operation(&self.account, &intent.attempt_id);
        let id = RequestId::String(operation.clone());
        let params = steer::params(&intent, &turn, &compiled.compiled);
        let thread_id = intent.thread_id.clone();
        if let Some(thread) = thread_id.as_deref() {
            self.idle_thread_touched(thread);
        }
        let context = Pending {
            thread: None,
            compiled: Some(compiled.compiled.clone()),
            intent,
            method: Method::TurnSteer.as_str().into(),
            operation: operation.clone(),
        };
        let control = self.control.clone();
        let prepared = self
            .drive(control.prepare(RequestSpec {
                id: id.clone(),
                operation_id: OperationId(operation),
                method: Method::TurnSteer,
                params,
                thread_id,
                lightweight: false,
                timeout_ms: self.timeout_ms,
            }))
            .await?;
        if self.closing {
            return Ok(());
        }
        let recovery = prepared.recovery_record();
        let save_context = context.clone();
        let authorized = Box::pin(self.db("steer_submitted", move |store| async move {
            scheduler::lifecycle_transaction_with(&store, move |tx| {
                steer::submitted(tx, &save_context, &recovery, &compiled)
            })
            .await
        }))
        .await?;
        if !authorized {
            self.wake(context.intent.agent_id.clone(), context.intent.epoch)
                .await?;
            return Ok(());
        }
        self.pending.insert(id.clone(), Arc::new(context));
        // A worker that began closing during the durable boundary never writes:
        // that proves the request not sent, so the input returns to the queue.
        let outcome = if self.closing {
            Some(WriteOutcome::NotSent)
        } else {
            match self.drive(control.submit(prepared)).await {
                Ok(_) => None,
                Err(error) if definite_rejection(&error) => Some(WriteOutcome::NotSent),
                Err(_) => Some(WriteOutcome::Unknown),
            }
        };
        if let Some(outcome) = outcome {
            self.event(SessionEvent {
                journal: None,
                outcomes: vec![SessionOutcome::Write {
                    operation_id: OperationId(String::new()),
                    ticket: Some(studio_runtime_providers::RequestTicket {
                        id,
                        generation: self.generation.0,
                        method: Method::TurnSteer,
                    }),
                    outcome,
                    recovery: None,
                }],
            })
            .await?;
        }
        Ok(())
    }

    /// Open a VM provider session for the durable actor at the turn boundary.
    /// The launch resolver is host-owned because it reads the account profile,
    /// guest provider configuration, and the minimal credential copy.
    async fn ensure_vm_session(
        &mut self,
        launch: Result<Option<preparation::VmProviderLaunchRequest>, WorkerError>,
    ) -> Result<Option<String>, WorkerError> {
        let Some(request) = launch? else {
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
                store
                    .read_async("runtime-services/account_worker_read", move |connection| {
                        if !crate::linux_vm_provider::vm_actor_matches(
                            connection,
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
                    store
                        .read_async("runtime-services/account_worker_read", move |connection| {
                            store::recover_vm_actor_requests(
                                connection,
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
                    initialize_params: studio_runtime_providers::initialize_params(
                        studio_runtime_providers::ClientName::AccountSession,
                    ),
                    generation,
                    wire: resolved
                        .provider
                        .parse::<studio_runtime_providers::ProviderKind>()
                        .map_err(|_| {
                            WorkerError::PreparationRefused(
                                "provider-session/unsupported-provider".into(),
                            )
                        })?
                        .wire(),
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
                self.pending
                    .insert(request.ticket.id.clone(), Arc::new(context));
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
            self.callback_scan = true;
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
                store
                    .read_async("runtime-services/account_worker_read", move |connection| {
                        let Some(record) =
                            AgentRepository::get(connection, &AgentId::from(lookup_id.as_str()))?
                        else {
                            return Ok(None);
                        };
                        let Ok(provider) = studio_runtime_providers::ProviderKind::of_field(
                            record.extra.get("provider"),
                        )
                        .map(|kind| kind.key()) else {
                            return Ok(None);
                        };
                        if !crate::linux_vm_provider::vm_actor_matches(
                            connection,
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
                            .unwrap_or(crate::linux_vm_provider::DEFAULT_GUEST_HOME);
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
        // The session admits a preparation only after it hands its held journal
        // frame to this worker, so keep reading events while it waits.
        let prepared = self
            .drive(control.prepare(RequestSpec {
                id: request_id,
                operation_id: OperationId(operation.clone()),
                method: Method::AccountLoginStart,
                params: payload,
                thread_id: None,
                lightweight: false,
                timeout_ms: self.timeout_ms,
            }))
            .await?;
        let (reply, response) = tokio::sync::oneshot::channel();
        self.request_waiters.insert(operation.clone(), reply);
        if let Err(error) = self.drive(control.submit(prepared)).await {
            self.request_waiters.remove(&operation);
            return Err(error);
        }
        let outcome = self
            .pump_until(response)
            .await?
            .map_err(|_| WorkerError::CallbackUnknown)?;
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
    /// Register the Chrome skill root with this Codex connection before a
    /// thread request (Python `codex_browser.ensure_skill`). The roots are
    /// process-wide in the app-server, so one confirmed reply covers every
    /// later thread of this connection. Setting the same roots again is
    /// idempotent: an error or an unknown outcome leaves them unconfirmed, and
    /// the next thread request sends them again. A failed registration does
    /// not hold the thread request; text work continues without the skill.
    async fn ensure_browser_skill(&mut self, roots: Vec<String>) -> Result<(), WorkerError> {
        if self.browser_skill_roots.as_ref() == Some(&roots) {
            return Ok(());
        }
        #[cfg(test)]
        if self
            .bridge
            .faults
            .hold_browser_skill
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.bridge.faults.browser_skill_held.notify_one();
            self.bridge.faults.release_browser_skill.notified().await;
        }
        self.browser_skill_roots = None;
        let operation = format!(
            "browser-skill:{}:{}",
            self.connection_id,
            uuid::Uuid::new_v4()
        );
        let timeout = crate::browser::BROWSER_SKILL_TIMEOUT;
        let control = self.control.clone();
        // The session admits a preparation only after it hands its held journal
        // frame to this worker, so keep reading events while it waits. Awaiting
        // it directly waited on this worker itself once 16 events were unread.
        let prepared = self
            .pump_until(tokio::time::timeout(
                timeout,
                control.prepare(RequestSpec {
                    id: RequestId::String(operation.clone()),
                    operation_id: OperationId(operation.clone()),
                    method: Method::SkillsExtraRootsSet,
                    params: json!({ "extraRoots": roots }),
                    thread_id: None,
                    lightweight: false,
                    timeout_ms: timeout.as_millis().try_into().unwrap_or(u64::MAX),
                }),
            ))
            .await;
        let outcome = match prepared {
            Err(error) => Err(error),
            // Not prepared in time: not confirmed, sent again with the next request.
            Ok(Err(_) | Ok(Err(_))) => Ok(None),
            Ok(Ok(Ok(prepared))) => {
                let (reply, response) = tokio::sync::oneshot::channel();
                self.request_waiters.insert(operation.clone(), reply);
                let control = self.control.clone();
                let outcome = match self.pump_until(control.submit(prepared)).await {
                    Ok(Ok(_ticket)) => self
                        .pump_until(tokio::time::timeout(timeout, response))
                        .await
                        .map(|reply| reply.ok().and_then(Result::ok)),
                    Ok(Err(_not_admitted)) => Ok(None),
                    Err(error) => Err(error),
                };
                self.request_waiters.remove(&operation);
                outcome
            }
        };
        match outcome {
            Ok(Some(RequestOutcome::Reply {
                body: ResponseBody::Result(_),
                ..
            })) => {
                self.browser_skill_roots = Some(roots);
                Ok(())
            }
            Ok(_) => {
                eprintln!(
                    "Studio browser skill registration for account {} is not confirmed; the next thread request sends it again",
                    self.account
                );
                Ok(())
            }
            Err(WorkerError::Closed) => {
                self.closing = true;
                Ok(())
            }
            Err(error) => Err(error),
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
            self.push(effect);
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
                store
                    .read_async("runtime-services/account_worker_read", move |connection| {
                        store::interrupt_intent(connection, &agent, &thread)
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
    fn push(&mut self, effect: Effect) {
        self.effects.push_back(effect);
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
                self.retry_deferred(true, false).await?;
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
        let outcome_count = event.outcomes.len();
        let mut merged = None;
        for (index, outcome) in event.outcomes.into_iter().enumerate() {
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
                if let Some(waiter) = self.request_waiters.remove(&operation) {
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
                    callback_record::written(&store, &operation, write_outcome).await
                })
                .await?;
                if !written {
                    return Err(WorkerError::CallbackUnknown);
                }
                self.managed_callback_cursors.remove(&(
                    self.generation.0,
                    callback_actor.clone(),
                    callback_epoch,
                    cursor.0,
                ));
                self.callback_scan = true;
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
            let resources = Arc::clone(&self.resources);
            let closing = self.closing;
            // Any turn/start outcome but its write or accepted reply may mean
            // the provider no longer has the thread loaded.
            let turn_unsettled = pending.as_ref().is_some_and(|context| {
                context.method == Method::TurnStart.as_str()
                    && !matches!(
                        outcome,
                        SessionOutcome::Write {
                            outcome: WriteOutcome::Written,
                            ..
                        } | SessionOutcome::Resolved {
                            outcome: RequestOutcome::Reply {
                                body: ResponseBody::Result(_),
                                ..
                            },
                            ..
                        }
                    )
            });
            if turn_unsettled && let Some(context) = &pending {
                self.thread_proofs.remove(&context.intent.agent_id);
            }
            // The last outcome of a reply settles in its journal transaction.
            if index + 1 == outcome_count
                && event.journal.is_some()
                && let Some(context) = pending
            {
                merged = Some(Box::new((
                    store::OutcomeStep {
                        account,
                        resources,
                        context,
                        outcome,
                        session_closed: closing,
                    },
                    id,
                )));
                continue;
            }
            let (settled, fault) = self
                .db_identified("outcome", identity, move |store| async move {
                    store::outcome(store, account, resources, pending, outcome, closing).await
                })
                .await?;
            if let Some(fault) = fault {
                fault.report(&self.account, &mut self.reported_faults);
            }
            self.settled_outcome(settled, id, actor_id);
        }
        if let Some(entry) = event.journal {
            self.journal_batch_with(vec![entry], source, settle, merged, false)
                .await?;
        }
        self.retry_deferred(settle, reclassify).await?;
        self.complete_retained().await?;
        Ok(())
    }
    /// Journal the deferred entries again. A settled outcome may bind them
    /// (`reclassify`); an unbound completion that no outstanding request can
    /// bind any more is settled at once, so it never holds the journal.
    async fn retry_deferred(&mut self, settle: bool, reclassify: bool) -> Result<(), WorkerError> {
        let mut reclassify = reclassify;
        // Each pass after the first settles at least one completion that no
        // request can bind; an entry journaled again comes back deferred with
        // its own agent, so the next pass can decide it.
        loop {
            if !reclassify
                && self
                    .deferred
                    .iter()
                    .all(|(entry, _, agent)| !self.unbindable(entry, agent.as_deref()))
            {
                return Ok(());
            }
            reclassify = false;
            for _ in 0..self.deferred.len() {
                let Some((entry, deferred_source, agent)) = self.deferred.pop_front() else {
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
                let park = self.unbindable(&entry, agent.as_deref());
                match self
                    .journal_batch_with(vec![entry], deferred_source.as_ref(), settle, None, park)
                    .await
                {
                    Err(WorkerError::StaleVmActor) => {
                        if let Some(source) = deferred_source.as_ref() {
                            self.retire_vm_actor_if_current(source);
                        }
                    }
                    result => {
                        result?;
                    }
                }
            }
        }
    }
    /// A deferred turn completion of a known agent that no outstanding
    /// request can bind any more. An entry without its agent (one deferred
    /// behind another) is journaled again first.
    fn unbindable(&self, entry: &JournalEntry, agent: Option<&str>) -> bool {
        agent.is_some() && !self.completion_may_bind(entry, agent)
    }
    /// Whether a request that may still bind this deferred turn completion
    /// is outstanding: a written request of this worker or a retained session
    /// request (a native review start) on its thread, or a preparation or
    /// commit of its agent.
    fn completion_may_bind(&self, entry: &JournalEntry, agent: Option<&str>) -> bool {
        let Some(thread) = agent_fault::completion_thread(entry) else {
            return true;
        };
        agent.is_some_and(|agent| self.preparing.contains_key(agent))
            || self
                .pending
                .values()
                .any(|pending| pending.intent.thread_id.as_deref() == Some(thread))
            || self
                .retained_requests
                .values()
                .any(|request| request.thread_id() == Some(thread))
    }
    /// Apply a committed outcome to the worker's in-memory request state.
    fn settled_outcome(
        &mut self,
        settled: store::SettledOutcome,
        id: Option<RequestId>,
        actor_id: Option<&str>,
    ) {
        let store::SettledOutcome {
            retired,
            effect,
            loaded,
        } = settled;
        // A VM actor's thread lives in its own session, not this connection.
        if actor_id.is_none()
            && let Some(loaded) = loaded
        {
            self.thread_proofs.insert(loaded.agent_id.clone(), loaded);
        }
        if retired && let Some(id) = id {
            self.pending.remove(&id);
        }
        if let Some(effect) = effect {
            self.push(effect);
        }
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
        self.deferred.retain(|(_, source, _)| {
            source
                .as_ref()
                .is_none_or(|source| source.actor_id != actor_id)
        });
        self.capacity_blocked.retain(|blocked| {
            blocked
                .source
                .as_ref()
                .is_none_or(|source| source.actor_id != actor_id)
        });
        if self.capacity_blocked.is_empty() {
            self.capacity_retry_at = None;
        }
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
    async fn journal_batch(
        &mut self,
        entries: Vec<JournalEntry>,
        source: Option<&VmActorIdentity>,
        settle: bool,
    ) -> Result<bool, WorkerError> {
        self.journal_batch_with(entries, source, settle, None, false)
            .await
    }
    /// Journal `entries` in one transaction that first settles `outcome`, the
    /// request outcome their reply carries, with its pending request id.
    /// `park_unbound` settles an unbound turn completion instead of deferring it.
    async fn journal_batch_with(
        &mut self,
        mut entries: Vec<JournalEntry>,
        source: Option<&VmActorIdentity>,
        settle: bool,
        outcome: Option<Box<(store::OutcomeStep, Option<RequestId>)>>,
        park_unbound: bool,
    ) -> Result<bool, WorkerError> {
        // Entries behind a refused one wait behind it; a reply's outcome
        // still settles now.
        if !self.capacity_blocked.is_empty() && !entries.is_empty() {
            self.capacity_blocked
                .extend(entries.drain(..).map(|entry| CapacityBlocked {
                    entry,
                    source: source.cloned(),
                    settle,
                    park_unbound,
                }));
            if outcome.is_none() {
                return Ok(false);
            }
        }
        let (outcome, outcome_id) = outcome.map(|merged| *merged).unzip();
        let outcome = outcome.map(Box::new);
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
        let delivered = std::mem::take(&mut self.delivered_publications);
        let claude_account_pin = self.resources.claude_account_pin.clone();
        let expected_vm_actor = actor_id
            .zip(actor_epoch)
            .map(|(actor, epoch)| (actor.to_owned(), epoch));
        let native_source = Some(crate::notification_dispatch::NativeProcessSource {
            account: self.account.clone(),
            connection: source
                .map(|source| format!("vm-guest:{}", source.actor_id))
                .unwrap_or_else(|| self.connection_id.clone()),
            generation: if source.is_some() {
                None
            } else {
                Some(self.generation.0)
            },
            provider: studio_runtime_providers::ProviderKind::of_record(&claude_account_pin)
                .map_err(|_| {
                    WorkerError::PreparationRefused("provider-session/unsupported-provider".into())
                })?
                .key()
                .to_owned(),
        });
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
                    native_source,
                    delivered,
                    outcome,
                    park_unbound,
                )
                .await;
                Ok((streams, result))
            })
            .await?;
        self.streams = previous;
        let store::JournalBatch {
            streams,
            committed,
            parked,
            deferred,
            deferred_agent,
            refused,
            settled,
            faults,
        } = committed?;
        for fault in &faults {
            fault.report(&self.account, &mut self.reported_faults);
        }
        if let Some(settled) = settled {
            self.settled_outcome(settled, outcome_id.flatten(), actor_id);
        }
        let had_deferred = !deferred.is_empty();
        let mut deferred_agent = deferred_agent;
        self.deferred.extend(
            deferred
                .into_iter()
                .map(|entry| (entry, source.cloned(), deferred_agent.take())),
        );
        let had_refused = !refused.is_empty();
        if had_refused {
            self.capacity_blocked
                .extend(refused.into_iter().map(|entry| CapacityBlocked {
                    entry,
                    source: source.cloned(),
                    settle,
                    park_unbound,
                }));
            self.capacity_retry_at = Some(tokio::time::Instant::now() + STREAM_CAPACITY_RETRY);
            // The consumer frees the room the refused output delta needs.
            self.bridge.monitor_dispatch_wake().notify_one();
        }
        #[cfg(test)]
        self.bridge
            .faults
            .journal_count
            .fetch_add(committed.len(), std::sync::atomic::Ordering::SeqCst);
        self.streams = streams; // commit precedes buffer replacement and ACK
        self.arm_flush();
        self.schedule_publication_drain()?;
        let committed_any = !committed.is_empty() || !parked.is_empty();
        let mut cursors = Vec::with_capacity(committed.len());
        let mut settle_cursors = Vec::with_capacity(committed.len());
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
            let cursor = entry.cursor;
            cursors.push(cursor);
            self.journal_committed(entry, effect, source).await?;
            settle_cursors.push(cursor);
        }
        // A parked entry has no in-memory effects; it settles with the batch.
        if !parked.is_empty() {
            cursors.extend(parked.iter().map(|entry| entry.cursor));
            settle_cursors.extend(parked.iter().map(|entry| entry.cursor));
            settle_cursors.sort_unstable();
        }
        if settle && !self.closing && !settle_cursors.is_empty() {
            self.settle_journal(settle_cursors, source).await?;
        }
        if let Some(actor_id) = actor_id {
            for cursor in cursors {
                if let Some(reply) = self
                    .vm_notification_waiters
                    .remove(&(actor_id.to_owned(), cursor.0))
                {
                    let _sent = reply.send(Ok(()));
                }
            }
        }
        Ok(committed_any && !had_deferred && !had_refused)
    }
    /// Journal again, in order, the entries a full monitor output queue
    /// refused. An entry refused again keeps its place ahead of the rest and
    /// re-arms the retry; provider events stay unread until the queue clears.
    async fn journal_capacity_blocked(&mut self) -> Result<(), WorkerError> {
        let mut queue = std::mem::take(&mut self.capacity_blocked);
        while let Some(CapacityBlocked {
            entry,
            source,
            settle,
            park_unbound,
        }) = queue.pop_front()
        {
            let mut entries = vec![entry];
            while entries.len() < JOURNAL_BATCH_MAX_EVENTS
                && queue.front().is_some_and(|next| {
                    next.source == source
                        && next.settle == settle
                        && next.park_unbound == park_unbound
                })
            {
                if let Some(next) = queue.pop_front() {
                    entries.push(next.entry);
                }
            }
            // A retired VM actor's entries are dropped, as `retry_deferred` does.
            if let Some(source) = source.as_ref()
                && !self
                    .vm_sessions
                    .get(&source.actor_id)
                    .is_some_and(|session| {
                        session.epoch == source.epoch && session.incarnation == source.incarnation
                    })
            {
                continue;
            }
            match self
                .journal_batch_with(entries, source.as_ref(), settle, None, park_unbound)
                .await
            {
                Err(WorkerError::StaleVmActor) => {
                    if let Some(source) = source.as_ref() {
                        self.retire_vm_actor_if_current(source);
                    }
                }
                result => {
                    result?;
                }
            }
            if !self.capacity_blocked.is_empty() {
                self.capacity_blocked.extend(queue);
                return Ok(());
            }
        }
        self.retry_deferred(true, false).await?;
        self.complete_retained().await
    }
    /// Report a drained batch's committed cursors to their session in one
    /// round trip, in journal order, after the batch's commit.
    async fn settle_journal(
        &mut self,
        cursors: Vec<studio_runtime_process::EventCursor>,
        source: Option<&VmActorIdentity>,
    ) -> Result<(), WorkerError> {
        let actor_id = source.map(|source| source.actor_id.as_str());
        let actor_epoch = source.map(|source| source.epoch);
        let actor_incarnation = source.map(|source| source.incarnation);
        if let Some(actor) = actor_id
            && !self.vm_sessions.get(actor).is_some_and(|session| {
                Some(session.epoch) == actor_epoch && Some(session.incarnation) == actor_incarnation
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
        if let Err(error) = control.settled_all(cursors).await {
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
        Ok(())
    }
    /// Apply the provider's sign-in completion to the account registry before
    /// its cursor settles. A failure leaves the receipt open; the registry
    /// reconcile settles it from the credentials or at its deadline.
    async fn login_completed(&self, params: Option<&Value>) {
        let Some(driver) = self.resources.account_driver.clone() else {
            return;
        };
        let account = self.account.clone();
        let params = params.cloned().unwrap_or(Value::Null);
        let result = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/account_worker",
            move || driver.codex_login_completed(&account, &params),
        )
        .await;
        match result {
            Ok(Ok(changed)) if !changed.is_empty() => self.bridge.event_wake().notify_one(),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => crate::diagnostics::log_stderr(format_args!(
                "account {} sign-in completion was not applied: {error}",
                self.account
            )),
            Err(_) => crate::diagnostics::log_stderr(format_args!(
                "account {} sign-in completion task failed",
                self.account
            )),
        }
    }

    /// Apply one committed entry's in-memory effects. Every cursor settles
    /// with its batch; a provider request owes its answer through its record.
    async fn journal_committed(
        &mut self,
        entry: JournalEntry,
        effect: Option<Effect>,
        source: Option<&VmActorIdentity>,
    ) -> Result<(), WorkerError> {
        if source.is_none()
            && let JournalPayload::Frame(Frame::Notification(notification)) = &entry.payload
        {
            self.idle_provider_notification(
                notification.method.as_str(),
                notification.params.as_ref(),
            )
            .await?;
            if notification.method.as_str() == "account/login/completed" {
                self.login_completed(notification.params.as_ref()).await;
            }
        }
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
            self.push(effect);
        }
        if let JournalPayload::Frame(frame) = &entry.payload
            && let Frame::Notification(notification) = frame
            && notification.method.as_str() == "command/exec/outputDelta"
        {
            self.bridge.monitor_dispatch_wake().notify_one();
        }
        // A provider request is already durably accepted with its batch. Its
        // handler starts from the record, so limits and restarts share one path.
        if let JournalPayload::Frame(frame) = &entry.payload
            && callback::response(frame).is_some()
        {
            self.callback_scan = true;
        }
        Ok(())
    }
    /// Start the handlers of accepted callback records, within the limits of
    /// running handlers and queued answers. A record beyond the limits stays
    /// accepted; this runs again when a handler or an answer finishes.
    async fn start_callbacks(&mut self) -> Result<(), WorkerError> {
        self.callback_scan = false;
        let queued_answers = self
            .effects
            .iter()
            .filter(|effect| matches!(effect, Effect::Callback(..)))
            .count();
        let mut free = MAX_RUNNING_CALLBACKS
            .saturating_sub(self.managed_callbacks.len())
            .min(MAX_QUEUED_CALLBACK_ANSWERS.saturating_sub(queued_answers));
        if free == 0 {
            return Ok(());
        }
        let account = self.account.clone();
        // Records this worker already holds are skipped below.
        let limit = free + self.managed_callback_cursors.len() + self.callback_parked.len();
        let generation = self.generation.0;
        let accepted = self
            .db("callbacks_unsent", move |store| async move {
                callback_record::unsent(&store, &account, generation, limit).await
            })
            .await?;
        self.callback_parked.clear();
        for record in accepted {
            if free == 0 {
                self.callback_scan = true;
                break;
            }
            let actor = record.actor.clone();
            let epoch = record.epoch.or_else(|| {
                actor.as_ref().and_then(|actor| {
                    self.vm_sessions
                        .get(actor)
                        .map(|session| session.launch_request.epoch)
                })
            });
            let key = (record.generation, actor.clone(), epoch, record.cursor);
            if self.managed_callback_cursors.contains(&key) {
                continue;
            }
            let origin = Generation(record.generation);
            let cursor = studio_runtime_process::EventCursor(record.cursor);
            #[cfg(test)]
            let exited = origin != self.generation
                && !self
                    .bridge
                    .faults
                    .journal_exited_requests_as_current
                    .load(std::sync::atomic::Ordering::SeqCst);
            #[cfg(not(test))]
            let exited = origin != self.generation;
            // The provider process that sent this request has exited: no
            // answer can reach its sender.
            if exited {
                self.abandon_callback(cursor, origin, actor.as_deref(), epoch)
                    .await?;
                continue;
            }
            if let Some(actor_id) = actor.as_deref() {
                match self.vm_sessions.get(actor_id).map(|session| session.epoch) {
                    Some(session_epoch) if Some(session_epoch) == epoch => {}
                    Some(_) => {
                        // A newer epoch of the actor runs; this one is gone.
                        self.abandon_callback(cursor, origin, actor.as_deref(), epoch)
                            .await?;
                        continue;
                    }
                    None => {
                        // The guest provider outlives a server restart and the
                        // actor reattaches later. Wait for it unless its row
                        // proves this epoch is gone.
                        if self.vm_actor_epoch_gone(actor_id, epoch).await? {
                            self.abandon_callback(cursor, origin, actor.as_deref(), epoch)
                                .await?;
                        } else {
                            self.callback_parked.insert(key);
                        }
                        continue;
                    }
                }
            }
            let frame = record.frame()?;
            self.managed_callback_cursors.insert(key);
            self.start_callback(cursor, origin, frame, actor, epoch)?;
            free -= 1;
        }
        Ok(())
    }
    /// Whether the stored VM actor row proves `epoch` of `actor` is gone.
    async fn vm_actor_epoch_gone(
        &mut self,
        actor: &str,
        epoch: Option<i64>,
    ) -> Result<bool, WorkerError> {
        let (Some(epoch), account) = (epoch, self.account.clone()) else {
            return Ok(false);
        };
        let actor = actor.to_owned();
        let matches = self
            .db("callback_actor_epoch", move |store| async move {
                store
                    .read_async("runtime-services/account_worker_read", move |connection| {
                        crate::linux_vm_provider::vm_actor_matches(
                            connection, &actor, epoch, &account, None,
                        )
                        .map_err(WorkerError::from)
                    })
                    .await
            })
            .await?;
        Ok(!matches)
    }
    /// Run the handler of one accepted request, or queue its fixed answer.
    fn start_callback(
        &mut self,
        cursor: studio_runtime_process::EventCursor,
        origin: Generation,
        frame: Frame,
        actor: Option<String>,
        actor_epoch: Option<i64>,
    ) -> Result<(), WorkerError> {
        if let Frame::Request(request) = &frame
            && request.method.as_str() == "item/tool/call"
        {
            let account = self.account.clone();
            let generation = self.generation.0;
            // Tool records and the time-awareness text carry wall time; `received_at_ms`
            // is session-relative and only orders and bounds journal entries.
            let now = crate::event_write::unix_time().map_err(|_| WorkerError::TaskFailed)?;
            let wake = self.bridge.event_wake();
            let monitor_dispatch_wake = self.bridge.monitor_dispatch_wake();
            let store = Arc::clone(&self.bridge.store);
            let managed_tool_runtime = self.bridge.managed_tool_runtime();
            #[cfg(test)]
            let held = Arc::clone(&self.bridge.faults.callback_gate);
            self.managed_callbacks.spawn(async move {
                #[cfg(test)]
                let _held = held.read().await;
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
                        return (cursor, origin, actor, actor_epoch, None);
                    };
                    return (
                        cursor,
                        origin,
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
                (cursor, origin, actor, actor_epoch, response)
            });
        } else if callback::is_vm_codex_refresh_request(&frame) {
            let account = self.account.clone();
            let generation = self.generation.0;
            let resolver = self.resources.vm_provider_launch.clone();
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
                (cursor, origin, actor, actor_epoch, response)
            });
        } else if let Some(response) = callback::response(&frame) {
            self.push(Effect::Callback(
                cursor,
                origin,
                response,
                actor,
                actor_epoch,
            ));
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
    /// Retire the record of a provider request that can no longer be answered:
    /// its generation exited, or the VM actor that relayed it is gone. A reply
    /// written now would reach the generation's successor under a request id
    /// that process never issued.
    async fn abandon_callback(
        &mut self,
        cursor: studio_runtime_process::EventCursor,
        origin: Generation,
        actor_id: Option<&str>,
        actor_epoch: Option<i64>,
    ) -> Result<(), WorkerError> {
        let operation =
            callback_record::operation(&self.journal_scope(actor_id), origin.0, cursor.0);
        self.db("callback_abandoned", move |store| async move {
            callback_record::retire_async(&store, &operation).await
        })
        .await?;
        self.managed_callback_cursors.remove(&(
            origin.0,
            actor_id.map(str::to_owned),
            actor_epoch,
            cursor.0,
        ));
        eprintln!(
            "provider request settled without an answer; account={} generation={} cursor={} current-generation={} outcome=provider-generation-ended",
            self.account, origin.0, cursor.0, self.generation.0
        );
        Ok(())
    }
    async fn answer_callback(
        &mut self,
        cursor: studio_runtime_process::EventCursor,
        origin: Generation,
        frame: Frame,
        actor_id: Option<String>,
        actor_epoch: Option<i64>,
    ) -> Result<(), WorkerError> {
        if origin != self.generation {
            // The reply is named and fenced by the current generation below;
            // it must never answer another generation's request.
            return self
                .abandon_callback(cursor, origin, actor_id.as_deref(), actor_epoch)
                .await;
        }
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
                    store
                        .read_async("runtime-services/account_worker_read", move |connection| {
                            crate::linux_vm_provider::vm_actor_matches(
                                connection,
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
        let operation = callback_record::operation(
            &self.journal_scope(actor_id.as_deref()),
            self.generation.0,
            cursor.0,
        );
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
        let answered = self
            .db("callback_admission", move |store| async move {
                callback_record::admit(&store, &key).await
            })
            .await?;
        if answered {
            self.managed_callback_cursors.remove(&(
                origin.0,
                actor_id.clone(),
                actor_epoch,
                cursor.0,
            ));
            return Ok(());
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
                    store
                        .read_async("runtime-services/account_worker_read", move |connection| {
                            if !crate::linux_vm_provider::vm_actor_matches(
                                connection,
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
                    callback_record::written(&store, &operation, WriteOutcome::NotSent).await
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
        let delivered = std::mem::take(&mut self.delivered_publications);
        let flush = move |store: Arc<RuntimeStore>| async move {
            store::flush_streams(store, &account, delivered).await
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
        self.schedule_publication_drain()?;
        Ok(())
    }
    /// Start a drain pass now, or right after the running one. Commits drive
    /// the drain here, joining a finished pass, because under load the serve
    /// loop's biased select keeps finding a finished preparation first and
    /// never reaches the drain branches.
    fn schedule_publication_drain(&mut self) -> Result<(), WorkerError> {
        let now = tokio::time::Instant::now();
        if let Some(done) = self
            .publication_drain
            .as_mut()
            .filter(|task| task.is_finished())
            .and_then(futures_util::FutureExt::now_or_never)
        {
            self.publication_retry_at.get_or_insert(now);
            return self.publication_drained(done);
        }
        if self.publication_drain.is_some() {
            self.publication_retry_at.get_or_insert(now);
        } else if self.publication_retry_at.is_none_or(|at| at <= now) {
            self.start_publication_drain();
        }
        Ok(())
    }
    /// Delivery runs off the worker's serial path as one single-flight task, so a
    /// slow sink or analytics transaction never delays a submission.
    fn start_publication_drain(&mut self) {
        self.publication_retry_at = None;
        let bridge = self.bridge.clone();
        let account = self.account.clone();
        let sink = Arc::clone(&self.publication_sink);
        let skip = self.delivered_publications.clone();
        self.publication_drain = Some(tokio::spawn(async move {
            // Not a fault-injected worker operation: it runs beside the worker, so
            // the crash matrix's operation order stays that of the serial path.
            bridge
                .run(move |store| async move {
                    store::deliver_publications(store, account, sink, skip, || Ok(())).await
                })
                .await
        }));
    }
    fn publication_drained(
        &mut self,
        done: Result<Result<store::PublicationPass, WorkerError>, tokio::task::JoinError>,
    ) -> Result<(), WorkerError> {
        self.publication_drain = None;
        let pass = done.map_err(|_| WorkerError::TaskFailed)??;
        self.delivered_publications.extend(pass.delivered);
        match pass.failure {
            None => {
                // A full page or a schedule raised during the pass runs another.
                if pass.full || self.publication_retry_at.is_some() {
                    self.schedule_publication_drain()?;
                }
                Ok(())
            }
            Some(WorkerError::Notification(_)) => {
                // A failed sink leaves its receipt in place. Retry on a fixed
                // cadence so a broken consumer cannot spin the account worker.
                self.publication_retry_at =
                    Some(tokio::time::Instant::now() + PUBLICATION_RETRY_DELAY);
                Ok(())
            }
            Some(error) => Err(error),
        }
    }
    async fn drain_managed_callbacks(&mut self) -> Result<(), WorkerError> {
        #[cfg(test)]
        self.bridge.faults.shutdown_callbacks_draining.notify_one();
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        let completed = tokio::time::timeout_at(deadline, async {
            while let Some(callback) = self.managed_callbacks.join_next().await {
                match callback {
                    Ok((cursor, generation, actor, epoch, Some(response))) => {
                        self.push(Effect::Callback(cursor, generation, response, actor, epoch));
                    }
                    Ok((_, _, _, _, None)) => {}
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
                Effect::Callback(cursor, generation, response, actor, epoch) => {
                    match self
                        .answer_callback(cursor, generation, response, actor, epoch)
                        .await
                    {
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
        let (cleanup_error, recovery, provider_result) = match exit.adapter {
            Some(adapter) => (
                adapter.cleanup_error,
                adapter.recovery_needed,
                Some(adapter.result),
            ),
            None => (None, None, None),
        };
        let identity = format!("account={account};generation={generation}");
        self.db_identified("finish", identity, move |store| async move {
            if let Some(recovery) = recovery {
                store::save_recovery(&store, &account, &recovery).await?;
            }
            scheduler::lifecycle_transaction(&store, move |tx| {
                store::disconnect_provider_generation(tx, &account, generation)
            })
            .await?;
            Ok(())
        })
        .await?;
        if let Some(error) = cleanup_error {
            return Err(WorkerError::CleanupUnconfirmed(error));
        }
        // An unrequested close reports the provider task's own cause when it has one.
        match (exit.result, provider_result) {
            (Err(SessionTaskError::Closed), Some(Err(cause))) if !*self.stop.borrow() => {
                Err(SessionTaskError::Adapter(cause).into())
            }
            (result, _) => result.map_err(Into::into),
        }
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
