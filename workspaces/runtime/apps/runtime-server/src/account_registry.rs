//! Runtime ownership of the existing account workers; all input and recovery
//! decisions remain in the worker. Wakes are hints over committed SQLite rows.
//! Registered runtime difference: `image-workspace-dependency-hold` persists
//! an image-workspace refusal on the agent and leaves its input event pending.
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use studio_runtime_process::provider_seam::{OpenOptions, ProviderTaskFactory, Shutdown};
use studio_runtime_services::{
    account_driver::AccountChangeSignal,
    account_worker::{
        AccountCommand, AccountHandle, StoreBridge, WorkerError, preparation::PreparationResources,
        start_account_worker_with_publication_sink,
    },
    host_project_runtime::PreparedHost,
    projects::ProjectsError,
    provider_session_task::SessionTaskConfig,
    radio_runtime::{
        self, Interrupt, InterruptOutcome, InterruptReceipt, InterruptTransport, RadioIngress,
    },
};
use studio_runtime_store::{
    AgentId, AgentRepository, AgentStatus, QueueEvent, QueueEventId, QueueEventRepository,
    RuntimeStore, SchedulerRepository,
};
use tokio::sync::{Notify, mpsc, oneshot};

/// Changed-row hints for the registry: which agents' accounts a pass must look at.
/// A hint only narrows the full pass; overflow or an unknown commit asks for it.
mod registry_hints {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    use studio_runtime_store::{ObservedTable, RuntimeStore, TableChange};
    use tokio::sync::Notify;

    /// Rowids kept per table between two registry passes before a full pass is requested.
    /// Unit: rowids.
    const MAX_PENDING_ROW_HINTS: usize = 4096;

    /// Durable changes since the last registry pass.
    #[derive(Debug, Default, Eq, PartialEq)]
    pub(crate) struct RegistryChanges {
        /// Row identity is unknown; every account needs a pass.
        pub(crate) full: bool,
        pub(crate) agent_rows: BTreeSet<i64>,
        pub(crate) event_rows: BTreeSet<i64>,
        pub(crate) event_meta_rows: BTreeSet<i64>,
        /// An accepted-work row changed, so the archive tick may have work.
        pub(crate) work: bool,
    }

    #[derive(Default)]
    pub(crate) struct RegistryHints {
        pending: Mutex<RegistryChanges>,
        wake: Notify,
    }

    impl RegistryHints {
        /// Register before the first full pass: commits after registration are either covered by
        /// that pass or kept here.
        pub(crate) fn observe(self: &Arc<Self>, store: &RuntimeStore) {
            for table in [
                ObservedTable::Agents,
                ObservedTable::Events,
                ObservedTable::EventMeta,
                ObservedTable::Work,
                ObservedTable::Rooms,
            ] {
                let hints = Arc::clone(self);
                store.add_table_row_change_observer(table, move |change| {
                    hints.record(table, change)
                });
            }
        }

        pub(crate) fn record(&self, table: ObservedTable, change: TableChange<'_>) {
            {
                let mut pending = self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let pending = &mut *pending;
                match change {
                    TableChange::Full => pending.full = true,
                    TableChange::Rows(ids) => {
                        let rows = match table {
                            ObservedTable::Agents => &mut pending.agent_rows,
                            ObservedTable::Events => &mut pending.event_rows,
                            ObservedTable::EventMeta => &mut pending.event_meta_rows,
                            ObservedTable::Work => {
                                pending.work = true;
                                return self.wake.notify_one();
                            }
                            _ => return self.wake.notify_one(),
                        };
                        if rows.len() + ids.len() > MAX_PENDING_ROW_HINTS {
                            rows.clear();
                            pending.full = true;
                        } else {
                            rows.extend(ids.iter().copied());
                        }
                    }
                }
            }
            self.wake.notify_one();
        }

        pub(crate) async fn notified(&self) {
            self.wake.notified().await;
        }

        /// Takes the pending changes; commits during the pass that follows arrive as new hints.
        pub(crate) fn take(&self) -> RegistryChanges {
            std::mem::take(
                &mut *self
                    .pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            )
        }
    }
}
use registry_hints::{RegistryChanges, RegistryHints};

const MAX_ACCOUNTS: usize = 128;
const WAKE_PAGE: usize = 32;
const RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const ACCOUNT_WORKER_SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
const IMAGE_EXECUTION_DEPENDENCY: &str = "workspace-state/image-execution";

fn next_retry_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_RETRY_DELAY)
}

fn allow_dormant_reconcile(
    idle_dormant: &mut BTreeSet<String>,
    account: &str,
    has_pending_work: bool,
) -> bool {
    if !idle_dormant.contains(account) {
        return true;
    }
    has_pending_work && idle_dormant.remove(account)
}

fn defer_wake(
    last_error: Option<&str>,
    now: tokio::time::Instant,
    retry_at: Option<tokio::time::Instant>,
) -> bool {
    last_error.is_some() && retry_at.is_some_and(|retry_at| now < retry_at)
}

fn next_registry_retry_at(
    archive_retry_at: Option<tokio::time::Instant>,
    checkpoint_retry_at: Option<tokio::time::Instant>,
    worker_retry_at: &BTreeMap<String, tokio::time::Instant>,
) -> Option<tokio::time::Instant> {
    archive_retry_at
        .into_iter()
        .chain(checkpoint_retry_at)
        .chain(worker_retry_at.values().copied())
        .min()
}

fn archive_deadline_read_deferred(
    retry_at: Option<tokio::time::Instant>,
    now: tokio::time::Instant,
) -> bool {
    retry_at.is_some_and(|retry_at| retry_at > now)
}

/// Paired clocks for converting the stored Unix-seconds deadline without
/// restarting its remaining hold after asynchronous database work.
struct ChildResultReleaseClock {
    monotonic: tokio::time::Instant,
    wall_seconds: f64,
}

impl ChildResultReleaseClock {
    fn capture() -> Self {
        Self {
            monotonic: tokio::time::Instant::now(),
            wall_seconds: event_time(),
        }
    }

    fn release_at(&self, deadline_seconds: f64) -> tokio::time::Instant {
        self.monotonic + Duration::from_secs_f64((deadline_seconds - self.wall_seconds).max(0.0))
    }
}

/// Wake when the earliest held child result stops waiting for its siblings
/// (the stored child-result hold); a failed read retries after `RETRY_DELAY`.
async fn child_result_release_at(
    store: Arc<studio_runtime_store::RuntimeStore>,
) -> Option<tokio::time::Instant> {
    let clock = ChildResultReleaseClock::capture();
    let now = clock.wall_seconds;
    let read = store
        .read_async("runtime-server/account_registry", move |db| {
            SchedulerRepository::child_result_release_at(db, now)
        })
        .await;
    match read {
        Ok(deadline) => deadline.map(|deadline| clock.release_at(deadline)),
        Err(error) => {
            studio_runtime_services::diagnostics::log_stderr(format_args!(
                "account registry child result release read deferred: {error}"
            ));
            Some(tokio::time::Instant::now() + RETRY_DELAY)
        }
    }
}

/// The registry's next timed wake: the earliest archive, checkpoint, or
/// worker retry, or the release of the earliest held child result.
async fn next_registry_wake_at(
    store: Arc<studio_runtime_store::RuntimeStore>,
    archive_retry_at: Option<tokio::time::Instant>,
    checkpoint_retry_at: Option<tokio::time::Instant>,
    worker_retry_at: &BTreeMap<String, tokio::time::Instant>,
) -> Option<tokio::time::Instant> {
    let release_at = child_result_release_at(store).await;
    next_registry_retry_at(
        archive_retry_at.into_iter().chain(release_at).min(),
        checkpoint_retry_at,
        worker_retry_at,
    )
}

async fn checkpoint_maintenance(
    store: Arc<studio_runtime_store::RuntimeStore>,
) -> (bool, Option<tokio::time::Instant>) {
    let result = store
        .checkpoint_wal_if_due()
        .await
        .map_err(|error| error.to_string())
        .and_then(|_| {
            store
                .wal_checkpoint_retry_after()
                .map_err(|error| error.to_string())
        });
    match result {
        Ok(Some(delay)) => {
            let delay = if delay.is_zero() { RETRY_DELAY } else { delay };
            (true, Some(tokio::time::Instant::now() + delay))
        }
        Ok(None) => (true, None),
        Err(_) => (false, Some(tokio::time::Instant::now() + RETRY_DELAY)),
    }
}

enum RegistryTrigger {
    Wake,
    /// Committed rows changed; the pass may narrow to the accounts they touch.
    Hinted,
    Retry,
    Ensure(EnsureRequest),
    /// A commit requested a WAL flush while none was pending.
    FlushRequested,
    WorkerExited(String, u64),
    CommandCapacityAvailable(String, bool),
}

#[derive(Debug)]
enum AccountPageDispatch {
    Exhausted,
    Continue,
    Full,
    StartFailed(WorkerError),
}

struct EnsureRequest {
    account: String,
    response: oneshot::Sender<
        Result<
            studio_runtime_services::account_session_runtime::ConnectionScope,
            studio_runtime_services::account_session_runtime::AccountSessionError,
        >,
    >,
}

pub type LaunchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AccountLaunch, WorkerError>> + Send + 'a>>;
type WorkerCompletion = Pin<Box<dyn Future<Output = (String, u64)> + Send>>;
type CommandCapacityWaiter = Pin<Box<dyn Future<Output = (String, bool)> + Send>>;
pub type AccountTargetPage = BTreeMap<String, Vec<(String, i64, Option<String>)>>;

pub struct AccountLaunch {
    pub factory: Arc<dyn ProviderTaskFactory>,
    pub options: OpenOptions,
    pub config: SessionTaskConfig,
    pub resources: PreparationResources,
    pub publication_sink:
        Arc<dyn studio_runtime_services::notification_runtime_effects::NotificationPostCommitSink>,
}

/// Launch resolution runs outside transactions. A successful return owns the
/// attachment until the worker consumes it; loss of a receipt never retries input.
pub trait AccountLauncher: Send + Sync {
    fn launch(&self, account: String) -> LaunchFuture<'_>;
}

pub struct AccountRegistry {
    store: Arc<RuntimeStore>,
    bridge: StoreBridge,
    launcher: Arc<dyn AccountLauncher>,
    radio_clock: Arc<dyn Fn() -> f64 + Send + Sync>,
    workers: BTreeMap<String, AccountHandle>,
    worker_completion_ids: BTreeMap<String, u64>,
    worker_retry_at: BTreeMap<String, tokio::time::Instant>,
    worker_retry_delay: BTreeMap<String, Duration>,
    idle_dormant: BTreeSet<String>,
    accepted_archive_batch_full: bool,
    next_worker_completion_id: u64,
    worker_completions: FuturesUnordered<WorkerCompletion>,
    command_capacity_waiters: FuturesUnordered<CommandCapacityWaiter>,
    command_capacity_waiting: BTreeSet<String>,
    session_broker: AccountSessionBroker,
    accepted_archive_tick_enabled: bool,
    ensure_requests: mpsc::Receiver<EnsureRequest>,
    after_account: String,
    after_agent: BTreeMap<String, String>,
    hints: Arc<RegistryHints>,
}

