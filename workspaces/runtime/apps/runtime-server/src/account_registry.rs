//! Runtime ownership of the existing account workers; all input and recovery
//! decisions remain in the worker. Wakes are hints over committed SQLite rows.
//! Registered runtime difference: `image-workspace-dependency-hold` persists
//! an image-workspace refusal on the agent and leaves its input event pending.
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use studio_runtime_process::provider_seam::{OpenOptions, ProviderTaskFactory, Shutdown};
use studio_runtime_services::{
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
const IMAGE_EXECUTION_DEPENDENCY: &str = "workspace-state/image-execution";

fn next_retry_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_RETRY_DELAY)
}

fn defer_wake(
    last_error: Option<&str>,
    now: tokio::time::Instant,
    retry_at: tokio::time::Instant,
) -> bool {
    last_error.is_some() && now < retry_at
}

enum RegistryTrigger {
    Wake,
    Retry,
    Ensure(EnsureRequest),
    WorkerExited(String, Result<(), WorkerError>),
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
    session_broker: AccountSessionBroker,
    last_accepted_archive_tick: Option<Instant>,
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
}

impl AccountSessionBroker {
    pub(crate) fn new() -> Self {
        let (ensure_sender, ensure_receiver) = mpsc::channel(32);
        Self {
            sessions: Arc::default(),
            ensure_sender,
            ensure_receiver: Arc::new(Mutex::new(Some(ensure_receiver))),
        }
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
        runtime: tokio::runtime::Handle,
    ) -> Arc<dyn studio_runtime_services::callback_runtime::CallbackReplyPort> {
        Arc::new(BrokerCallbackReplyPort {
            broker: self.clone(),
            runtime,
        })
    }
}

struct BrokerCallbackReplyPort {
    broker: AccountSessionBroker,
    runtime: tokio::runtime::Handle,
}

impl studio_runtime_services::callback_runtime::CallbackReplyPort for BrokerCallbackReplyPort {
    fn deliver(
        &self,
        reply: &studio_runtime_services::provider_callbacks::Reply,
        deadline: Duration,
    ) -> Result<
        studio_runtime_services::callback_runtime::ReplyReceipt,
        studio_runtime_services::provider_callbacks::CallbackError,
    > {
        use studio_runtime_services::{
            callback_runtime::{ReplyOutcome, ReplyReceipt},
            provider_callbacks::CallbackError,
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
        let outcome = self
            .runtime
            .block_on(session.submit_committed_frame(&expected, operation, frame, until));
        let outcome = match outcome {
            Ok(WriteOutcome::Written) => ReplyOutcome::Written,
            Ok(WriteOutcome::NotSent) => ReplyOutcome::NotSent,
            Ok(WriteOutcome::Unknown)
            | Err(studio_runtime_services::account_worker::retained::PortError::Unknown) => {
                ReplyOutcome::Unknown
            }
            Err(studio_runtime_services::account_worker::retained::PortError::Identity)
            | Err(studio_runtime_services::account_worker::retained::PortError::NotSent)
            | Err(studio_runtime_services::account_worker::retained::PortError::Deadline) => {
                ReplyOutcome::NotSent
            }
            Err(studio_runtime_services::account_worker::retained::PortError::Capacity) => {
                return Err(CallbackError::Invalid("Callback response queue is full"));
            }
            Err(_) => return Err(CallbackError::Unknown),
        };
        Ok(ReplyReceipt { identity, outcome })
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
        )
        .await
    }

    pub async fn new_with_broker(
        store: Arc<RuntimeStore>,
        launcher: Arc<dyn AccountLauncher>,
        monitor_dispatch_wake: Arc<Notify>,
        session_broker: AccountSessionBroker,
        config: AccountRegistryConfig,
    ) -> Result<Self, WorkerError> {
        let bridge = StoreBridge::new(store.clone()).await?;
        bridge.set_monitor_dispatch_wake(monitor_dispatch_wake);
        let ensure_requests = session_broker.take_ensure_receiver()?;
        Ok(Self {
            store,
            bridge,
            launcher,
            radio_clock: Arc::new(event_time),
            workers: BTreeMap::new(),
            session_broker,
            last_accepted_archive_tick: None,
            accepted_archive_tick_enabled: config.accepted_archive_tick_enabled,
            ensure_requests,
            after_account: String::new(),
            after_agent: BTreeMap::new(),
        })
    }

    #[cfg(test)]
    pub fn set_radio_clock_for_test(&mut self, clock: Arc<dyn Fn() -> f64 + Send + Sync>) {
        self.radio_clock = clock;
    }

