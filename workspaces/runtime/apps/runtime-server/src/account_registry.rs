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

const MAX_ACCOUNTS: usize = 128;
const WAKE_PAGE: usize = 32;
const RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const ACCOUNT_WORKER_SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
const IMAGE_EXECUTION_DEPENDENCY: &str = "workspace-state/image-execution";

fn next_retry_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_RETRY_DELAY)
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

/// Wake when the earliest held child result stops waiting for its siblings
/// (the stored child-result hold); a failed read retries after `RETRY_DELAY`.
async fn child_result_release_at(
    store: Arc<studio_runtime_store::RuntimeStore>,
) -> Option<tokio::time::Instant> {
    let now = event_time();
    let read = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/account_registry",
        move || -> Result<Option<f64>, WorkerError> {
            let db = store.read()?;
            Ok(SchedulerRepository::child_result_release_at(&db, now)?)
        },
    )
    .await;
    match read {
        Ok(Ok(deadline)) => deadline.map(|deadline| {
            tokio::time::Instant::now() + Duration::from_secs_f64((deadline - now).max(0.0))
        }),
        Ok(Err(error)) => {
            eprintln!("account registry child result release read deferred: {error}");
            Some(tokio::time::Instant::now() + RETRY_DELAY)
        }
        Err(error) => {
            eprintln!("account registry child result release worker failed: {error}");
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
    Retry,
    Ensure(EnsureRequest),
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

pub(crate) type LaunchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AccountLaunch, WorkerError>> + Send + 'a>>;
type WorkerCompletion = Pin<Box<dyn Future<Output = (String, u64)> + Send>>;
type CommandCapacityWaiter = Pin<Box<dyn Future<Output = (String, bool)> + Send>>;
pub(crate) type AccountTargetPage = BTreeMap<String, Vec<(String, i64, Option<String>)>>;

pub(crate) struct AccountLaunch {
    pub factory: Arc<dyn ProviderTaskFactory>,
    pub options: OpenOptions,
    pub config: SessionTaskConfig,
    pub resources: PreparationResources,
    pub publication_sink:
        Arc<dyn studio_runtime_services::notification_runtime_effects::NotificationPostCommitSink>,
}

/// Launch resolution runs outside transactions. A successful return owns the
/// attachment until the worker consumes it; loss of a receipt never retries input.
pub(crate) trait AccountLauncher: Send + Sync {
    fn launch(&self, account: String) -> LaunchFuture<'_>;
}

pub(crate) struct AccountRegistry {
    store: Arc<RuntimeStore>,
    bridge: StoreBridge,
    launcher: Arc<dyn AccountLauncher>,
    radio_clock: Arc<dyn Fn() -> f64 + Send + Sync>,
    workers: BTreeMap<String, AccountHandle>,
    worker_completion_ids: BTreeMap<String, u64>,
    worker_retry_at: BTreeMap<String, tokio::time::Instant>,
    worker_retry_delay: BTreeMap<String, Duration>,
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
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AccountRegistryConfig {
    pub(crate) accepted_archive_tick_enabled: bool,
}

impl Default for AccountRegistryConfig {
    fn default() -> Self {
        Self {
            accepted_archive_tick_enabled: true,
        }
    }
}

#[derive(Clone)]
pub(crate) struct AccountSessionBroker {
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

impl AccountSessionBroker {
    #[cfg(test)]
    pub(crate) fn new() -> Self {
        Self::new_with_change_signal(AccountChangeSignal::new())
    }

    pub(crate) fn new_with_change_signal(changes: AccountChangeSignal) -> Self {
        let (ensure_sender, ensure_receiver) = mpsc::channel(32);
        Self {
            sessions: Arc::default(),
            ensure_sender,
            ensure_receiver: Arc::new(Mutex::new(Some(ensure_receiver))),
            changes,
        }
    }

    pub(crate) fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
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

    pub(crate) fn callback_reply_port(
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

impl studio_runtime_services::callback_runtime::CallbackReplyPort for BrokerCallbackReplyPort {
    fn has_live_session(
        &self,
        context: &studio_runtime_services::provider_callbacks::CallbackContext,
    ) -> bool {
        self.broker
            .live_scope(&context.account)
            .is_some_and(|scope| {
                scope.account == context.account
                    && scope.connection == context.connection
                    && scope.generation == context.generation
            })
    }

    fn deliver<'a>(
        &'a self,
        reply: &'a studio_runtime_services::provider_callbacks::Reply,
        deadline: Duration,
    ) -> studio_runtime_services::callback_runtime::CallbackReplyFuture<'a> {
        Box::pin(async move {
            use studio_runtime_services::{
                account_worker::retained::PortError,
                callback_runtime::{ReplyOutcome, ReplyReceipt},
                provider_session_task::WriteOutcome,
                recovery_history::ConnectionScope,
            };

            let identity = reply.identity.clone();
            let context = &identity.context;
            let expected = ConnectionScope {
                account: context.account.clone(),
                connection: context.connection.clone(),
                generation: context.generation,
            };
            let session = self
                .broker
                .sessions
                .read()
                .ok()
                .and_then(|sessions| sessions.get(&context.account).cloned());
            let Some(session) =
                session.filter(|session| session.is_live() && session.scope() == &expected)
            else {
                return Ok(ReplyReceipt {
                    identity,
                    outcome: ReplyOutcome::NotSent,
                });
            };
            let operation = identity.operation_id();
            let frame = reply.frame();
            let until = std::time::Instant::now() + deadline;
            let outcome = session
                .submit_committed_frame(&expected, operation, frame, until)
                .await;
            let outcome = match outcome {
                Ok(WriteOutcome::Written) => ReplyOutcome::Written,
                Ok(WriteOutcome::NotSent) => ReplyOutcome::NotSent,
                Ok(WriteOutcome::Unknown) | Err(PortError::Unknown) => ReplyOutcome::Unknown,
                Err(PortError::Identity | PortError::NotSent | PortError::Deadline) => {
                    ReplyOutcome::NotSent
                }
                Err(PortError::Capacity) => {
                    return Err(
                        studio_runtime_services::provider_callbacks::CallbackError::Invalid(
                            "Callback response queue is full",
                        ),
                    );
                }
                Err(_) => {
                    return Err(
                        studio_runtime_services::provider_callbacks::CallbackError::Unknown,
                    );
                }
            };
            Ok(ReplyReceipt { identity, outcome })
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
    #[cfg(test)]
    #[allow(dead_code)] // Integration tests use this constructor from a separate crate.
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
        })
    }

    #[cfg(test)]
    #[allow(dead_code)] // Integration tests compile this module in a separate crate.
    pub fn set_radio_clock_for_test(&mut self, clock: Arc<dyn Fn() -> f64 + Send + Sync>) {
        self.radio_clock = clock;
    }

    #[cfg(test)]
    #[allow(dead_code)] // Integration tests compile this module in a separate crate.
    pub async fn close_worker_for_test(&mut self, account: &str) -> Result<(), WorkerError> {
        let worker = self.workers.get_mut(account).ok_or(WorkerError::Closed)?;
        worker.cancel();
        worker.completion().await
    }

    pub(crate) async fn start(&mut self, account: &str) -> Result<(), WorkerError> {
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
                eprintln!("account worker ended before publishing its completion signal");
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
                eprintln!("account worker exited with an error before replacement");
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
                eprintln!(
                    "account registry refused recovery for account {account}: workspace-state/image-execution"
                );
            }
            return Ok(if page_was_full {
                AccountPageDispatch::Continue
            } else {
                AccountPageDispatch::Exhausted
            });
        }
        if let Err(error) = self.start(account).await {
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
            let store = self.store.clone();
            let after_agent = self.after_agent.get(account).cloned().unwrap_or_default();
            let page_account = account.to_owned();
            let rows = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-server/account_registry",
                move || {
                    let db = store.read()?;
                    SchedulerRepository::account_wake_agents_after(
                        &db,
                        &page_account,
                        &after_agent,
                        WAKE_PAGE,
                        event_time(),
                    )
                },
            )
            .await
            .map_err(|_| WorkerError::TaskFailed)??;
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
        let now = tokio::time::Instant::now();
        self.worker_retry_at.retain(|_, deadline| *deadline > now);
        self.tick_radio().await?;
        self.accepted_archive_batch_full = false;
        if self.accepted_archive_tick_enabled {
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
        let stopped = || shutdown.is_some_and(|stop| *stop.subscribe().borrow());
        // Every real startup/commit wake begins one bounded full no-wrap pass.
        // Agent cursors are retained only for a command-capacity continuation.
        self.after_account.clear();
        let store = self.store.clone();
        let after_agent = BTreeMap::new();
        let mut targets = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/account_registry",
            move || target_page(&store, "", &after_agent),
        )
        .await
        .map_err(|_| WorkerError::TaskFailed)??;
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
                if self
                    .worker_retry_at
                    .get(&account)
                    .is_some_and(|retry_at| *retry_at > tokio::time::Instant::now())
                {
                    continue;
                }
                self.worker_retry_at.remove(&account);
                let mut page = pending_agents;
                loop {
                    match self.dispatch_account_page(&account, page, shutdown).await? {
                        AccountPageDispatch::Continue => {
                            let store = self.store.clone();
                            let page_account = account.clone();
                            let after_agent =
                                self.after_agent.get(&account).cloned().unwrap_or_default();
                            page = studio_runtime_process::blocking_pool::spawn_blocking(
                                "runtime-server/account_registry",
                                move || {
                                    let db = store.read()?;
                                    SchedulerRepository::account_wake_agents_after(
                                        &db,
                                        &page_account,
                                        &after_agent,
                                        WAKE_PAGE,
                                        event_time(),
                                    )
                                },
                            )
                            .await
                            .map_err(|_| WorkerError::TaskFailed)??;
                            if page.is_empty() {
                                break;
                            }
                        }
                        AccountPageDispatch::Exhausted | AccountPageDispatch::Full => break,
                        AccountPageDispatch::StartFailed(error) => {
                            failure.get_or_insert(error);
                            break;
                        }
                    }
                }
            }
            if !full_account_page || stopped() {
                break;
            }
            let store = self.store.clone();
            let after_account = self.after_account.clone();
            let after_agent = BTreeMap::new();
            targets = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-server/account_registry",
                move || target_page_after(&store, &after_account, &after_agent),
            )
            .await
            .map_err(|_| WorkerError::TaskFailed)??;
        }
        failure.map_or(Ok(()), Err)
    }

    /// Python's codex_runtime.runtime_maintenance_tick calls codex_radio.tick(self, db)
    /// before dispatch_candidates. Run it before ordinary event reconciliation so
    /// its durable radio_turn events use the same retained account workers.
    async fn tick_radio(&mut self) -> Result<(), WorkerError> {
        let store = self.store.clone();
        let has_rooms = studio_runtime_process::blocking_pool::spawn_blocking("runtime-server/account_registry", move || -> Result<bool, WorkerError> {
            let db = store.read()?;
            db.query_row(
                "SELECT EXISTS(SELECT 1 FROM runtime_rooms WHERE json_type(record,'$.radio')='object')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|error| WorkerError::Store(error.into()))
        })
        .await
        .map_err(|_| WorkerError::TaskFailed)??;
        if !has_rooms {
            return Ok(());
        }
        let now = (self.radio_clock)();
        let store = Arc::clone(&self.store);
        let prepared = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/account_registry",
            move || {
                let prepared = PreparedHost::load(&store)
                    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                prepared
                    .recheck_paths()
                    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                Ok::<_, WorkerError>(Arc::new(prepared))
            },
        )
        .await
        .map_err(|_| WorkerError::TaskFailed)??;
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
            let store = Arc::clone(&self.store);
            let now = (self.radio_clock)();
            let prepared = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-server/account_registry",
                move || {
                    let prepared = PreparedHost::load(&store)
                        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                    prepared
                        .recheck_paths()
                        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
                    Ok::<_, WorkerError>(Arc::new(prepared))
                },
            )
            .await
            .map_err(|_| WorkerError::TaskFailed)??;
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
                    request = self.ensure_requests.recv() => request.map(RegistryTrigger::Ensure),
                    _ = async {
                        match retry_at {
                            Some(deadline) => tokio::time::sleep_until(deadline).await,
                            None => std::future::pending::<()>().await,
                        }
                    } => Some(RegistryTrigger::Retry),
                }
            };
            let checkpoint_retry_at = match trigger {
                Some(RegistryTrigger::WorkerExited(account, id))
                    if self.worker_completion_ids.get(&account) == Some(&id) =>
                {
                    self.worker_completion_ids.remove(&account);
                    let result = if let Some(mut worker) = self.workers.remove(&account) {
                        worker.completion().await
                    } else {
                        Ok(())
                    };
                    if let Err(error) = self.session_broker.remove_session(&account) {
                        eprintln!("account session registry update failed: {error}");
                    }
                    if let Err(error) = result {
                        eprintln!("account worker ended: {error}");
                    }
                    // Do not reconnect in a hot loop after an unavailable provider.
                    let delay = self
                        .worker_retry_delay
                        .get(&account)
                        .copied()
                        .unwrap_or(RETRY_DELAY);
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
                            eprintln!("account registry capacity resume deferred: {error}");
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
                        _ => Err(studio_runtime_services::account_session_runtime::AccountSessionError::Requires(
                            "provider-session/account-session",
                        )),
                    };
                    if request.response.send(response).is_err() {
                        eprintln!("account-session requester stopped before receiving its result");
                    }
                    continue;
                }
                Some(RegistryTrigger::Wake)
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
                        eprintln!("account registry WAL checkpoint deferred after failure");
                    }
                    next_checkpoint
                }
                Some(RegistryTrigger::Wake) => {
                    let store = self.store.clone();
                    let (checkpoint_succeeded, next_checkpoint) =
                        checkpoint_maintenance(store).await;
                    if !checkpoint_succeeded {
                        eprintln!("account registry WAL checkpoint deferred after commit wake");
                    }
                    next_checkpoint
                }
                None => continue,
            };
            match self.reconcile(Some(&shutdown)).await {
                Err(error) => {
                    let message = error.to_string();
                    if last_reconcile_error.as_deref() != Some(&message) {
                        eprintln!("account registry reconciliation deferred: {message}");
                        last_reconcile_error = Some(message);
                    }
                    retry_at = Some(tokio::time::Instant::now() + retry_delay);
                    retry_delay = next_retry_delay(retry_delay);
                }
                Ok(()) => {
                    if last_reconcile_error.take().is_some() {
                        eprintln!("account registry reconciliation recovered");
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
                        let store = self.store.clone();
                        let accepted_batch_full = self.accepted_archive_batch_full;
                        match studio_runtime_process::blocking_pool::spawn_blocking(
                            "runtime-server/account_registry",
                            move || {
                                studio_runtime_services::work_write::next_archive_retry_at(
                                    &store,
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs_f64(),
                                    accepted_batch_full,
                                )
                            },
                        )
                        .await
                        {
                            Ok(Ok(Some(deadline))) => {
                                let now_wall = std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_secs_f64();
                                let delay = (deadline - now_wall).max(0.0);
                                Some(tokio::time::Instant::now() + Duration::from_secs_f64(delay))
                            }
                            Ok(Ok(None)) => None,
                            Ok(Err(error)) => {
                                eprintln!(
                                    "account registry archive deadline read deferred: {error}"
                                );
                                let retry_at = tokio::time::Instant::now() + RETRY_DELAY;
                                archive_deadline_retry_at = Some(retry_at);
                                Some(retry_at)
                            }
                            Err(error) => {
                                eprintln!(
                                    "account registry archive deadline worker failed: {error}"
                                );
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
                    eprintln!(
                        "runtime server detached an account worker after its {:.0}s shutdown bound",
                        ACCOUNT_WORKER_SHUTDOWN_WAIT.as_secs_f64()
                    );
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
        let store = Arc::clone(&store);
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/account_registry",
            move || has_image_workspace_recovery(&store, &account),
        )
        .await
        .map_err(|_| WorkerError::TaskFailed)?
    } else {
        Ok(false)
    }?;
    Ok((image_agents, agents, image_recovery))
}

pub(crate) fn target_page(
    store: &RuntimeStore,
    after_account: &str,
    after_agent: &BTreeMap<String, String>,
) -> Result<AccountTargetPage, WorkerError> {
    let db = store.read()?;
    SchedulerRepository::account_wake_page(
        &db,
        after_account,
        after_agent,
        MAX_ACCOUNTS,
        WAKE_PAGE,
        event_time(),
    )
    .map_err(Into::into)
}

fn target_page_after(
    store: &RuntimeStore,
    after_account: &str,
    after_agent: &BTreeMap<String, String>,
) -> Result<AccountTargetPage, WorkerError> {
    let db = store.read()?;
    SchedulerRepository::account_wake_page_after(
        &db,
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

fn has_image_workspace_recovery(store: &RuntimeStore, account: &str) -> Result<bool, WorkerError> {
    let db = store.read()?;
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

#[cfg(test)]
mod bounded_reconcile_tests {
    use super::*;
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::mpsc,
    };
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, AgentStatus, StoreError};

    #[tokio::test(flavor = "current_thread")]
    async fn image_classification_waits_off_executor_while_writer_is_held() {
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

        let page = target_page(&store, "", &BTreeMap::new()).expect("pending target page");
        assert_eq!(page["default"][0].2.as_deref(), Some("image"));
    }

    #[tokio::test]
    async fn next_wake_includes_the_earliest_child_result_release() {
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

        let before = tokio::time::Instant::now();
        let wake = next_registry_wake_at(store, None, None, &BTreeMap::new())
            .await
            .expect("a held child result schedules a registry wake");
        assert!(
            wake > before && wake <= before + hold,
            "the wake falls at the release of the held result"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{
        AccountSessionBroker, MAX_RETRY_DELAY, RETRY_DELAY, archive_deadline_read_deferred,
        defer_wake, next_registry_retry_at, next_retry_delay,
    };
    use std::{collections::BTreeMap, time::Duration};
    use studio_runtime_services::account_session_runtime::{
        NotSentReason, ProviderRequest, RequestCertainty, RetainedAccountSession,
    };

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
        let checkpoint_retry_at = now + studio_runtime_store::WAL_CHECKPOINT_CADENCE;
        assert_eq!(
            next_registry_retry_at(None, Some(checkpoint_retry_at), &BTreeMap::new()),
            Some(checkpoint_retry_at),
            "a deferred WAL checkpoint schedules the next registry wake without another commit"
        );
    }

    #[test]
    fn command_capacity_wake_preserves_deferred_checkpoint_deadline() {
        let now = tokio::time::Instant::now();
        let checkpoint_retry_at = now + studio_runtime_store::WAL_CHECKPOINT_CADENCE;

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