#[derive(Clone, Copy, Debug)]
pub struct AccountRegistryConfig {
    pub accepted_archive_tick_enabled: bool,
}

impl Default for AccountRegistryConfig {
    fn default() -> Self {
        Self {
            accepted_archive_tick_enabled: true,
        }
    }
}

#[derive(Clone)]
pub struct AccountSessionBroker {
    sessions: Arc<
        RwLock<
            BTreeMap<
                String,
                studio_runtime_services::account_worker::retained::RetainedSessionPort,
            >,
        >,
    >,
    ensure_sender: mpsc::Sender<EnsureRequest>,
    ensure_receiver: Arc<Mutex<Option<mpsc::Receiver<EnsureRequest>>>>,
    changes: AccountChangeSignal,
}

impl Default for AccountSessionBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl AccountSessionBroker {
    #[doc(hidden)]
    pub fn new() -> Self {
        Self::new_with_change_signal(AccountChangeSignal::new())
    }

    pub fn new_with_change_signal(changes: AccountChangeSignal) -> Self {
        let (ensure_sender, ensure_receiver) = mpsc::channel(32);
        Self {
            sessions: Arc::default(),
            ensure_sender,
            ensure_receiver: Arc::new(Mutex::new(Some(ensure_receiver))),
            changes,
        }
    }

    pub fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.subscribe()
    }

    fn publish_change(&self) {
        self.changes.publish_session_change();
    }

    fn insert_session(
        &self,
        account: String,
        session: studio_runtime_services::account_worker::retained::RetainedSessionPort,
    ) -> Result<(), WorkerError> {
        self.sessions
            .write()
            .map_err(|_| WorkerError::TaskFailed)?
            .insert(account, session);
        self.publish_change();
        Ok(())
    }

    fn remove_session(&self, account: &str) -> Result<(), WorkerError> {
        self.sessions
            .write()
            .map_err(|_| WorkerError::TaskFailed)?
            .remove(account);
        self.publish_change();
        Ok(())
    }

    fn take_ensure_receiver(&self) -> Result<mpsc::Receiver<EnsureRequest>, WorkerError> {
        self.ensure_receiver
            .lock()
            .map_err(|_| WorkerError::TaskFailed)?
            .take()
            .ok_or(WorkerError::TaskFailed)
    }

    fn live_scope(
        &self,
        account: &str,
    ) -> Option<studio_runtime_services::account_session_runtime::ConnectionScope> {
        self.sessions
            .read()
            .ok()?
            .get(account)
            .filter(|session| session.is_live())
            .map(|session| session.scope().clone())
    }

    pub fn callback_reply_port(
        &self,
    ) -> Arc<dyn studio_runtime_services::callback_runtime::CallbackReplyPort> {
        Arc::new(BrokerCallbackReplyPort {
            broker: self.clone(),
        })
    }
}

struct BrokerCallbackReplyPort {
    broker: AccountSessionBroker,
}

impl BrokerCallbackReplyPort {
    fn session(
        &self,
        account: &str,
    ) -> Option<studio_runtime_services::account_worker::retained::RetainedSessionPort> {
        self.broker
            .sessions
            .read()
            .ok()
            .and_then(|sessions| sessions.get(account).cloned())
    }
}

impl studio_runtime_services::callback_runtime::CallbackReplyPort for BrokerCallbackReplyPort {
    /// The session must be the provider process that sent the callback; see
    /// `RetainedSessionPort::serves_callback`.
    fn has_live_session(
        &self,
        context: &studio_runtime_services::provider_callbacks::CallbackContext,
    ) -> bool {
        self.session(&context.account)
            .is_some_and(|session| session.serves_callback(context))
    }

    fn deliver<'a>(
        &'a self,
        reply: &'a studio_runtime_services::provider_callbacks::Reply,
        deadline: Duration,
    ) -> studio_runtime_services::callback_runtime::CallbackReplyFuture<'a> {
        Box::pin(async move {
            let identity = reply.identity.clone();
            let Some(session) = self.session(&identity.context.account) else {
                return Ok(studio_runtime_services::callback_runtime::ReplyReceipt {
                    identity,
                    outcome: studio_runtime_services::callback_runtime::ReplyOutcome::NotSent,
                });
            };
            session.deliver_callback_reply(reply, deadline).await
        })
    }
}