    #[cfg(test)]
    pub async fn close_worker_for_test(&mut self, account: &str) -> Result<(), WorkerError> {
        let worker = self.workers.get_mut(account).ok_or(WorkerError::Closed)?;
        worker.cancel();
        let sender = worker.radio_interrupt_sender();
        while !sender.is_closed() {
            tokio::task::yield_now().await;
        }
        Ok(())
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
            .sessions
            .write()
            .map_err(|_| WorkerError::TaskFailed)?
            .insert(account.to_owned(), worker.retained_session());
        self.workers.insert(account.to_owned(), worker);
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
            if let Ok(mut sessions) = self.session_broker.sessions.write() {
                sessions.remove(account);
            }
            // A retained worker can lose its session before its join handle is
            // observed by the registry select loop. Stop and reap it before
            // starting the replacement so concurrent ensures still own one worker.
            let _ = worker.shutdown().await;
        }
        self.start(account).await?;
        self.session_broker
            .live_scope(account)
            .ok_or(WorkerError::Closed)
    }

    /// Startup and post-commit use the same persisted selection, so a crash
    /// between commit and notify cannot strand a queued event.
    pub async fn reconcile(&mut self, shutdown: Option<&Shutdown>) -> Result<(), WorkerError> {
        self.tick_radio().await?;
        let archive_tick_at = Instant::now();
        if self.accepted_archive_tick_enabled
            && studio_runtime_services::work_write::accepted_archive_tick_due(
                self.last_accepted_archive_tick,
                archive_tick_at,
            )
        {
            self.last_accepted_archive_tick = Some(archive_tick_at);
            let archive_store = self.store.clone();
            let archive_now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64();
            tokio::task::spawn_blocking(move || {
                studio_runtime_services::work_write::accepted_archive_tick(
                    &archive_store,
                    archive_now,
                )
            })
            .await
            .map_err(|_| WorkerError::TaskFailed)?
            .map_err(|_| WorkerError::TaskFailed)?;
        }
        let stopped = || shutdown.is_some_and(|stop| *stop.subscribe().borrow());
        let store = self.store.clone();
        let after_account = self.after_account.clone();
        let after_agent = self.after_agent.clone();
        let targets =
            tokio::task::spawn_blocking(move || target_page(&store, &after_account, &after_agent))
                .await
                .map_err(|_| WorkerError::TaskFailed)??;
        let mut failure = None;
        for (account, pending_agents) in targets {
            if stopped() {
                break;
            }
            self.after_account.clone_from(&account);
            let image_agents = image_workspace_agents(&self.store, &pending_agents)?;
            hold_image_workspace_agents(&self.store, &image_agents)?;
            for agent_id in &image_agents {
                self.after_agent.insert(account.clone(), agent_id.clone());
            }
            let agents = pending_agents
                .into_iter()
                .filter(|(agent_id, _)| !image_agents.contains(agent_id))
                .collect::<Vec<_>>();
            let image_recovery =
                agents.is_empty() && has_image_workspace_recovery(&self.store, &account)?;
            // An image-only pending page must not start an account provider
            // outside the workspace image's shared mount namespace.
            if agents.is_empty()
                && (!image_agents.is_empty() || image_recovery)
                && !self.workers.contains_key(&account)
            {
                if image_recovery {
                    eprintln!(
                        "account registry refused recovery for account {account}: workspace-state/image-execution"
                    );
                }
                continue;
            }
            if let Err(error) = self.start(&account).await {
                failure.get_or_insert(error);
                continue;
            }
            if stopped() {
                break;
            }
            let worker = self.workers.get(&account).ok_or(WorkerError::Closed)?;
            for (agent_id, epoch) in agents {
                match worker.try_send(AccountCommand::Wake {
                    agent_id: agent_id.clone(),
                    epoch,
                }) {
                    Ok(()) => {
                        self.after_agent.insert(account.clone(), agent_id);
                    }
                    // Durable input remains pending. Timer/next commit retries
                    // the hint; only the worker can claim and submit it.
                    Err(mpsc::error::TrySendError::Full(_)) => break,
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Python's codex_runtime.runtime_maintenance_tick calls codex_radio.tick(self, db)
    /// before dispatch_candidates. Run it before ordinary event reconciliation so
    /// its durable radio_turn events use the same retained account workers.
    async fn tick_radio(&mut self) -> Result<(), WorkerError> {
        let store = self.store.clone();
        let has_rooms = tokio::task::spawn_blocking(move || -> Result<bool, WorkerError> {
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
        let store = self.store.clone();
        let effects = tokio::task::spawn_blocking(move || {
            let prepared = PreparedHost::load(&store)?;
            prepared.recheck_paths()?;
            store.transaction_with(|tx| {
                let mut ingress = RuntimeRadioIngress;
                radio_runtime::tick(tx, &prepared, &mut ingress, now)
            })
        })
        .await
        .map_err(|_| WorkerError::TaskFailed)?
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
            let store = self.store.clone();
            let now = (self.radio_clock)();
            tokio::task::spawn_blocking(move || {
                let prepared = PreparedHost::load(&store)?;
                let mut transport = RuntimeRadioInterrupt { sender };
                radio_runtime::handoff_interrupt(&store, &prepared, &effect, &mut transport, now)
            })
            .await
            .map_err(|_| WorkerError::TaskFailed)?
            .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
        }
        Ok(())
    }

    pub async fn run(mut self, wake: Arc<Notify>, shutdown: Shutdown) -> Result<(), WorkerError> {
        self.bridge.set_event_wake(wake.clone());
        let mut stop = shutdown.subscribe();
        let mut retry_delay = RETRY_DELAY;
        let mut retry_at = tokio::time::Instant::now();
        let mut last_reconcile_error: Option<String> = None;
        loop {
            if *stop.borrow() {
                break;
            }
            // Borrow existing join handles rather than introducing another
            // account task or a second command queue.
            let trigger = {
                let mut exits: FuturesUnordered<_> =
                    self.workers
                        .iter_mut()
                        .map(|(account, worker)| async move {
                            (account.clone(), worker.completion().await)
                        })
                        .collect();
                tokio::select! { biased;
                    _ = Shutdown::cancelled(&mut stop) => break,
                    exit = exits.next(), if !exits.is_empty() => {
                        exit.map(|(account, result)| RegistryTrigger::WorkerExited(account, result))
                    },
                    _ = wake.notified() => Some(RegistryTrigger::Wake),
                    request = self.ensure_requests.recv() => request.map(RegistryTrigger::Ensure),
                    _ = tokio::time::sleep_until(retry_at) => Some(RegistryTrigger::Retry),
                }
            };
            match trigger {
                Some(RegistryTrigger::WorkerExited(account, result)) => {
                    self.workers.remove(&account);
                    if let Ok(mut sessions) = self.session_broker.sessions.write() {
                        sessions.remove(&account);
                    }
                    if let Err(error) = result {
                        eprintln!("account worker ended: {error}");
                    }
                    // Do not reconnect in a hot loop after an unavailable provider.
                    continue;
                }
                Some(RegistryTrigger::Ensure(request)) => {
                    let response = match self.ensure_account_session(&request.account).await {
                        Ok(scope) if scope.account == request.account => Ok(scope),
                        _ => Err(studio_runtime_services::account_session_runtime::AccountSessionError::Requires(
                            "provider-session/account-session",
                        )),
                    };
                    let _ = request.response.send(response);
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
                Some(RegistryTrigger::Wake | RegistryTrigger::Retry) => {}
                None => continue,
            }
            match self.reconcile(Some(&shutdown)).await {
                Err(error) => {
                    let message = error.to_string();
                    if last_reconcile_error.as_deref() != Some(&message) {
                        eprintln!("account registry reconciliation deferred: {message}");
                        last_reconcile_error = Some(message);
                    }
                    retry_at = tokio::time::Instant::now() + retry_delay;
                    retry_delay = next_retry_delay(retry_delay);
                }
                Ok(()) => {
                    if last_reconcile_error.take().is_some() {
                        eprintln!("account registry reconciliation recovered");
                    }
                    retry_delay = RETRY_DELAY;
                    retry_at = tokio::time::Instant::now() + RETRY_DELAY;
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
            if let Err(error) = worker.completion().await {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

pub(crate) fn target_page(
    store: &RuntimeStore,
    after_account: &str,
    after_agent: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Vec<(String, i64)>>, WorkerError> {
    let db = store.read()?;
    SchedulerRepository::account_wake_page(&db, after_account, after_agent, MAX_ACCOUNTS, WAKE_PAGE)
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

fn image_workspace_agents(
    store: &RuntimeStore,
    agents: &[(String, i64)],
) -> Result<std::collections::BTreeSet<String>, WorkerError> {
    let db = store.read()?;
    let mut image_agents = std::collections::BTreeSet::new();
    for (agent_id, _) in agents {
        let mode = db.query_row(
            "SELECT json_extract(record,'$.workspaceMode') FROM runtime_agents WHERE id=?1",
            [agent_id],
            |row| row.get::<_, Option<String>>(0),
        )?;
        if mode.as_deref() == Some("image") {
            image_agents.insert(agent_id.clone());
        }
    }
    Ok(image_agents)
}

/// Keep the durable event pending while exposing the unavailable workspace executor on its agent.
fn hold_image_workspace_agents(
    store: &RuntimeStore,
    agents: &std::collections::BTreeSet<String>,
) -> Result<(), WorkerError> {
    store.transaction_with(|tx| {
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
mod tests {
    use super::{AccountSessionBroker, MAX_RETRY_DELAY, RETRY_DELAY, defer_wake, next_retry_delay};
    use std::time::Duration;
    use studio_runtime_services::account_session_runtime::{
        AccountSessionError, NotSentReason, ProviderRequest, RequestCertainty,
        RetainedAccountSession,
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
        assert!(defer_wake(Some("supervisor unavailable"), now, retry_at));
        assert!(!defer_wake(None, now, retry_at));
        assert!(!defer_wake(
            Some("supervisor unavailable"),
            retry_at,
            retry_at,
        ));
    }

    #[tokio::test]
    async fn missing_live_account_worker_is_explicitly_not_sent() {
        let broker = AccountSessionBroker::new();
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
        let _: Result<_, AccountSessionError> = Ok(receipt);
    }
}