impl studio_runtime_services::account_session_runtime::RetainedAccountSession
    for AccountSessionBroker
{
    fn ensure_session(
        &self,
        account: String,
    ) -> studio_runtime_services::account_session_runtime::SessionFutureScope {
        use studio_runtime_services::account_session_runtime::AccountSessionError;
        if let Some(scope) = self.live_scope(&account) {
            return Box::pin(async move { Ok(scope) });
        }
        let sender = self.ensure_sender.clone();
        Box::pin(async move {
            let (response, receiver) = oneshot::channel();
            sender
                .send(EnsureRequest { account, response })
                .await
                .map_err(|_| AccountSessionError::Requires("provider-session/account-session"))?;
            receiver
                .await
                .map_err(|_| AccountSessionError::Requires("provider-session/account-session"))?
        })
    }

    fn current_scope(
        &self,
        account: &str,
    ) -> Option<studio_runtime_services::account_session_runtime::ConnectionScope> {
        self.live_scope(account)
    }

    fn deliver_vm_notification(
        &self,
        account: String,
        agent_id: String,
        epoch: u64,
        sequence: u64,
        frame: studio_runtime_providers::Frame,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        (),
                        studio_runtime_services::account_session_runtime::AccountSessionError,
                    >,
                > + Send,
        >,
    > {
        use studio_runtime_services::account_session_runtime::AccountSessionError;

        let session = self
            .sessions
            .read()
            .ok()
            .and_then(|sessions| sessions.get(&account).cloned())
            .filter(|session| session.is_live());
        Box::pin(async move {
            let Some(session) = session else {
                return Err(AccountSessionError::Requires(
                    "account-worker/vm-notification",
                ));
            };
            let epoch = i64::try_from(epoch).map_err(|_| AccountSessionError::Unknown)?;
            session
                .deliver_vm_notification(&account, &agent_id, epoch, sequence, frame)
                .await
                .map_err(|_| AccountSessionError::Unknown)
        })
    }

    fn claude_control_session(
        &self,
        account: &str,
    ) -> Result<
        Arc<dyn studio_runtime_services::claude_controls::service::ControlSession>,
        studio_runtime_services::account_session_runtime::AccountSessionError,
    > {
        let session = self
            .sessions
            .read()
            .ok()
            .and_then(|sessions| sessions.get(account).cloned())
            .filter(|session| session.is_live())
            .ok_or(
                studio_runtime_services::account_session_runtime::AccountSessionError::Requires(
                    "provider-session/claude-controls",
                ),
            )?;
        let port =
            studio_runtime_services::account_worker::retained::consumers::ClaudePort::new(session)
                .map_err(|_| {
                    studio_runtime_services::account_session_runtime::AccountSessionError::Refused(
                        "Claude account session is unavailable",
                    )
                })?;
        Ok(Arc::new(
            studio_runtime_services::claude_runtime::RetainedClaudeSession::new(Arc::new(port)),
        ))
    }

    fn request(
        &self,
        request: studio_runtime_services::account_session_runtime::ProviderRequest,
    ) -> studio_runtime_services::account_session_runtime::SessionFuture {
        let session = self
            .sessions
            .read()
            .ok()
            .and_then(|sessions| sessions.get(&request.account).cloned());
        Box::pin(async move {
            let Some(session) = session.filter(|session| session.is_live()) else {
                return Ok(not_sent_receipt(request, studio_runtime_services::account_session_runtime::NotSentReason::NoLiveAccountWorker));
            };
            dispatch_request(session, request).await
        })
    }

    fn transfer_request(
        &self,
        request: studio_runtime_services::account_session_runtime::TransferProviderRequest,
    ) -> studio_runtime_services::account_session_runtime::SessionFuture {
        use studio_runtime_services::{
            account_session_runtime::{
                AccountSessionError, NotSentReason, ProviderResponse, RequestCertainty,
                SessionReceipt,
            },
            account_worker::retained::{RequestResult, SessionRequest, TransferAuthorization},
        };
        let session = self
            .sessions
            .read()
            .ok()
            .and_then(|sessions| sessions.get(&request.source_account).cloned());
        Box::pin(async move {
            let not_sent = |reason| SessionReceipt {
                scope: request.expected_scope.clone(),
                operation: request.operation.clone(),
                ticket: None,
                certainty: RequestCertainty::NotSent,
                result: None,
                not_sent_reason: Some(reason),

                replayed: false,
            };
            let Some(session) = session.filter(|session| session.is_live()) else {
                return Ok(not_sent(NotSentReason::NoLiveAccountWorker));
            };
            if session.scope() != &request.expected_scope
                || request.expected_scope.account != request.source_account
            {
                return Ok(not_sent(NotSentReason::ExpectedScopeMismatch));
            }
            let allowed_params = request.params.as_object().is_some_and(|params| {
                params.len() == 1
                    && params.get("threadId").and_then(serde_json::Value::as_str)
                        == Some(request.thread_id.as_str())
            });
            if !allowed_params
                || !matches!(
                    request.method.as_str(),
                    "thread/queue/list" | "thread/backgroundTerminals/list" | "thread/unsubscribe"
                )
                || !request.operation.starts_with(&format!(
                    "transfer:{}:{}:",
                    request.transfer_id, request.member
                ))
                || tokio::time::Instant::from_std(request.deadline) <= tokio::time::Instant::now()
            {
                return Err(AccountSessionError::Refused(
                    "Invalid account transfer source preflight",
                ));
            }
            let transfer = TransferAuthorization {
                transfer_id: request.transfer_id,
                member: request.member,
                source_account: request.source_account,
                thread_id: request.thread_id,
            };
            let scope = request.expected_scope.clone();
            let operation = request.operation.clone();
            let response = session
                .request(SessionRequest {
                    scope: scope.clone(),
                    expected_scope: Some(scope.clone()),
                    operation: operation.clone(),
                    actor: None,
                    transfer: Some(transfer),
                    deleted_actor_session: None,
                    method: request.method,
                    params: request.params,
                    deadline: request.deadline,
                })
                .await;
            match response {
                Ok(receipt) if receipt.scope == scope && receipt.operation == operation => {
                    let (certainty, result) = match receipt.result {
                        RequestResult::Reply(value) => (
                            RequestCertainty::Written,
                            Some(ProviderResponse::Reply(value)),
                        ),
                        RequestResult::Rejected(value) => (
                            RequestCertainty::Written,
                            Some(ProviderResponse::Rejected(value)),
                        ),
                        RequestResult::NotSent => (RequestCertainty::NotSent, None),
                        RequestResult::Unknown => (RequestCertainty::Unknown, None),
                    };
                    Ok(SessionReceipt {
                        scope: receipt.scope,
                        operation: receipt.operation,
                        ticket: Some(receipt.ticket),
                        certainty,
                        result,
                        not_sent_reason: None,
                        replayed: false,
                    })
                }
                Ok(_) => Err(AccountSessionError::Unknown),
                Err(studio_runtime_services::account_worker::retained::PortError::NotSent)
                | Err(studio_runtime_services::account_worker::retained::PortError::Identity)
                | Err(
                    studio_runtime_services::account_worker::retained::PortError::ScopeMismatch,
                ) => Ok(not_sent(NotSentReason::WorkerNotSent)),
                Err(studio_runtime_services::account_worker::retained::PortError::Unknown) => {
                    Err(AccountSessionError::Unknown)
                }
                Err(_) => Err(AccountSessionError::Refused(
                    "Account transfer source request was rejected",
                )),
            }
        })
    }

    fn callback_barrier(
        &self,
        account: String,
        expected_scope: studio_runtime_services::account_session_runtime::ConnectionScope,
        deadline: std::time::Instant,
    ) -> studio_runtime_services::account_session_runtime::BarrierFuture {
        let session = self
            .sessions
            .read()
            .ok()
            .and_then(|sessions| sessions.get(&account).cloned());
        Box::pin(async move {
            let Some(session) = session.filter(|session| session.is_live()) else {
                return Err(
                    studio_runtime_services::account_session_runtime::AccountSessionError::Refused(
                        "No live account worker",
                    ),
                );
            };
            session
                .barrier(&expected_scope, deadline)
                .await
                .map_err(|error| match error {
                    studio_runtime_services::account_worker::retained::PortError::Unknown => {
                        studio_runtime_services::account_session_runtime::AccountSessionError::Unknown
                    }
                    studio_runtime_services::account_worker::retained::PortError::Identity
                    | studio_runtime_services::account_worker::retained::PortError::ScopeMismatch => {
                        studio_runtime_services::account_session_runtime::AccountSessionError::Refused("Provider session scope changed")
                    }
                    _ => studio_runtime_services::account_session_runtime::AccountSessionError::Refused("Callback barrier was not completed"),
                })
        })
    }

    fn recover_connection(
        &self,
        account: String,
        agent_id: String,
    ) -> studio_runtime_services::account_session_runtime::RecoveryFuture {
        let session = self
            .sessions
            .read()
            .ok()
            .and_then(|sessions| sessions.get(&account).cloned());
        Box::pin(async move {
            let Some(session) = session.filter(|session| session.is_live()) else {
                return Err(
                    studio_runtime_services::account_session_runtime::AccountSessionError::Unknown,
                );
            };
            session.recover_connection(agent_id).await.map_err(|error| match error {
                studio_runtime_services::account_worker::retained::PortError::Unknown => {
                    studio_runtime_services::account_session_runtime::AccountSessionError::Unknown
                }
                studio_runtime_services::account_worker::retained::PortError::Identity => {
                    studio_runtime_services::account_session_runtime::AccountSessionError::Refused("Provider session scope changed")
                }
                _ => studio_runtime_services::account_session_runtime::AccountSessionError::Refused("Connection recovery could not be started"),
            })
        })
    }
}

fn not_sent_receipt(
    request: studio_runtime_services::account_session_runtime::ProviderRequest,
    reason: studio_runtime_services::account_session_runtime::NotSentReason,
) -> studio_runtime_services::account_session_runtime::SessionReceipt {
    studio_runtime_services::account_session_runtime::SessionReceipt {
        scope: request.expected_scope.unwrap_or(
            studio_runtime_services::account_session_runtime::ConnectionScope {
                account: request.account,
                connection: String::new(),
                generation: 0,
            },
        ),
        operation: request.operation,
        ticket: None,
        certainty: studio_runtime_services::account_session_runtime::RequestCertainty::NotSent,
        result: None,
        not_sent_reason: Some(reason),
        replayed: false,
    }
}

async fn dispatch_request(
    port: studio_runtime_services::account_worker::retained::RetainedSessionPort,
    request: studio_runtime_services::account_session_runtime::ProviderRequest,
) -> Result<
    studio_runtime_services::account_session_runtime::SessionReceipt,
    studio_runtime_services::account_session_runtime::AccountSessionError,
> {
    use studio_runtime_services::{
        account_session_runtime::{ProviderResponse, RequestCertainty, SessionReceipt},
        account_worker::retained::{RequestResult, SessionRequest},
    };

    let scope = port.scope().clone();
    if request
        .expected_scope
        .as_ref()
        .is_some_and(|expected| expected != &scope)
    {
        return Ok(not_sent_receipt(
            request,
            studio_runtime_services::account_session_runtime::NotSentReason::ExpectedScopeMismatch,
        ));
    }
    let fallback_request = request.clone();
    let retained_request = SessionRequest {
        scope: scope.clone(),
        expected_scope: request.expected_scope.clone(),
        operation: request.operation.clone(),
        actor: request.actor,
        transfer: None,
        deleted_actor_session: request.deleted_actor_session.clone(),
        method: request.method,
        params: request.params,
        deadline: request.deadline,
    };
    let receipt = match port.request(retained_request).await {
        Ok(receipt) => receipt,
        Err(studio_runtime_services::account_worker::retained::PortError::NotSent)
        | Err(studio_runtime_services::account_worker::retained::PortError::Deadline) => {
            return Ok(not_sent_receipt(
                fallback_request,
                studio_runtime_services::account_session_runtime::NotSentReason::DeadlineExpired,
            ));
        }
        Err(studio_runtime_services::account_worker::retained::PortError::ScopeMismatch) => {
            return Ok(not_sent_receipt(
                fallback_request,
                studio_runtime_services::account_session_runtime::NotSentReason::ExpectedScopeMismatch,
            ));
        }
        Err(error) => {
            return Err(match error {
                studio_runtime_services::account_worker::retained::PortError::Unknown => {
                    studio_runtime_services::account_session_runtime::AccountSessionError::Unknown
                }
                studio_runtime_services::account_worker::retained::PortError::Identity => {
                    studio_runtime_services::account_session_runtime::AccountSessionError::Refused(
                        "Provider session identity changed",
                    )
                }
                _ => {
                    studio_runtime_services::account_session_runtime::AccountSessionError::Refused(
                        "Provider request was refused",
                    )
                }
            });
        }
    };
    let (certainty, result) = match receipt.result {
        RequestResult::Reply(value) => (
            RequestCertainty::Written,
            Some(ProviderResponse::Reply(value)),
        ),
        RequestResult::Rejected(value) => (
            RequestCertainty::Written,
            Some(ProviderResponse::Rejected(value)),
        ),
        RequestResult::NotSent => (RequestCertainty::NotSent, None),
        RequestResult::Unknown => (RequestCertainty::Unknown, None),
    };
    Ok(SessionReceipt {
        scope: receipt.scope,
        operation: receipt.operation,
        ticket: Some(receipt.ticket),
        certainty,
        result,
        not_sent_reason: (certainty == RequestCertainty::NotSent).then_some(
            studio_runtime_services::account_session_runtime::NotSentReason::WorkerNotSent,
        ),
        replayed: receipt.replayed,
    })
}

impl AccountRegistry {
    #[doc(hidden)]
    pub async fn new(
        store: Arc<RuntimeStore>,
        launcher: Arc<dyn AccountLauncher>,
        config: AccountRegistryConfig,
    ) -> Result<Self, WorkerError> {
        Self::new_with_broker(
            store,
            launcher,
            Arc::new(Notify::new()),
            AccountSessionBroker::new(),
            config,
            None,
        )
        .await
    }

    pub async fn new_with_broker(
        store: Arc<RuntimeStore>,
        launcher: Arc<dyn AccountLauncher>,
        monitor_dispatch_wake: Arc<Notify>,
        session_broker: AccountSessionBroker,
        config: AccountRegistryConfig,
        managed_tool_runtime: Option<
            Arc<studio_runtime_services::provider_callbacks::ManagedToolRuntime>,
        >,
    ) -> Result<Self, WorkerError> {
        let hints = Arc::new(RegistryHints::default());
        hints.observe(&store);
        let bridge = StoreBridge::new(store.clone()).await?;
        bridge.set_monitor_dispatch_wake(monitor_dispatch_wake);
        let ensure_requests = session_broker.take_ensure_receiver()?;
        if let Some(runtime) = managed_tool_runtime {
            bridge.set_managed_tool_runtime(runtime);
        }
        Ok(Self {
            store,
            bridge,
            launcher,
            radio_clock: Arc::new(event_time),
            workers: BTreeMap::new(),
            worker_completion_ids: BTreeMap::new(),
            worker_retry_at: BTreeMap::new(),
            worker_retry_delay: BTreeMap::new(),
            idle_dormant: BTreeSet::new(),
            accepted_archive_batch_full: false,
            next_worker_completion_id: 0,
            worker_completions: FuturesUnordered::new(),
            command_capacity_waiters: FuturesUnordered::new(),
            command_capacity_waiting: BTreeSet::new(),
            session_broker,
            accepted_archive_tick_enabled: config.accepted_archive_tick_enabled,
            ensure_requests,
            after_account: String::new(),
            after_agent: BTreeMap::new(),
            hints,
        })
    }

    #[doc(hidden)]
    pub fn set_radio_clock_for_test(&mut self, clock: Arc<dyn Fn() -> f64 + Send + Sync>) {
        self.radio_clock = clock;
    }

    #[doc(hidden)]
    pub async fn close_worker_for_test(&mut self, account: &str) -> Result<(), WorkerError> {
        let worker = self.workers.get_mut(account).ok_or(WorkerError::Closed)?;
        worker.cancel();
        worker.completion().await
    }

    pub async fn start(&mut self, account: &str) -> Result<(), WorkerError> {
        if self.workers.contains_key(account) {
            return Ok(());
        }
        let launch = self.launcher.launch(account.to_owned()).await?;
        let worker = start_account_worker_with_publication_sink(
            self.bridge.clone(),
            account.to_owned(),
            launch.factory,
            launch.options,
            launch.config,
            launch.resources,
            launch.publication_sink,
        )
        .await?;
        self.session_broker
            .insert_session(account.to_owned(), worker.retained_session())?;
        self.workers.insert(account.to_owned(), worker);
        self.next_worker_completion_id = self
            .next_worker_completion_id
            .checked_add(1)
            .ok_or(WorkerError::TaskFailed)?;
        let completion_id = self.next_worker_completion_id;
        let mut finished = self
            .workers
            .get(account)
            .ok_or(WorkerError::Closed)?
            .completion_signal();
        let completion_account = account.to_owned();
        self.worker_completion_ids
            .insert(completion_account.clone(), completion_id);
        self.worker_completions.push(Box::pin(async move {
            if finished.wait_for(|done| *done).await.is_err() {
                studio_runtime_services::diagnostics::log_stderr(format_args!(
                    "account worker ended before publishing its completion signal"
                ));
            }
            (completion_account, completion_id)
        }));
        self.worker_retry_at.remove(account);
        self.worker_retry_delay.remove(account);
        Ok(())
    }

    async fn ensure_account_session(
        &mut self,
        account: &str,
    ) -> Result<studio_runtime_services::account_session_runtime::ConnectionScope, WorkerError>
    {
        // An explicit session request is fresh demand, even when no agent wake
        // row exists (for example, a model-catalog request).
        self.idle_dormant.remove(account);
        if let Some(scope) = self.session_broker.live_scope(account) {
            return Ok(scope);
        }
        if let Some(mut worker) = self.workers.remove(account) {
            self.worker_completion_ids.remove(account);
            self.session_broker.remove_session(account)?;
            // A retained worker can lose its session before its join handle is
            // observed by the registry select loop. Stop and reap it before
            // starting the replacement so concurrent ensures still own one worker.
            if worker.shutdown().await.is_err() {
                // Completion was observed; the failed worker no longer owns a live session.
                studio_runtime_services::diagnostics::log_stderr(format_args!(
                    "account worker exited with an error before replacement"
                ));
            }
        }
        self.start(account).await?;
        self.session_broker
            .live_scope(account)
            .ok_or(WorkerError::Closed)
    }

    async fn dispatch_account_page(
        &mut self,
        account: &str,
        pending_agents: Vec<(String, i64, Option<String>)>,
        shutdown: Option<&Shutdown>,
    ) -> Result<AccountPageDispatch, WorkerError> {
        if !allow_dormant_reconcile(&mut self.idle_dormant, account, !pending_agents.is_empty()) {
            self.after_agent.remove(account);
            return Ok(AccountPageDispatch::Exhausted);
        }
        let page_was_full = pending_agents.len() == WAKE_PAGE;
        let (image_agents, agents, image_recovery) =
            classify_pending_agents(self.store.clone(), account.to_owned(), pending_agents).await?;
        for agent_id in &image_agents {
            self.after_agent
                .insert(account.to_owned(), agent_id.clone());
        }
        // An image-only pending page must not start an account provider
        // outside the workspace image's shared mount namespace.
        if agents.is_empty()
            && (!image_agents.is_empty() || image_recovery)
            && !self.workers.contains_key(account)
        {
            if image_recovery {
                studio_runtime_services::diagnostics::log_stderr(format_args!(
                    "account registry refused recovery for account {account}: workspace-state/image-execution"
                ));
            }
            return Ok(if page_was_full {
                AccountPageDispatch::Continue
            } else {
                AccountPageDispatch::Exhausted
            });
        }
        use studio_runtime_services::account_start_gate::{AgentStartWait, StartWaitReason};
        // The registry owns provider startup, before a worker can claim input.
        if !self.workers.contains_key(account) {
            let waits = agents
                .iter()
                .map(|(agent, epoch)| {
                    (
                        agent.clone(),
                        AgentStartWait::new(StartWaitReason::ProviderSession, *epoch),
                    )
                })
                .collect();
            studio_runtime_services::account_start_gate::show_blocked(&self.store, waits).await?;
        }
        if let Err(error) = self.start(account).await {
            // A launch that only a sign-in can fix holds these starts with
            // their reason and action, instead of a silent provider wait.
            if let Some(reason) =
                studio_runtime_services::account_auth_gate::launch_hold_reason(&error)
            {
                let held = agents.clone();
                if let Err(hold) = studio_runtime_services::account_auth_gate::hold_unstarted(
                    &self.store,
                    held,
                    reason,
                )
                .await
                {
                    studio_runtime_services::diagnostics::log_stderr(format_args!(
                        "account {account} sign-in hold was not recorded: {hold}"
                    ));
                }
            }
            return Ok(AccountPageDispatch::StartFailed(error));
        }
        if shutdown.is_some_and(|stop| *stop.subscribe().borrow()) {
            return Ok(AccountPageDispatch::Exhausted);
        }
        let worker = self.workers.get(account).ok_or(WorkerError::Closed)?;
        for (agent_id, epoch) in agents {
            match worker.try_send(AccountCommand::Wake {
                agent_id: agent_id.clone(),
                epoch,
            }) {
                Ok(()) => {
                    self.after_agent.insert(account.to_owned(), agent_id);
                }
                // Durable input remains pending. A one-shot capacity waiter
                // resumes this account's cursor when the worker drains its lane.
                Err(error) => match *error {
                    mpsc::error::TrySendError::Full(_) => {
                        // The ordinary selection cursor wraps at the end of an
                        // account. If this is the first candidate after that
                        // wrap, restart the strict continuation at the head.
                        if self
                            .after_agent
                            .get(account)
                            .is_some_and(|after| agent_id <= *after)
                        {
                            self.after_agent.remove(account);
                        }
                        if self.command_capacity_waiting.insert(account.to_owned()) {
                            let account_waiting = account.to_owned();
                            let capacity = worker.wait_for_command_capacity();
                            self.command_capacity_waiters
                                .push(Box::pin(async move { (account_waiting, capacity.await) }));
                        }
                        return Ok(AccountPageDispatch::Full);
                    }
                    mpsc::error::TrySendError::Closed(_) => {
                        return Ok(AccountPageDispatch::Exhausted);
                    }
                },
            }
        }
        Ok(if page_was_full {
            AccountPageDispatch::Continue
        } else {
            AccountPageDispatch::Exhausted
        })
    }

    async fn continue_account_after(
        &mut self,
        account: &str,
        shutdown: Option<&Shutdown>,
    ) -> Result<(), WorkerError> {
        loop {
            if shutdown.is_some_and(|stop| *stop.subscribe().borrow()) {
                return Ok(());
            }
            let rows = wake_page_after(
                &self.store,
                account,
                self.after_agent.get(account).cloned().unwrap_or_default(),
            )
            .await?;
            if rows.is_empty() {
                self.after_agent.remove(account);
                return Ok(());
            }
            match self.dispatch_account_page(account, rows, shutdown).await? {
                AccountPageDispatch::Continue => continue,
                AccountPageDispatch::Exhausted => {
                    self.after_agent.remove(account);
                    return Ok(());
                }
                AccountPageDispatch::Full => return Ok(()),
                AccountPageDispatch::StartFailed(error) => return Err(error),
            }
        }
    }

    async fn resume_account_after_capacity(
        &mut self,
        account: &str,
        shutdown: Option<&Shutdown>,
    ) -> Result<(), WorkerError> {
        if self
            .worker_retry_at
            .get(account)
            .is_some_and(|retry_at| *retry_at > tokio::time::Instant::now())
        {
            self.after_agent.remove(account);
            return Ok(());
        }
        self.worker_retry_at.remove(account);
        self.continue_account_after(account, shutdown).await
    }

    /// Startup and post-commit use the same persisted selection, so a crash
    /// between commit and notify cannot strand a queued event.
    pub async fn reconcile(&mut self, shutdown: Option<&Shutdown>) -> Result<(), WorkerError> {
        // This pass reads the whole state, so earlier hints are covered by it.
        drop(self.hints.take());
        self.maintain(true).await?;
        let stopped = || shutdown.is_some_and(|stop| *stop.subscribe().borrow());
        // Every real startup/commit wake begins one bounded full no-wrap pass.
        // Agent cursors are retained only for a command-capacity continuation.
        self.after_account.clear();
        let mut targets = self
            .store
            .read_async("runtime-server/account_registry", |db| {
                target_page(db, "", &BTreeMap::new())
            })
            .await?;
        let mut failure = None;
        loop {
            if targets.is_empty() || stopped() {
                break;
            }
            let full_account_page = targets.len() == MAX_ACCOUNTS;
            for (account, pending_agents) in targets {
                if stopped() {
                    break;
                }
                self.after_account.clone_from(&account);
                self.reconcile_account(account, pending_agents, shutdown, &mut failure)
                    .await?;
            }
            if !full_account_page || stopped() {
                break;
            }
            let after_account = self.after_account.clone();
            targets = self
                .store
                .read_async("runtime-server/account_registry", move |db| {
                    target_page_after(db, &after_account, &BTreeMap::new())
                })
                .await?;
        }
        failure.map_or(Ok(()), Err)
    }

    /// Reconcile only the accounts that the committed rows touch. Cost depends on the
    /// changed rows, not on the number of agents; unknown or overflowed hints take the
    /// full pass.
    async fn reconcile_changes(
        &mut self,
        changes: RegistryChanges,
        shutdown: Option<&Shutdown>,
    ) -> Result<(), WorkerError> {
        if changes.full {
            return self.reconcile(shutdown).await;
        }
        self.maintain(changes.work).await?;
        let targets = self
            .store
            .read_async(
                "runtime-server/account_registry",
                move |db| -> Result<AccountTargetPage, WorkerError> {
                    let accounts = changed_accounts(db, &changes)?;
                    SchedulerRepository::account_wake_for_accounts(
                        db,
                        accounts.iter().map(String::as_str),
                        WAKE_PAGE,
                        event_time(),
                    )
                    .map_err(Into::into)
                },
            )
            .await?;
        let mut failure = None;
        for (account, pending_agents) in targets {
            if shutdown.is_some_and(|stop| *stop.subscribe().borrow()) {
                break;
            }
            self.reconcile_account(account, pending_agents, shutdown, &mut failure)
                .await?;
        }
        failure.map_or(Ok(()), Err)
    }

    /// Per-pass upkeep that does not depend on which accounts changed.
    async fn maintain(&mut self, archive: bool) -> Result<(), WorkerError> {
        let now = tokio::time::Instant::now();
        self.worker_retry_at.retain(|_, deadline| *deadline > now);
        self.tick_radio().await?;
        if archive {
            self.accepted_archive_batch_full = false;
        }
        if archive && self.accepted_archive_tick_enabled {
            let archive_now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            let handled = studio_runtime_services::work_write::accepted_archive_tick(
                Arc::clone(&self.store),
                archive_now,
            )
            .await
            .map_err(|_| WorkerError::TaskFailed)?;
            self.accepted_archive_batch_full =
                handled >= studio_runtime_services::work_write::ACCEPTED_ARCHIVE_PAGE_SIZE;
        }
        Ok(())
    }

    async fn reconcile_account(
        &mut self,
        account: String,
        pending_agents: Vec<(String, i64, Option<String>)>,
        shutdown: Option<&Shutdown>,
        failure: &mut Option<WorkerError>,
    ) -> Result<(), WorkerError> {
        if let Some(retry_at) = self
            .worker_retry_at
            .get(&account)
            .filter(|retry_at| **retry_at > tokio::time::Instant::now())
        {
            use studio_runtime_services::account_start_gate::{AgentStartWait, StartWaitReason};
            let until = event_time()
                + retry_at
                    .saturating_duration_since(tokio::time::Instant::now())
                    .as_secs_f64();
            let waits = pending_agents
                .iter()
                .map(|(agent, epoch, _)| {
                    let mut wait = AgentStartWait::new(StartWaitReason::ProviderSession, *epoch);
                    wait.until = Some(until);
                    (agent.clone(), wait)
                })
                .collect();
            studio_runtime_services::account_start_gate::show_blocked(&self.store, waits).await?;
            return Ok(());
        }
        self.worker_retry_at.remove(&account);
        let mut page = pending_agents;
        loop {
            match self.dispatch_account_page(&account, page, shutdown).await? {
                AccountPageDispatch::Continue => {
                    page = wake_page_after(
                        &self.store,
                        &account,
                        self.after_agent.get(&account).cloned().unwrap_or_default(),
                    )
                    .await?;
                    if page.is_empty() {
                        return Ok(());
                    }
                }
                AccountPageDispatch::Exhausted | AccountPageDispatch::Full => return Ok(()),
                AccountPageDispatch::StartFailed(error) => {
                    failure.get_or_insert(error);
                    return Ok(());
                }
            }
        }
    }

    /// Python's codex_runtime.runtime_maintenance_tick calls codex_radio.tick(self, db)
    /// before dispatch_candidates. Run it before ordinary event reconciliation so
    /// its durable radio_turn events use the same retained account workers.
    async fn tick_radio(&mut self) -> Result<(), WorkerError> {
        let has_rooms = self
            .store
            .read_async("runtime-server/account_registry", |db| {
                db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_rooms WHERE json_type(record,'$.radio')='object')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(studio_runtime_store::StoreError::from)
            })
            .await?;
        if !has_rooms {
            return Ok(());
        }
        let now = (self.radio_clock)();
        let prepared = prepared_host(&self.store).await?;
        let effects = self
            .store
            .transaction_with(move |tx| {
                let mut ingress = RuntimeRadioIngress;
                radio_runtime::tick(tx, &prepared, &mut ingress, now)
            })
            .await
            .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;

        for effect in effects {
            let account = effect
                .agent
                .get("accountKey")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("default");
            let Some(worker) = self.workers.get(account) else {
                continue;
            };
            let sender = worker.radio_interrupt_sender();
            let now = (self.radio_clock)();
            let prepared = prepared_host(&self.store).await?;
            let mut transport = RuntimeRadioInterrupt { sender };
            radio_runtime::handoff_interrupt(&self.store, prepared, &effect, &mut transport, now)
                .await
                .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
        }
        Ok(())
    }

    pub async fn run(
        mut self,
        wake: Arc<Notify>,
        continuation_wake: Arc<Notify>,
        shutdown: Shutdown,
    ) -> Result<(), WorkerError> {
        self.bridge.set_event_wake(wake.clone());
        self.bridge.set_continuation_wake(continuation_wake);
        let mut stop = shutdown.subscribe();
        let mut retry_delay = RETRY_DELAY;
        let mut retry_at = Some(tokio::time::Instant::now());
        let mut archive_deadline_retry_at = None;
        let mut last_reconcile_error: Option<String> = None;
        loop {
            if *stop.borrow() {
                break;
            }
            let trigger = {
                tokio::select! { biased;
                    _ = Shutdown::cancelled(&mut stop) => break,
                    exit = self.worker_completions.next(), if !self.worker_completions.is_empty() => {
                        exit.map(|(account, id)| RegistryTrigger::WorkerExited(account, id))
                    },
                    capacity = self.command_capacity_waiters.next(), if !self.command_capacity_waiters.is_empty() => {
                        capacity.map(|(account, available)| {
                            RegistryTrigger::CommandCapacityAvailable(account, available)
                        })
                    },
                    _ = wake.notified() => Some(RegistryTrigger::Wake),
                    () = self.hints.notified() => Some(RegistryTrigger::Hinted),
                    () = self.store.wal_flush_requested() => Some(RegistryTrigger::FlushRequested),
                    request = self.ensure_requests.recv() => request.map(RegistryTrigger::Ensure),
                    _ = async {
                        match retry_at {
                            Some(deadline) => tokio::time::sleep_until(deadline).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => Some(RegistryTrigger::Retry),
                }
            };
            let mut changes = None;
            let checkpoint_retry_at = match trigger {
                Some(RegistryTrigger::WorkerExited(account, id))
                    if self.worker_completion_ids.get(&account) == Some(&id) =>
                {
                    self.worker_completion_ids.remove(&account);
                    let (result, planned_idle_exit) =
                        if let Some(mut worker) = self.workers.remove(&account) {
                            let result = worker.completion().await;
                            (result, worker.planned_idle_exit())
                        } else {
                            (Ok(()), false)
                        };
                    if let Err(error) = self.session_broker.remove_session(&account) {
                        studio_runtime_services::diagnostics::log_stderr(format_args!(
                            "account session registry update failed: {error}"
                        ));
                    }
                    let clean_exit = result.is_ok();
                    if let Err(error) = result {
                        studio_runtime_services::diagnostics::log_stderr(format_args!(
                            "account worker ended: {error}"
                        ));
                    }
                    if planned_idle_exit && clean_exit {
                        self.idle_dormant.insert(account.clone());
                        self.worker_retry_at.remove(&account);
                        self.worker_retry_delay.remove(&account);
                        continue;
                    }
                    self.idle_dormant.remove(&account);
                    let delay = self
                        .worker_retry_delay
                        .get(&account)
                        .copied()
                        .unwrap_or(RETRY_DELAY);
                    // Do not reconnect in a hot loop after an unavailable provider.
                    let deadline = tokio::time::Instant::now() + delay;
                    self.worker_retry_at.insert(account.clone(), deadline);
                    self.worker_retry_delay
                        .insert(account, next_retry_delay(delay));
                    retry_at = Some(retry_at.map_or(deadline, |current| current.min(deadline)));
                    continue;
                }
                Some(RegistryTrigger::WorkerExited(_, _)) => continue,
                Some(RegistryTrigger::CommandCapacityAvailable(account, available)) => {
                    self.command_capacity_waiting.remove(&account);
                    if !available {
                        self.after_agent.remove(&account);
                        continue;
                    }
                    // Capacity is a local queue event, not a store commit. Resume
                    // only this account's strict no-wrap suffix; do not run global
                    // archive, radio, WAL, or account-page maintenance here.
                    match self
                        .resume_account_after_capacity(&account, Some(&shutdown))
                        .await
                    {
                        // Local queue progress says nothing about a prior global
                        // reconciliation failure; retain its backoff unchanged.
                        Ok(()) => {}
                        Err(error) => {
                            studio_runtime_services::diagnostics::log_stderr(format_args!(
                                "account registry capacity resume deferred: {error}"
                            ));
                            if last_reconcile_error.is_none() {
                                let capacity_retry = tokio::time::Instant::now() + retry_delay;
                                retry_at =
                                    Some(retry_at.map_or(capacity_retry, |current| {
                                        current.min(capacity_retry)
                                    }));
                                retry_delay = next_retry_delay(retry_delay);
                            }
                        }
                    }
                    continue;
                }
                Some(RegistryTrigger::Ensure(request)) => {
                    let response = match self.ensure_account_session(&request.account).await {
                        Ok(scope) if scope.account == request.account => Ok(scope),
                        result => {
                            match result {
                                Err(error) => {
                                    studio_runtime_services::diagnostics::log_stderr(format_args!(
                                        "account session for {} did not start: {error}",
                                        request.account
                                    ))
                                }
                                Ok(_) => {
                                    studio_runtime_services::diagnostics::log_stderr(format_args!(
                                        "account session for {} started with another account scope",
                                        request.account
                                    ))
                                }
                            }
                            Err(studio_runtime_services::account_session_runtime::AccountSessionError::Requires(
                                "provider-session/account-session",
                            ))
                        }
                    };
                    if request.response.send(response).is_err() {
                        studio_runtime_services::diagnostics::log_stderr(format_args!(
                            "account-session requester stopped before receiving its result"
                        ));
                    }
                    continue;
                }
                Some(RegistryTrigger::FlushRequested) => {
                    // Arm the flush on the registry's one retry timer. Commits
                    // that wake no scheduler are flushed this way too.
                    match self.store.wal_checkpoint_retry_after() {
                        Ok(Some(delay)) => {
                            let flush_at = tokio::time::Instant::now() + delay;
                            retry_at =
                                Some(retry_at.map_or(flush_at, |current| current.min(flush_at)));
                        }
                        Ok(None) => {}
                        Err(error) => {
                            studio_runtime_services::diagnostics::log_stderr(format_args!(
                                "account registry WAL flush deadline read deferred: {error}"
                            ));
                            let flush_at = tokio::time::Instant::now() + RETRY_DELAY;
                            retry_at =
                                Some(retry_at.map_or(flush_at, |current| current.min(flush_at)));
                        }
                    }
                    continue;
                }
                Some(RegistryTrigger::Wake | RegistryTrigger::Hinted)
                    if defer_wake(
                        last_reconcile_error.as_deref(),
                        tokio::time::Instant::now(),
                        retry_at,
                    ) =>
                {
                    // Committed rows remain durable until the scheduled retry.
                    continue;
                }
                Some(RegistryTrigger::Retry) => {
                    let store = self.store.clone();
                    let (checkpoint_succeeded, next_checkpoint) =
                        checkpoint_maintenance(store).await;
                    if !checkpoint_succeeded {
                        studio_runtime_services::diagnostics::log_stderr(format_args!(
                            "account registry WAL checkpoint deferred after failure"
                        ));
                    }
                    next_checkpoint
                }
                Some(trigger @ (RegistryTrigger::Wake | RegistryTrigger::Hinted)) => {
                    if matches!(trigger, RegistryTrigger::Hinted) {
                        let taken = self.hints.take();
                        if taken == RegistryChanges::default() {
                            continue;
                        }
                        changes = Some(taken);
                    }
                    let store = self.store.clone();
                    let (checkpoint_succeeded, next_checkpoint) =
                        checkpoint_maintenance(store).await;
                    if !checkpoint_succeeded {
                        studio_runtime_services::diagnostics::log_stderr(format_args!(
                            "account registry WAL checkpoint deferred after commit wake"
                        ));
                    }
                    next_checkpoint
                }
                None => continue,
            };
            let reconciled = match changes {
                Some(changes) => self.reconcile_changes(changes, Some(&shutdown)).await,
                None => self.reconcile(Some(&shutdown)).await,
            };
            match reconciled {
                Err(error) => {
                    let message = error.to_string();
                    if last_reconcile_error.as_deref() != Some(&message) {
                        studio_runtime_services::diagnostics::log_stderr(format_args!(
                            "account registry reconciliation deferred: {message}"
                        ));
                        last_reconcile_error = Some(message);
                    }
                    // A pending WAL flush keeps its deadline during reconcile backoff.
                    let reconcile_retry_at = tokio::time::Instant::now() + retry_delay;
                    retry_at = checkpoint_retry_at.map_or(Some(reconcile_retry_at), |flush_at| {
                        Some(reconcile_retry_at.min(flush_at))
                    });
                    retry_delay = next_retry_delay(retry_delay);
                }
                Ok(()) => {
                    if last_reconcile_error.take().is_some() {
                        studio_runtime_services::diagnostics::log_stderr(format_args!(
                            "account registry reconciliation recovered"
                        ));
                    }
                    retry_delay = RETRY_DELAY;
                    let archive_retry_at = if !self.accepted_archive_tick_enabled {
                        None
                    } else if archive_deadline_read_deferred(
                        archive_deadline_retry_at,
                        tokio::time::Instant::now(),
                    ) {
                        archive_deadline_retry_at
                    } else {
                        archive_deadline_retry_at = None;
                        match studio_runtime_services::work_write::next_archive_retry_at(
                            &self.store,
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs_f64(),
                            self.accepted_archive_batch_full,
                        )
                        .await
                        {
                            Ok(Some(deadline)) => {
                                let now_wall = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs_f64();
                                let delay = (deadline - now_wall).max(0.0);
                                Some(tokio::time::Instant::now() + Duration::from_secs_f64(delay))
                            }
                            Ok(None) => None,
                            Err(error) => {
                                studio_runtime_services::diagnostics::log_stderr(format_args!(
                                    "account registry archive deadline read deferred: {error}"
                                ));
                                let retry_at = tokio::time::Instant::now() + RETRY_DELAY;
                                archive_deadline_retry_at = Some(retry_at);
                                Some(retry_at)
                            }
                        }
                    };
                    retry_at = next_registry_wake_at(
                        self.store.clone(),
                        archive_retry_at,
                        checkpoint_retry_at,
                        &self.worker_retry_at,
                    )
                    .await;
                }
            }
        }
        self.shutdown().await
    }

    pub async fn shutdown(&mut self) -> Result<(), WorkerError> {
        for worker in self.workers.values() {
            worker.cancel();
        }
        let mut failure = None;
        for (_, mut worker) in std::mem::take(&mut self.workers) {
            match tokio::time::timeout(ACCOUNT_WORKER_SHUTDOWN_WAIT, worker.completion()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    failure.get_or_insert(error);
                }
                Err(_) => {
                    studio_runtime_services::diagnostics::log_stderr(format_args!(
                        "runtime server detached an account worker after its {:.0}s shutdown bound",
                        ACCOUNT_WORKER_SHUTDOWN_WAIT.as_secs_f64()
                    ));
                    failure.get_or_insert(WorkerError::TaskFailed);
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

async fn classify_pending_agents(
    store: Arc<RuntimeStore>,
    account: String,
    pending_agents: Vec<(String, i64, Option<String>)>,
) -> Result<(std::collections::BTreeSet<String>, Vec<(String, i64)>, bool), WorkerError> {
    // A host that runs image turns starts the account worker for them like for
    // any agent: workspace preparation mounts the image and runs the turn in
    // `<mount>/repo`, and the worker's first pass makes their restart check.
    if studio_runtime_services::image_workspace::IMAGE_EXECUTION_SUPPORTED {
        let agents = pending_agents
            .into_iter()
            .map(|(id, epoch, _)| (id, epoch))
            .collect();
        return Ok((std::collections::BTreeSet::new(), agents, false));
    }
    let image_agents = pending_agents
        .iter()
        .filter(|(_, _, mode)| mode.as_deref() == Some("image"))
        .map(|(id, _, _)| id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    hold_image_workspace_agents(&store, &image_agents).await?;
    let agents = pending_agents
        .into_iter()
        .filter_map(|(id, epoch, mode)| (mode.as_deref() != Some("image")).then_some((id, epoch)))
        .collect::<Vec<_>>();
    let image_recovery = if agents.is_empty() {
        store
            .read_async("runtime-server/account_registry", move |db| {
                has_image_workspace_recovery(db, &account)
            })
            .await
    } else {
        Ok(false)
    }?;
    Ok((image_agents, agents, image_recovery))
}

/// Accounts whose pending or recovery work can have changed with these rows. A row that no
/// longer exists was deleted, and a deletion never adds work. A child agent also counts
/// for its parent, because a held `child_result` waits for its running children.
fn changed_accounts(
    db: &rusqlite::Connection,
    changes: &RegistryChanges,
) -> Result<BTreeSet<String>, WorkerError> {
    const ACCOUNT: &str = "COALESCE(json_extract(a.record,'$.accountKey'),'default')";
    let mut accounts = BTreeSet::new();
    let mut agent_rows = db
        .prepare_cached(&format!(
            "SELECT {ACCOUNT},(SELECT {ACCOUNT} FROM runtime_agents a WHERE a.id=json_extract(c.record,'$.parentId')) \
             FROM runtime_agents c JOIN runtime_agents a ON a.rowid=c.rowid WHERE c.rowid=?1"
        ))
        .map_err(|error| WorkerError::Store(error.into()))?;
    for row_id in &changes.agent_rows {
        let mut rows = agent_rows
            .query([row_id])
            .map_err(|error| WorkerError::Store(error.into()))?;
        while let Some(row) = rows
            .next()
            .map_err(|error| WorkerError::Store(error.into()))?
        {
            accounts.insert(
                row.get::<_, String>(0)
                    .map_err(|error| WorkerError::Store(error.into()))?,
            );
            if let Some(parent) = row
                .get::<_, Option<String>>(1)
                .map_err(|error| WorkerError::Store(error.into()))?
            {
                accounts.insert(parent);
            }
        }
    }
    for (rows, sql) in [
        (
            &changes.event_rows,
            format!(
                "SELECT {ACCOUNT} FROM runtime_events e JOIN runtime_agents a ON a.id=e.agent WHERE e.rowid=?1"
            ),
        ),
        (
            &changes.event_meta_rows,
            format!(
                "SELECT {ACCOUNT} FROM runtime_event_meta m JOIN runtime_events e ON e.id=m.id JOIN runtime_agents a ON a.id=e.agent WHERE m.rowid=?1"
            ),
        ),
    ] {
        if rows.is_empty() {
            continue;
        }
        let mut statement = db
            .prepare_cached(&sql)
            .map_err(|error| WorkerError::Store(error.into()))?;
        for row_id in rows {
            let found = statement
                .query_map([row_id], |row| row.get::<_, String>(0))
                .map_err(|error| WorkerError::Store(error.into()))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| WorkerError::Store(error.into()))?;
            accounts.extend(found);
        }
    }
    Ok(accounts)
}

/// The account's next page of wakeable agents after its agent cursor.
async fn wake_page_after(
    store: &RuntimeStore,
    account: &str,
    after_agent: String,
) -> Result<Vec<(String, i64, Option<String>)>, WorkerError> {
    let account = account.to_owned();
    Ok(store
        .read_async("runtime-server/account_registry", move |db| {
            SchedulerRepository::account_wake_agents_after(
                db,
                &account,
                &after_agent,
                WAKE_PAGE,
                event_time(),
            )
        })
        .await?)
}

pub fn target_page(
    db: &rusqlite::Connection,
    after_account: &str,
    after_agent: &BTreeMap<String, String>,
) -> Result<AccountTargetPage, WorkerError> {
    SchedulerRepository::account_wake_page(
        db,
        after_account,
        after_agent,
        MAX_ACCOUNTS,
        WAKE_PAGE,
        event_time(),
    )
    .map_err(Into::into)
}

fn target_page_after(
    db: &rusqlite::Connection,
    after_account: &str,
    after_agent: &BTreeMap<String, String>,
) -> Result<AccountTargetPage, WorkerError> {
    SchedulerRepository::account_wake_page_after(
        db,
        after_account,
        after_agent,
        MAX_ACCOUNTS,
        WAKE_PAGE,
        event_time(),
    )
    .map_err(Into::into)
}

struct RuntimeRadioIngress;
impl RadioIngress for RuntimeRadioIngress {
    fn enqueue(
        &mut self,
        db: &rusqlite::Transaction<'_>,
        agent: &str,
        id: &str,
        text: &str,
        now: f64,
    ) -> Result<(), ProjectsError> {
        let record = AgentRepository::get(db, &AgentId::from(agent))?
            .ok_or(ProjectsError::Invalid("radio participant missing"))?;
        QueueEventRepository::put(
            db,
            &QueueEvent {
                id: QueueEventId::from(id),
                agent: AgentId::from(agent),
                kind: "radio_turn".to_owned(),
                text: text.to_owned(),
                status: "pending".to_owned(),
                created: now,
                epoch: record.epoch,
                turn_id: None,
                error: None,
            },
        )?;
        Ok(())
    }
}

struct RuntimeRadioInterrupt {
    sender: mpsc::Sender<AccountCommand>,
}
impl InterruptTransport for RuntimeRadioInterrupt {
    fn dispatch(&mut self, effect: &Interrupt) -> Result<InterruptReceipt, ProjectsError> {
        let agent = effect
            .agent
            .get("id")
            .and_then(serde_json::Value::as_str)
            .ok_or(ProjectsError::Invalid("radio participant missing"))?;
        let thread = effect
            .agent
            .get("threadId")
            .and_then(serde_json::Value::as_str)
            .ok_or(ProjectsError::Invalid("radio thread missing"))?;
        let command = AccountCommand::RadioInterrupt {
            agent_id: agent.to_owned(),
            thread_id: thread.to_owned(),
            turn_id: effect.turn_id.clone(),
        };
        let outcome = if self.sender.try_send(command).is_ok() {
            InterruptOutcome::Accepted
        } else {
            InterruptOutcome::Unknown
        };
        Ok(InterruptReceipt::new(effect.request_id.clone(), outcome))
    }
}

fn event_time() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

/// Keep the durable event pending while exposing the unavailable workspace executor on its agent.
async fn hold_image_workspace_agents(
    store: &RuntimeStore,
    agents: &std::collections::BTreeSet<String>,
) -> Result<(), WorkerError> {
    if agents.is_empty() {
        return Ok(());
    }
    let agents = agents.clone();
    store
        .transaction_with(move |tx| {
            for id in agents {
                let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(id.as_str()))? else {
                    continue;
                };
                if agent
                    .extra
                    .get("workspaceMode")
                    .and_then(|mode| mode.as_str())
                    != Some("image")
                {
                    continue;
                }
                if agent.status == AgentStatus::Paused
                    && agent.extra.get("error").and_then(|value| value.as_str())
                        == Some(IMAGE_EXECUTION_DEPENDENCY)
                    && agent
                        .extra
                        .get("nativeFailureHold")
                        .and_then(|value| value.as_bool())
                        == Some(true)
                {
                    continue;
                }
                agent.status = AgentStatus::Paused;
                agent
                    .extra
                    .insert("autoWake".into(), serde_json::json!(false));
                agent
                    .extra
                    .insert("inFlight".into(), serde_json::json!(false));
                agent
                    .extra
                    .insert("nativeFailureHold".into(), serde_json::json!(true));
                agent.extra.insert(
                    "error".into(),
                    serde_json::json!(IMAGE_EXECUTION_DEPENDENCY),
                );
                agent.extra.insert(
                    "startOutcomeHold".into(),
                    serde_json::json!({"stage":"held","reason":IMAGE_EXECUTION_DEPENDENCY}),
                );
                AgentRepository::put(tx, &agent)?;
            }
            Ok::<_, WorkerError>(())
        })
        .await
}

fn has_image_workspace_recovery(
    db: &rusqlite::Connection,
    account: &str,
) -> Result<bool, WorkerError> {
    Ok(db.query_row(
        "SELECT EXISTS(
          SELECT 1 FROM runtime_agents a
          WHERE COALESCE(json_extract(a.record,'$.accountKey'),'default')=?1
            AND json_extract(a.record,'$.workspaceMode')='image'
            AND json_extract(a.record,'$.deletedAt') IS NULL
            AND COALESCE(json_extract(a.record,'$.remoteWorker'),0)=0
            AND COALESCE(json_extract(a.record,'$.remoteAnchor'),0)=0
            AND ((COALESCE(json_extract(a.record,'$.provider'),'codex')='codex'
                  AND json_extract(a.record,'$.startAttempt.submitted')=1
                  AND json_extract(a.record,'$.startAttempt.connectionGeneration') IS NOT NULL
                  AND json_extract(a.record,'$.status') IN ('starting','running','waiting','paused','interrupted'))
              OR (json_type(a.record,'$.disconnectRecovery')='object' AND (
                  COALESCE(json_extract(a.record,'$.status'),'') NOT IN ('completed','waiting','paused','parked')
                  OR json_extract(a.record,'$.disconnectRecovery.stage') IS NOT NULL))
              OR (json_type(a.record,'$.restartRecovery')='object' AND (
                  COALESCE(json_extract(a.record,'$.status'),'') NOT IN ('completed','waiting','paused','parked')
                  OR COALESCE(json_extract(a.record,'$.restartRecovery.stage'),'')
                     NOT IN ('finished','continued','reattached','input_restored'))))
        )",
        [account],
        |row| row.get(0),
    )?)
}

/// Load the host project snapshot and recheck its paths for one radio write.
async fn prepared_host(store: &RuntimeStore) -> Result<Arc<PreparedHost>, WorkerError> {
    let prepared = PreparedHost::load(store)
        .await
        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/account_registry",
        move || {
            prepared
                .recheck_paths()
                .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
            Ok::<_, WorkerError>(Arc::new(prepared))
        },
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)?
}

#[cfg(test)]
mod bounded_reconcile_tests {
    use super::*;
    use std::collections::BTreeMap;
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus};

    // The image hold exists only on hosts without the image backend.
    #[cfg(not(target_os = "macos"))]
    #[tokio::test(flavor = "current_thread")]
    async fn image_classification_waits_off_executor_while_writer_is_held() {
        use std::{collections::BTreeSet, sync::mpsc};
        use studio_runtime_store::StoreError;

        let directory = tempfile::tempdir().expect("isolated store directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store"),
        );
        let id = AgentId::from("image-agent");
        store
            .transaction(move |transaction| {
                AgentRepository::put(
                    transaction,
                    &AgentRecord {
                        id: id.clone(),
                        root_id: id.clone(),
                        epoch: 1,
                        status: AgentStatus::Queued,
                        extra: BTreeMap::from([
                            ("accountKey".to_owned(), serde_json::json!("account-a")),
                            ("workspaceMode".to_owned(), serde_json::json!("image")),
                            ("autoWake".to_owned(), serde_json::json!(true)),
                            ("inFlight".to_owned(), serde_json::json!(false)),
                        ]),
                    },
                )?;
                Ok(())
            })
            .await
            .expect("image agent stored");

        let (held_tx, held_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer_store = Arc::clone(&store);
        let writer = tokio::spawn(async move {
            writer_store
                .transaction_with(move |_| {
                    held_tx.send(()).expect("announce held writer");
                    release_rx.recv().expect("release held writer");
                    Ok::<_, StoreError>(())
                })
                .await
                .expect("held writer transaction");
        });
        held_rx.await.expect("writer reaches barrier");

        let classification = tokio::spawn(classify_pending_agents(
            Arc::clone(&store),
            "account-a".to_owned(),
            vec![("image-agent".to_owned(), 1, Some("image".to_owned()))],
        ));
        tokio::task::yield_now().await;
        let unrelated_executor_work = tokio::spawn(async { "executor progressed" });
        assert_eq!(
            unrelated_executor_work.await.expect("executor task"),
            "executor progressed"
        );

        release_tx.send(()).expect("release writer barrier");
        writer.await.expect("writer task");
        let (image_agents, agents, recovery) = classification
            .await
            .expect("classification task")
            .expect("classification succeeds");
        assert_eq!(image_agents, BTreeSet::from(["image-agent".to_owned()]));
        assert!(agents.is_empty());
        assert!(!recovery);
    }

    #[tokio::test]
    async fn account_wake_page_returns_workspace_mode_with_pending_agents() {
        let directory = tempfile::tempdir().expect("isolated store directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store");
        let id = AgentId::from("image-agent");
        store
            .transaction(move |transaction| {
                AgentRepository::put(
                    transaction,
                    &AgentRecord {
                        id: id.clone(),
                        root_id: id.clone(),
                        epoch: 1,
                        status: AgentStatus::Queued,
                        extra: BTreeMap::from([
                            ("accountKey".to_owned(), serde_json::json!("default")),
                            ("workspaceMode".to_owned(), serde_json::json!("image")),
                            ("autoWake".to_owned(), serde_json::json!(true)),
                            ("inFlight".to_owned(), serde_json::json!(false)),
                        ]),
                    },
                )?;
                transaction.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES (?1,?1,'annotation','fixture','pending',0,1)",
                    [id.0.as_str()],
                )?;
                Ok(())
            })
            .await
            .expect("pending image agent stored");

        let page = target_page(&store.test_reader().expect("read"), "", &BTreeMap::new())
            .expect("pending target page");
        assert_eq!(page["default"][0].2.as_deref(), Some("image"));
    }

    async fn held_child_result_store() -> (tempfile::TempDir, Arc<RuntimeStore>) {
        let directory = tempfile::tempdir().expect("isolated store directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store"),
        );
        let hold = studio_runtime_store::CHILD_RESULT_COALESCE_MAX;
        let created = event_time();
        store
            .transaction(move |transaction| {
                for (id, parent, status) in [
                    ("lead", None, AgentStatus::Queued),
                    ("worker", Some("lead"), AgentStatus::Running),
                ] {
                    let mut extra =
                        BTreeMap::from([("accountKey".to_owned(), serde_json::json!("default"))]);
                    if let Some(parent) = parent {
                        extra.insert("parentId".to_owned(), serde_json::json!(parent));
                    }
                    AgentRepository::put(
                        transaction,
                        &AgentRecord {
                            id: AgentId::from(id),
                            root_id: AgentId::from("lead"),
                            epoch: 1,
                            status,
                            extra,
                        },
                    )?;
                }
                transaction.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                     VALUES ('child:done:turn','lead','child_result','{}','pending',?1,1)",
                    [created],
                )?;
                studio_runtime_store::OrchestrationSettingsRepository::set_child_result_coalesce(
                    transaction,
                    hold.as_secs(),
                )
            })
            .await
            .expect("held child result stored");

        (directory, store)
    }

    #[tokio::test]
    async fn next_wake_includes_the_earliest_child_result_release() {
        let (_directory, store) = held_child_result_store().await;
        let hold = studio_runtime_store::CHILD_RESULT_COALESCE_MAX;
        let before = tokio::time::Instant::now();
        let wake = next_registry_wake_at(store, None, None, &BTreeMap::new())
            .await
            .expect("a held child result schedules a registry wake");
        assert!(
            wake > before && wake <= before + hold,
            "the wake falls at the release of the held result"
        );
    }

    fn assert_child_result_read_delay_consumed(delay: Duration) {
        // A single blocking thread and a channel gate hold the real read before
        // it starts. Polling once records its clocks before controlled time moves.
        const BLOCKING_THREADS: usize = 1;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(BLOCKING_THREADS)
            .build()
            .expect("isolated current-thread runtime");
        runtime.block_on(async {
            let (_directory, store) = held_child_result_store().await;
            tokio::time::pause();
            let (entered_tx, entered_rx) = oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = studio_runtime_store::blocking_pool::spawn_blocking(
                "runtime-server/tests/child_result_read_gate",
                move || {
                    entered_tx.send(()).expect("announce blocking gate");
                    release_rx.recv().expect("release blocking gate");
                },
            );
            entered_rx.await.expect("blocking gate entered");
            let retries = BTreeMap::new();
            let before = tokio::time::Instant::now();
            let wake = next_registry_wake_at(store, None, None, &retries);
            tokio::pin!(wake);
            assert!(futures_util::poll!(&mut wake).is_pending());
            tokio::time::advance(delay).await;
            release_tx.send(()).expect("release database read");
            blocker.await.expect("blocking gate completes");
            let wake = wake.await.expect("held child result schedules a wake");
            let hold = studio_runtime_store::CHILD_RESULT_COALESCE_MAX;
            assert!(wake <= before + hold, "read delay must consume the hold");
            if delay >= hold {
                assert!(
                    wake <= tokio::time::Instant::now(),
                    "elapsed hold wakes immediately"
                );
            } else {
                assert!(wake > before + delay, "unelapsed hold remains scheduled");
            }
        });
    }

    /// Simulated database wait used within a hold and beyond its end. Unit: time.
    const CHILD_RESULT_READ_DELAY: Duration = Duration::from_secs(1);

    #[test]
    fn child_result_release_consumes_partial_read_delay() {
        assert_child_result_read_delay_consumed(CHILD_RESULT_READ_DELAY);
    }

    #[test]
    fn child_result_release_consumes_elapsed_read_delay() {
        assert_child_result_read_delay_consumed(
            studio_runtime_store::CHILD_RESULT_COALESCE_MAX + CHILD_RESULT_READ_DELAY,
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{
        AccountSessionBroker, MAX_RETRY_DELAY, RETRY_DELAY, allow_dormant_reconcile,
        archive_deadline_read_deferred, defer_wake, next_registry_retry_at, next_retry_delay,
    };
    use std::{
        collections::{BTreeMap, BTreeSet},
        time::Duration,
    };
    use studio_runtime_services::account_session_runtime::{
        NotSentReason, ProviderRequest, RequestCertainty, RetainedAccountSession,
    };

    use super::{RegistryChanges, RegistryHints, changed_accounts};
    use serde_json::json;
    use std::sync::Arc;
    use studio_runtime_store::{
        AgentRecord, ObservedTable, RuntimeStore, SchedulerRepository, Table, TableChange,
    };

    async fn registry_fixture(
        agents: &[(&str, &str, &str, Option<&str>)],
    ) -> (tempfile::TempDir, Arc<RuntimeStore>, Arc<RegistryHints>) {
        let directory = tempfile::tempdir().expect("state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store"),
        );
        let hints = Arc::new(RegistryHints::default());
        hints.observe(&store);
        for &(id, provider, account, parent) in agents {
            let agent = AgentRecord::from_value(json!({
                "id":id, "rootId":id, "parentId":parent, "name":"Registry fixture",
                "epoch":0, "status":"queued", "autoWake":true, "inFlight":false,
                "accountHistory":[], "provider":provider,
                "accountKey":account, "threadId":id, "turnId":"turn", "effort":"high",
                "fastMode":false, "daybreakEnabled":false, "cyberAccessProgram":false,
                "cwd":"/fixture", "deletedAt":null
            }))
            .expect("typed fixture agent");
            store
                .transaction(move |tx| Table::<AgentRecord>::new(tx).put(&agent))
                .await
                .expect("commit agent");
        }
        drop(hints.take());
        (directory, store, hints)
    }

    async fn queue_user_event(store: &RuntimeStore, id: &'static str, agent: &'static str) {
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) \
                     VALUES(?1,?2,'user','input','pending',1,0,NULL,NULL)",
                    [id, agent],
                )?;
                Ok(())
            })
            .await
            .expect("commit event");
    }

    fn accounts_of(store: &RuntimeStore, changes: &RegistryChanges) -> BTreeSet<String> {
        changed_accounts(&store.test_reader().expect("read"), changes).expect("changed accounts")
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn row_hints_name_only_the_touched_accounts_for_both_providers() {
        let (_dir, store, hints) = registry_fixture(&[
            ("codex-a", "codex", "acct-codex", None),
            ("claude-a", "claude", "acct-claude", None),
            ("parent", "codex", "acct-parent", None),
            ("kid", "claude", "acct-claude", Some("parent")),
        ])
        .await;
        for (id, agent, expected) in [
            ("e-claude", "claude-a", "acct-claude"),
            ("e-codex", "codex-a", "acct-codex"),
        ] {
            queue_user_event(&store, id, agent).await;
            let changes = hints.take();
            assert!(!changes.full);
            assert_eq!(
                accounts_of(&store, &changes),
                BTreeSet::from([expected.to_owned()]),
                "{agent}"
            );
        }

        // A child's change also reaches its parent's account: held child results wait for it.
        let kid = AgentRecord::from_value(json!({
            "id":"kid", "rootId":"parent", "parentId":"parent", "name":"Registry fixture",
            "epoch":0, "status":"running", "accountHistory":[], "provider":"claude",
            "accountKey":"acct-claude", "threadId":"kid", "turnId":"turn", "effort":"high",
            "fastMode":false, "daybreakEnabled":false, "cyberAccessProgram":false,
            "cwd":"/fixture", "deletedAt":null
        }))
        .expect("typed child");
        store
            .transaction(move |tx| Table::<AgentRecord>::new(tx).put(&kid))
            .await
            .expect("commit child");
        assert_eq!(
            accounts_of(&store, &hints.take()),
            BTreeSet::from(["acct-claude".to_owned(), "acct-parent".to_owned()])
        );

        // A deleted row never adds work, so it names no account.
        store
            .transaction(|tx| {
                tx.execute("DELETE FROM runtime_events WHERE id='e-codex'", [])?;
                Ok(())
            })
            .await
            .expect("delete event");
        let changes = hints.take();
        assert_eq!(changes.event_rows.len(), 1);
        assert!(accounts_of(&store, &changes).is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rolled_back_commit_leaves_no_registry_hint() {
        let (_dir, store, hints) =
            registry_fixture(&[("codex-a", "codex", "acct-codex", None)]).await;
        let result = store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) \
                     VALUES('rolled-back','codex-a','user','input','pending',1,0,NULL,NULL)",
                    [],
                )?;
                Err::<(), _>(studio_runtime_store::StoreError::RecordContract("test rollback"))
            })
            .await;
        assert!(result.is_err());
        assert_eq!(hints.take(), RegistryChanges::default());
    }

    #[test]
    fn hint_overflow_and_unknown_commits_request_the_full_pass() {
        let hints = RegistryHints::default();
        hints.record(ObservedTable::Events, TableChange::Rows(&[4, 9, 4]));
        hints.record(ObservedTable::Work, TableChange::Rows(&[1]));
        let changes = hints.take();
        assert_eq!(changes.event_rows, BTreeSet::from([4, 9]));
        assert!(changes.work && !changes.full);

        let many = (0..5000).collect::<Vec<i64>>();
        hints.record(ObservedTable::Agents, TableChange::Rows(&many));
        let changes = hints.take();
        assert!(changes.full && changes.agent_rows.is_empty());

        hints.record(ObservedTable::EventMeta, TableChange::Full);
        assert!(hints.take().full);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn targeted_wake_page_reads_only_the_named_accounts() {
        let (_dir, store, _hints) = registry_fixture(&[
            ("codex-a", "codex", "acct-codex", None),
            ("claude-a", "claude", "acct-claude", None),
            ("claude-b", "claude", "acct-other", None),
        ])
        .await;
        for (id, agent) in [("e1", "codex-a"), ("e2", "claude-a"), ("e3", "claude-b")] {
            queue_user_event(&store, id, agent).await;
        }
        let db = store.test_reader().expect("read");
        for (account, agent) in [("acct-codex", "codex-a"), ("acct-claude", "claude-a")] {
            let page = SchedulerRepository::account_wake_for_accounts(&db, [account], 8, 0.0)
                .expect("targeted page");
            assert_eq!(page.keys().collect::<Vec<_>>(), [account]);
            assert_eq!(page[account][0].0, agent);
        }
        let page = SchedulerRepository::account_wake_for_accounts(&db, ["acct-idle"], 8, 0.0)
            .expect("idle account page");
        assert!(page.is_empty());
    }

    #[test]
    fn reconciliation_retry_delay_doubles_to_a_cap() {
        let delays = (0..8)
            .scan(RETRY_DELAY, |delay, _| {
                let current = *delay;
                *delay = next_retry_delay(current);
                Some(current)
            })
            .collect::<Vec<_>>();

        assert_eq!(
            delays,
            [1, 2, 4, 8, 16, 30, 30, 30].map(Duration::from_secs),
        );
        assert_eq!(next_retry_delay(MAX_RETRY_DELAY), MAX_RETRY_DELAY);
    }

    #[test]
    fn planned_idle_exit_stays_dormant_until_new_work_wakes_it() {
        let mut dormant = BTreeSet::from(["account-a".to_owned()]);
        assert!(
            !allow_dormant_reconcile(&mut dormant, "account-a", false),
            "an unrelated reconciliation with no queued work must not reopen the provider"
        );
        assert!(dormant.contains("account-a"));
        assert!(
            allow_dormant_reconcile(&mut dormant, "account-a", true),
            "new queued work is an immediate wake and clears dormancy"
        );
        assert!(!dormant.contains("account-a"));
    }

    #[test]
    fn reconciliation_wakes_wait_for_the_scheduled_retry_after_failure() {
        let now = tokio::time::Instant::now();
        let retry_at = now + Duration::from_secs(2);
        assert!(defer_wake(
            Some("supervisor unavailable"),
            now,
            Some(retry_at)
        ));
        assert!(!defer_wake(None, now, Some(retry_at)));
        assert!(!defer_wake(Some("supervisor unavailable"), now, None));
        assert!(!defer_wake(
            Some("supervisor unavailable"),
            retry_at,
            Some(retry_at),
        ));
    }

    #[test]
    fn deferred_checkpoint_keeps_registry_asleep_until_its_deadline() {
        let now = tokio::time::Instant::now();
        let checkpoint_retry_at = now + studio_runtime_store::WAL_FLUSH_INTERVAL;
        assert_eq!(
            next_registry_retry_at(None, Some(checkpoint_retry_at), &BTreeMap::new()),
            Some(checkpoint_retry_at),
            "a deferred WAL checkpoint schedules the next registry wake without another commit"
        );
    }

    #[test]
    fn command_capacity_wake_preserves_deferred_checkpoint_deadline() {
        let now = tokio::time::Instant::now();
        let checkpoint_retry_at = now + studio_runtime_store::WAL_FLUSH_INTERVAL;

        // Capacity events skip checkpoint I/O but must retain the existing retry
        // deadline when the next scheduler wait is assembled.
        assert_eq!(
            next_registry_retry_at(None, Some(checkpoint_retry_at), &BTreeMap::new()),
            Some(checkpoint_retry_at),
        );
    }

    #[test]
    fn archive_deadline_read_failure_suppresses_repeated_commit_driven_reads() {
        let now = tokio::time::Instant::now();
        let retry_at = now + RETRY_DELAY;
        assert!(archive_deadline_read_deferred(Some(retry_at), now));
        assert!(!archive_deadline_read_deferred(Some(retry_at), retry_at));
        assert!(!archive_deadline_read_deferred(None, now));
    }

    #[tokio::test]
    async fn missing_live_account_worker_is_explicitly_not_sent() {
        let broker = AccountSessionBroker::new();
        let changes = broker.subscribe_changes();
        assert!(broker.current_scope("account-a").is_none());
        let receipt = broker
            .request(ProviderRequest {
                account: "account-a".into(),
                operation: "read-1".into(),
                actor: None,
                expected_scope: None,
                deleted_actor_session: None,
                method: studio_runtime_providers::Method::from_wire("thread/list").unwrap(),
                params: serde_json::json!({"limit":1}),
                deadline: std::time::Instant::now() + Duration::from_secs(1),
            })
            .await
            .expect("absence is a known pre-handoff result");
        assert_eq!(receipt.certainty, RequestCertainty::NotSent);
        assert_eq!(
            receipt.not_sent_reason,
            Some(NotSentReason::NoLiveAccountWorker)
        );
        assert!(receipt.ticket.is_none());
        assert!(receipt.result.is_none());
        assert!(!changes.has_changed().expect("change watcher remains open"));
    }
}
