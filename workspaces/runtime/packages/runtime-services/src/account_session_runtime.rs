//! Account HTTP effects supplied by the retained session and registry owners.
//!
//! This module never opens a second provider connection or receipt ledger. An
//! absent capability refuses before writes; implementations own durable request
//! reservation, exact receipt binding, bounded waits and restart reconciliation.
//!
//! Registered P18 semantic deviation (approved 2026-10-13): unlike Python, an
//! unconfirmed cancellation is durably latched as uncertain before hand-off and
//! never resent. Exact repeats inspect the receipt. Cancellation stays pending
//! until a confirmed late receipt or auth reconciliation. The regression test is
//! `restart_and_cancellation_latch_never_authorize_another_send`.

use crate::account_worker::retained::RequestActor;
use serde::Deserialize;
use serde_json::Value;
use studio_runtime_providers::Method;
use studio_runtime_providers::RequestTicket;
use studio_runtime_schema::shared_values::{
    ACCOUNT_LIMITS_ERROR_RETRY_SECONDS, ACCOUNT_LIMITS_FULL_READ_FRESH_SECONDS,
    ACCOUNT_LIMITS_WINDOW_FRESH_SECONDS,
};
use thiserror::Error;
use tokio::sync::Notify;

pub type AccountSessionFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, AccountSessionError>> + Send + 'a>>;

#[derive(Debug, Error)]
pub enum AccountSessionError {
    #[error("Not implemented: requires {0}")]
    Requires(&'static str),
    #[error("{0}")]
    Refused(&'static str),
    #[error("Invalid account session response at {0}")]
    InvalidResponse(String),
    #[error("Account session persistence failed")]
    Persistence,
    #[error("The provider request outcome is unknown; inspect its receipt before retrying")]
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotSentReason {
    NoLiveAccountWorker,
    ExpectedScopeMismatch,
    DeadlineExpired,
    WorkerNotSent,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProviderResponse {
    Reply(Value),
    Rejected(Value),
}

/// One request on an account's retained provider session. Agent-bound requests
/// are checked against that agent's epoch, account and thread by the worker;
/// only allow-listed read methods may omit an actor.
#[derive(Clone)]
pub struct ProviderRequest {
    pub account: String,
    pub operation: String,
    pub actor: Option<RequestActor>,
    pub expected_scope: Option<ConnectionScope>,
    /// Exact voice session authorized by a committed agent-delete intent.
    /// Only `thread/realtime/stop` may set this field.
    pub deleted_actor_session: Option<String>,
    pub method: Method,
    pub params: Value,
    pub deadline: std::time::Instant,
}

/// A narrowly scoped source-session preflight authorized by a persisted account
/// transfer member, independent of the agent's already adopted destination account.
#[derive(Clone)]
pub struct TransferProviderRequest {
    pub transfer_id: String,
    pub member: String,
    pub source_account: String,
    pub thread_id: String,
    pub operation: String,
    pub expected_scope: ConnectionScope,
    pub method: Method,
    pub params: Value,
    pub deadline: std::time::Instant,
}

pub use crate::account_driver::CodexLoginRequest as LoginRequest;

#[derive(Clone, Deserialize)]
pub struct ProfileRequest {
    pub account_key: Option<String>,
    pub options: Option<serde_json::Map<String, Value>>,
    pub label: Option<String>,
}

/// Runtime-owned account operations. Login identities cover the complete request
/// content. An uncertain hand-off must only be inspected, never resent. Registry
/// methods publish after durable commit; they must not call native code under a
/// registry/store lock. These defaults also allow composition one capability at a
/// time without advertising success for an absent consumer.
pub trait AccountSessionPort: Send + Sync {
    /// Ensure the account's retained worker is live before an account-scoped read.
    fn ensure_session<'a>(
        &'a self,
        _account: &'a str,
    ) -> AccountSessionFuture<'a, ConnectionScope> {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "provider-session/account-session",
            ))
        })
    }
    fn current_scope(&self, _account: &str) -> Option<ConnectionScope> {
        None
    }
    fn provider_request<'a>(
        &'a self,
        _request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "provider-session/native-request",
            ))
        })
    }
    /// Durably deliver a VM provider notification through the retained account
    /// worker before the guest event cursor is acknowledged.
    fn deliver_vm_notification<'a>(
        &'a self,
        _account: &'a str,
        _agent_id: &'a str,
        _epoch: u64,
        _sequence: u64,
        _frame: studio_runtime_providers::Frame,
    ) -> AccountSessionFuture<'a, ()> {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "account-worker/vm-notification",
            ))
        })
    }
    /// Return the retained Claude control session for an account. The returned
    /// handle is bound to the live account connection and rejects stale receipts.
    fn claude_control_session(
        &self,
        _account: &str,
    ) -> Result<Arc<dyn crate::claude_controls::service::ControlSession>, AccountSessionError> {
        Err(AccountSessionError::Requires(
            "provider-session/claude-controls",
        ))
    }
    fn transfer_provider_request<'a>(
        &'a self,
        _request: TransferProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "account-transfer/source-session-preflight",
            ))
        })
    }
    fn callback_barrier<'a>(
        &'a self,
        _account: &'a str,
        _expected_scope: &'a ConnectionScope,
        _deadline: std::time::Instant,
    ) -> AccountSessionFuture<'a, ()> {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "provider-session/callback-barrier",
            ))
        })
    }
    fn recover_connection<'a>(
        &'a self,
        _account: &'a str,
        _agent_id: &'a str,
    ) -> AccountSessionFuture<'a, Value> {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "account-worker/recovery-history",
            ))
        })
    }
    fn login<'a>(&'a self, _request: LoginRequest) -> AccountSessionFuture<'a, Value> {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "provider-session/codex-login",
            ))
        })
    }
    fn cancel_login<'a>(&'a self, _request: &'a str) -> AccountSessionFuture<'a, Value> {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "provider-session/codex-login",
            ))
        })
    }
    /// Stop accepting and drain durable login/cancel operations before service shutdown.
    fn shutdown_durable_operations<'a>(&'a self) -> AccountSessionFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn discover<'a>(&'a self) -> AccountSessionFuture<'a, Value> {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "provider-session/claude-discovery",
            ))
        })
    }
    fn profile<'a>(&'a self, _request: ProfileRequest) -> AccountSessionFuture<'a, Value> {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "provider-session/claude-profile-auth",
            ))
        })
    }
    fn limits_snapshot(&self, _account: &str) -> Result<Value, AccountSessionError> {
        Err(AccountSessionError::Requires(LIMITS_REQUIRED))
    }
    /// Coalesce by account in the session owner. Return after enqueueing a bounded
    /// read; repeat refreshes must not create unbounded tasks or native requests.
    fn refresh_limits<'a>(&'a self, _account: &'a str) -> AccountSessionFuture<'a, ()> {
        Box::pin(async { Err(AccountSessionError::Requires(LIMITS_REQUIRED)) })
    }
    /// Read only `account/rateLimits/read`, then commit telemetry and publish the
    /// fresh snapshot through the existing notification/usage-resume adapters.
    fn read_limits<'a>(&'a self, _account: &'a str) -> AccountSessionFuture<'a, Value> {
        Box::pin(async { Err(AccountSessionError::Requires(LIMITS_REQUIRED)) })
    }
    /// Read only when the latest account snapshot is outside its provider
    /// freshness window. Explicit callers such as reset preflight keep using
    /// `read_limits` to demand a fresh provider result.
    fn read_limits_if_stale<'a>(&'a self, account: &'a str) -> AccountSessionFuture<'a, Value> {
        Box::pin(async move { self.read_limits(account).await })
    }
    fn persist_limits<'a>(
        &'a self,
        _account: &'a str,
        _value: &'a Value,
    ) -> AccountSessionFuture<'a, ()> {
        Box::pin(async { Err(AccountSessionError::Requires(LIMITS_REQUIRED)) })
    }
}

pub const LIMITS_REQUIRED: &str = "provider-session/account-rate-limits";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitsDecision {
    Cached,
    Refresh,
    Read,
}

/// Schema-checked response retaining absent/null fields and provider extensions.
pub struct LimitsSnapshot(Value);
impl LimitsSnapshot {
    pub fn read(account: &str, value: Value) -> Result<Self, AccountSessionError> {
        let invalid = studio_runtime_schema::first_error(
            "workspace-usage-limits/UsageLimitsResponse",
            &value,
        )
        .map_err(|_| AccountSessionError::InvalidResponse("/".into()))?;
        if let Some((pointer, _)) = invalid {
            return Err(AccountSessionError::InvalidResponse(pointer));
        }
        if value.get("accountKey").is_some_and(|key| key != account) {
            return Err(AccountSessionError::InvalidResponse("/accountKey".into()));
        }
        Ok(Self(value))
    }
    fn is_fresh_for_provider(&self, now: f64) -> bool {
        let error = self
            .0
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
        if error {
            return self
                .0
                .get("checkedAt")
                .and_then(Value::as_f64)
                .is_some_and(|checked_at| {
                    checked_at > 0.0 && now - checked_at < ACCOUNT_LIMITS_ERROR_RETRY_SECONDS
                });
        }
        let at_is_fresh = self
            .0
            .get("at")
            .and_then(Value::as_f64)
            .is_some_and(|at| at > 0.0 && now - at < ACCOUNT_LIMITS_WINDOW_FRESH_SECONDS);
        let read_at_is_fresh =
            self.0
                .get("readAt")
                .and_then(Value::as_f64)
                .is_some_and(|read_at| {
                    read_at > 0.0 && now - read_at < ACCOUNT_LIMITS_FULL_READ_FRESH_SECONDS
                });
        at_is_fresh && read_at_is_fresh
    }

    pub fn decision(&self, cached: bool, now: f64) -> LimitsDecision {
        let data = self.0.get("data").is_some_and(|value| !value.is_null());
        let error = self
            .0
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
        if (cached && (data || error)) || self.is_fresh_for_provider(now) {
            LimitsDecision::Cached
        } else if data {
            LimitsDecision::Refresh
        } else {
            LimitsDecision::Read
        }
    }
    pub fn into_value(self) -> Value {
        self.0
    }
}

/// Same provider freshness window as the Python account runtime. A cold cache
/// waits for the bounded read; stale data is returned while one refresh queues.
pub async fn limits(
    port: &dyn AccountSessionPort,
    account: &str,
    cached: bool,
) -> Result<Value, AccountSessionError> {
    let snapshot = LimitsSnapshot::read(account, port.limits_snapshot(account)?)?;
    let now = timestamp()?;
    match snapshot.decision(cached, now) {
        LimitsDecision::Cached => Ok(snapshot.into_value()),
        LimitsDecision::Refresh => {
            port.refresh_limits(account).await?;
            Ok(snapshot.into_value())
        }
        LimitsDecision::Read => Ok(LimitsSnapshot::read(
            account,
            port.read_limits_if_stale(account).await?,
        )?
        .into_value()),
    }
}

#[cfg(test)]
mod limits_freshness_tests {
    #![allow(clippy::unwrap_used)]

    use super::{
        AccountSessionError, AccountSessionFuture, AccountSessionPort, LimitsDecision,
        LimitsSnapshot, limits,
    };
    use serde_json::{Value, json};
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    const NOW: f64 = 1_000.0;

    fn snapshot(at: Value, read_at: Value, data: Value, error: Value, checked_at: Value) -> Value {
        json!({
            "accountKey":"default",
            "at":at,
            "readAt":read_at,
            "data":data,
            "error":error,
            "checkedAt":checked_at
        })
    }

    #[test]
    fn provider_freshness_requires_both_window_and_complete_read_ages() {
        let notification_fresh_but_full_read_stale = LimitsSnapshot::read(
            "default",
            snapshot(
                json!(NOW - 1.0),
                json!(NOW - 601.0),
                json!({"accountId":"fixture"}),
                Value::Null,
                Value::Null,
            ),
        )
        .unwrap();
        assert_eq!(
            notification_fresh_but_full_read_stale.decision(false, NOW),
            LimitsDecision::Refresh
        );

        let full_read_fresh = LimitsSnapshot::read(
            "default",
            snapshot(
                json!(NOW - 1.0),
                json!(NOW - 1.0),
                json!({"accountId":"fixture"}),
                Value::Null,
                Value::Null,
            ),
        )
        .unwrap();
        assert_eq!(full_read_fresh.decision(false, NOW), LimitsDecision::Cached);
    }

    #[test]
    fn fresh_null_success_and_recent_error_do_not_retry_provider_reads() {
        let null_success = LimitsSnapshot::read(
            "default",
            snapshot(
                json!(NOW - 1.0),
                json!(NOW - 1.0),
                Value::Null,
                Value::Null,
                Value::Null,
            ),
        )
        .unwrap();
        assert_eq!(null_success.decision(true, NOW), LimitsDecision::Cached);
        assert_eq!(null_success.decision(false, NOW), LimitsDecision::Cached);

        let recent_error = LimitsSnapshot::read(
            "default",
            snapshot(
                Value::Null,
                Value::Null,
                Value::Null,
                json!("read failed"),
                json!(NOW - 1.0),
            ),
        )
        .unwrap();
        assert_eq!(recent_error.decision(false, NOW), LimitsDecision::Cached);
    }

    struct ColdCachePort {
        snapshot: Mutex<Value>,
        reads: AtomicUsize,
    }

    impl AccountSessionPort for ColdCachePort {
        fn limits_snapshot(&self, _account: &str) -> Result<Value, AccountSessionError> {
            Ok(self.snapshot.lock().unwrap().clone())
        }

        fn read_limits<'a>(&'a self, _account: &'a str) -> AccountSessionFuture<'a, Value> {
            Box::pin(async move {
                self.reads.fetch_add(1, Ordering::SeqCst);
                let now = super::timestamp().unwrap();
                let result = snapshot(
                    json!(now),
                    json!(now),
                    Value::Null,
                    Value::Null,
                    Value::Null,
                );
                *self.snapshot.lock().unwrap() = result.clone();
                Ok(result)
            })
        }
    }

    #[tokio::test]
    async fn cached_cold_fill_is_not_repeated_after_a_null_success() {
        let port = ColdCachePort {
            snapshot: Mutex::new(snapshot(
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            )),
            reads: AtomicUsize::new(0),
        };

        for _ in 0..5 {
            limits(&port, "default", true).await.unwrap();
        }

        assert_eq!(port.reads.load(Ordering::SeqCst), 1);
    }
}

use crate::account_driver::{
    AccountDriver, AccountDriverError, AccountMutationError, AuthProfile, LoginReservation,
    ProviderAuthReader, SystemProviderAuthReader,
};
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(20);
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

pub use crate::recovery_history::ConnectionScope;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestCertainty {
    Written,
    NotSent,
    Unknown,
}
impl RequestCertainty {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Written => "written",
            Self::NotSent => "not_sent",
            Self::Unknown => "unknown",
        }
    }
}
pub struct SessionReceipt {
    pub scope: ConnectionScope,
    pub operation: String,
    /// Missing only when the retained owner proves the request was not handed
    /// off (for example, no live account worker exists).
    pub ticket: Option<RequestTicket>,
    pub certainty: RequestCertainty,
    pub result: Option<ProviderResponse>,
    pub not_sent_reason: Option<NotSentReason>,
    /// True only when the retained broker returned a checkpoint-verified replay.
    pub replayed: bool,
}

fn provider_receipt_matches(
    receipt: &SessionReceipt,
    account: &str,
    operation: &str,
    expected_scope: Option<&ConnectionScope>,
) -> Result<(), AccountSessionError> {
    let same_identity = receipt.operation == operation && receipt.scope.account == account;
    let valid_scope = match expected_scope {
        None => receipt.certainty == RequestCertainty::NotSent,
        Some(scope) if receipt.scope == *scope => true,
        Some(scope) => {
            receipt.replayed
                && receipt.certainty == RequestCertainty::Written
                && receipt.scope.generation < scope.generation
                && receipt.scope.account == scope.account
        }
    };
    if same_identity && valid_scope {
        Ok(())
    } else {
        Err(AccountSessionError::Unknown)
    }
}
pub type SessionFuture =
    Pin<Box<dyn Future<Output = Result<SessionReceipt, AccountSessionError>> + Send>>;
pub type BarrierFuture = Pin<Box<dyn Future<Output = Result<(), AccountSessionError>> + Send>>;
pub type RecoveryFuture = Pin<Box<dyn Future<Output = Result<Value, AccountSessionError>> + Send>>;

/// The integrator adapts its retained session's request(scope, operation, method,
/// params, absolute deadline) here. Scope lookup must not dispatch a mutation.
pub trait RetainedAccountSession: Send + Sync {
    fn current_scope(&self, _account: &str) -> Option<ConnectionScope> {
        None
    }
    fn ensure_session(&self, _account: String) -> SessionFutureScope {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "provider-session/account-session",
            ))
        })
    }
    fn request(&self, request: ProviderRequest) -> SessionFuture;
    fn deliver_vm_notification(
        &self,
        _account: String,
        _agent_id: String,
        _epoch: u64,
        _sequence: u64,
        _frame: studio_runtime_providers::Frame,
    ) -> Pin<Box<dyn Future<Output = Result<(), AccountSessionError>> + Send>> {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "account-worker/vm-notification",
            ))
        })
    }
    fn claude_control_session(
        &self,
        _account: &str,
    ) -> Result<Arc<dyn crate::claude_controls::service::ControlSession>, AccountSessionError> {
        Err(AccountSessionError::Requires(
            "provider-session/claude-controls",
        ))
    }
    fn transfer_request(&self, _request: TransferProviderRequest) -> SessionFuture {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "account-transfer/source-session-preflight",
            ))
        })
    }
    fn callback_barrier(
        &self,
        _account: String,
        _expected_scope: ConnectionScope,
        _deadline: std::time::Instant,
    ) -> BarrierFuture {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "provider-session/callback-barrier",
            ))
        })
    }
    fn recover_connection(&self, _account: String, _agent_id: String) -> RecoveryFuture {
        Box::pin(async {
            Err(AccountSessionError::Requires(
                "account-worker/recovery-history",
            ))
        })
    }
}

pub type SessionFutureScope =
    Pin<Box<dyn Future<Output = Result<ConnectionScope, AccountSessionError>> + Send>>;

/// Called on the HTTP edge's blocking pool. Only the retained-session wait uses
/// the Tokio handle; no store transaction or registry lock spans that wait.
#[derive(Clone)]
pub struct AccountSessionRuntime {
    driver: Arc<AccountDriver>,
    session: Option<Arc<dyn RetainedAccountSession>>,
    runtime: tokio::runtime::Handle,
    accounts_root: PathBuf,
    base_home: PathBuf,
    home: PathBuf,
    limits: Option<Arc<crate::account_policy::AccountPolicyState>>,
    store: Option<Arc<studio_runtime_store::RuntimeStore>>,
    refreshed_limits: Arc<std::sync::Mutex<std::collections::BTreeMap<String, Value>>>,
    pending_limit_refreshes: Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    limits_read_locks:
        Arc<std::sync::Mutex<std::collections::BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    continuation_wake: Option<Arc<Notify>>,
    durable_operations: Arc<tokio::sync::Mutex<Option<tokio::task::JoinSet<()>>>>,
}

async fn spawn_account_driver<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, AccountSessionError> {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/account_session_runtime",
        operation,
    )
    .await
    .map_err(|_| AccountSessionError::Persistence)
}

async fn persist_limits(
    store: Arc<studio_runtime_store::RuntimeStore>,
    refreshed_limits: Arc<std::sync::Mutex<std::collections::BTreeMap<String, Value>>>,
    account: String,
    value: Value,
    continuation_wake: Option<Arc<Notify>>,
) -> Result<(), AccountSessionError> {
    use rusqlite::OptionalExtension;
    use studio_runtime_store::SyncDocumentsRepository;
    let now = timestamp()?;
    let transaction_account = account.clone();
    let transaction_value = value.clone();
    let cache_account = account;
    let cache_value = value;
    store
        .transaction_with_post_commit(
            move |transaction| {
                let raw = transaction
                    .query_row(
                        "SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                let mut by_account = raw
                    .map(|raw| serde_json::from_str::<Value>(&raw))
                    .transpose()
                    .map_err(|_| studio_runtime_store::StoreError::InvalidSyncObject {
                        scope: "workspace".into(),
                        id: "current".into(),
                    })?
                    .and_then(|entity| entity.pointer("/value/rateLimitsByAccount").cloned())
                    .unwrap_or_else(|| serde_json::json!({}));
                let by_account = by_account
                    .as_object_mut()
                    .ok_or_else(|| studio_runtime_store::StoreError::InvalidSyncObject {
                        scope: "workspace".into(),
                        id: "current".into(),
                    })?;
                by_account.insert(transaction_account.clone(), transaction_value.clone());
                let mut patch = serde_json::json!({"rateLimitsByAccount":by_account});
                if transaction_account == "default" {
                    patch["rateLimits"] = transaction_value.clone();
                }
                SyncDocumentsRepository::patch_entity(transaction, "workspace", "current", patch)
                    .map_err(|_| studio_runtime_store::StoreError::InvalidSyncObject {
                        scope: "workspace".into(),
                        id: "current".into(),
                    })?;
                crate::continuation_runtime::stage_limits_changed_for_account(
                    transaction,
                    &transaction_account,
                    &transaction_value,
                    now,
                )
                .map_err(|error| {
                    studio_runtime_store::StoreError::SchemaMismatch(error.to_string())
                })
            },
            move |changed| {
                refreshed_limits
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(cache_account, cache_value);
                if *changed && let Some(wake) = continuation_wake {
                    wake.notify_one();
                }
            },
        )
        .await
        .map_err(|_| AccountSessionError::Persistence)?;
    Ok(())
}

impl AccountSessionRuntime {
    pub fn new(
        driver: Arc<AccountDriver>,
        session: Option<Arc<dyn RetainedAccountSession>>,
        runtime: tokio::runtime::Handle,
        accounts_root: PathBuf,
        base_home: PathBuf,
        home: PathBuf,
    ) -> Self {
        Self {
            driver,
            session,
            runtime,
            accounts_root,
            base_home,
            home,
            limits: None,
            store: None,
            refreshed_limits: Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new())),
            pending_limit_refreshes: Arc::new(std::sync::Mutex::new(
                std::collections::BTreeSet::new(),
            )),
            limits_read_locks: Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::new())),
            continuation_wake: None,
            durable_operations: Arc::new(tokio::sync::Mutex::new(
                Some(tokio::task::JoinSet::new()),
            )),
        }
    }
    pub fn with_limits(mut self, policy: Arc<crate::account_policy::AccountPolicyState>) -> Self {
        self.limits = Some(policy);
        self
    }
    pub fn with_store(mut self, store: Arc<studio_runtime_store::RuntimeStore>) -> Self {
        self.store = Some(store);
        self
    }
    pub fn with_continuation_wake(mut self, wake: Arc<Notify>) -> Self {
        self.continuation_wake = Some(wake);
        self
    }
    async fn run_durable_operation<T: Send + 'static>(
        &self,
        operation: impl Future<Output = Result<T, AccountSessionError>> + Send + 'static,
    ) -> Result<T, AccountSessionError> {
        let (reply, receiver) = tokio::sync::oneshot::channel();
        {
            let mut owner = self.durable_operations.lock().await;
            let tasks = owner.as_mut().ok_or(AccountSessionError::Requires(
                "provider-session/durable-operations-shutdown",
            ))?;
            while let Some(result) = tasks.try_join_next() {
                result.map_err(|_| AccountSessionError::Unknown)?;
            }
            tasks.spawn(async move {
                let result = operation.await;
                let _ignored = reply.send(result);
            });
        }
        receiver.await.map_err(|_| AccountSessionError::Unknown)?
    }
    async fn drain_durable_operations(&self) -> Result<(), AccountSessionError> {
        let mut owner = self.durable_operations.lock().await;
        let Some(mut tasks) = owner.take() else {
            return Ok(());
        };
        drop(owner);
        let mut failed = false;
        while let Some(result) = tasks.join_next().await {
            failed |= result.is_err();
        }
        if failed {
            Err(AccountSessionError::Unknown)
        } else {
            Ok(())
        }
    }
    async fn persist_limits(
        &self,
        account: &str,
        value: &Value,
    ) -> Result<(), AccountSessionError> {
        let store = Arc::clone(
            self.store
                .as_ref()
                .ok_or(AccountSessionError::Requires(LIMITS_REQUIRED))?,
        );
        let refreshed_limits = Arc::clone(&self.refreshed_limits);
        let account = account.to_owned();
        let value = value.clone();
        let continuation_wake = self.continuation_wake.clone();
        persist_limits(store, refreshed_limits, account, value, continuation_wake).await
    }
    fn limits_read_lock(
        &self,
        account: &str,
    ) -> Result<Arc<tokio::sync::Mutex<()>>, AccountSessionError> {
        let mut locks = self
            .limits_read_locks
            .lock()
            .map_err(|_| AccountSessionError::Persistence)?;
        Ok(Arc::clone(
            locks
                .entry(account.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        ))
    }
    async fn exchange(
        &self,
        account: &str,
        _for_login: bool,
        operation: String,
        method: &'static str,
        params: Value,
        timeout: Duration,
    ) -> Result<SessionReceipt, AccountSessionError> {
        let session = self.session.as_ref().ok_or(AccountSessionError::Requires(
            "provider-session/codex-login",
        ))?;
        let method = Method::from_wire(method).ok_or(AccountSessionError::Refused(
            "Unsupported retained provider method",
        ))?;
        let deadline = Instant::now() + timeout;
        let expected_scope = session.current_scope(account);
        let receipt = {
            tokio::time::timeout_at(
                deadline,
                session.request(ProviderRequest {
                    account: account.to_owned(),
                    operation: operation.clone(),
                    actor: None,
                    expected_scope: expected_scope.clone(),
                    deleted_actor_session: None,
                    method,
                    params,
                    deadline: deadline.into_std(),
                }),
            )
            .await
        };
        match receipt {
            Ok(Ok(receipt))
                if provider_receipt_matches(
                    &receipt,
                    account,
                    &operation,
                    expected_scope.as_ref(),
                )
                .is_ok() =>
            {
                Ok(receipt)
            }
            Ok(Err(error)) => Err(error),
            _ => Err(AccountSessionError::Unknown),
        }
    }
    fn accounts(&self) -> Result<Value, AccountSessionError> {
        let snapshot = self.driver.listing_snapshot().map_err(account_error)?;
        Ok(
            serde_json::json!({"accounts":snapshot.accounts,"archivedAccounts":snapshot.archived_accounts,"defaultAccountKey":snapshot.default_account_key,"logins":snapshot.logins,"supportsDisconnect":true,"supportsDelete":true}),
        )
    }
    fn discover_sync(&self) -> Result<Value, AccountSessionError> {
        self.driver
            .discover_local(&self.home)
            .map_err(mutation_error)?;
        self.accounts()
    }
    fn profile_sync(&self, request: ProfileRequest) -> Result<Value, AccountSessionError> {
        let options = normalize_profile(request.options.unwrap_or_default(), &self.home)?;
        if let Some(account) = request.account_key {
            self.driver
                .update_claude_settings(&account, Value::Object(options), request.label.as_deref())
                .map_err(mutation_error)?;
        } else {
            let home = options
                .get("configDir")
                .and_then(Value::as_str)
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
                .unwrap_or_else(|| self.home.join(".claude"));
            let profile = AuthProfile {
                provider: studio_runtime_providers::ProviderKind::Claude,
                home: home.clone(),
                claude_options: Some(Value::Object(options.clone())),
            };
            SystemProviderAuthReader.invalidate(&profile);
            let metadata = SystemProviderAuthReader.read(&profile);
            self.driver
                .register_claude_verified(
                    options,
                    request.label.as_deref().unwrap_or("Claude Code"),
                    &home,
                    &metadata,
                )
                .map_err(|_| AccountSessionError::Refused("Cannot register this Claude profile"))?;
        }
        self.accounts()
    }
    fn prepare_login_home(&self, request: &str) -> Result<PathBuf, AccountSessionError> {
        let accounts_root = self
            .accounts_root
            .canonicalize()
            .map_err(|_| AccountSessionError::Persistence)?;
        // The accounts directory also contains the registry. Keep it private
        // before creating provider-owned login material beneath it.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&accounts_root, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| AccountSessionError::Persistence)?;
        }
        // create_dir is exclusive. Reuse only a pre-existing real directory
        // beneath accounts_root; this covers a failed durable-intent write,
        // while rejecting symlinks and other path types.
        let home = accounts_root.join(format!("login-{request}"));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&home) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&home)
                    .map_err(|_| AccountSessionError::Persistence)?;
                if !metadata.file_type().is_dir() {
                    return Err(AccountSessionError::Refused(
                        "Login home must be a real directory under the accounts root",
                    ));
                }
            }
            Err(_) => return Err(AccountSessionError::Persistence),
        }
        let home = home
            .canonicalize()
            .map_err(|_| AccountSessionError::Persistence)?;
        if home.parent() != Some(accounts_root.as_path())
            || home.file_name() != Some(std::ffi::OsStr::new(&format!("login-{request}")))
        {
            return Err(AccountSessionError::Persistence);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))
                .map_err(|_| AccountSessionError::Persistence)?;
        }
        let source = self.base_home.join("config.toml");
        let target = home.join("config.toml");
        if source.is_file() && !target.exists() {
            use std::io::{Read, Write};
            let mut config = String::new();
            std::fs::File::open(source)
                .map_err(|_| AccountSessionError::Persistence)?
                .take(MAX_CONFIG_BYTES + 1)
                .read_to_string(&mut config)
                .map_err(|_| AccountSessionError::Persistence)?;
            if config.len() as u64 > MAX_CONFIG_BYTES {
                return Err(AccountSessionError::Refused(
                    "Codex configuration is too large",
                ));
            }
            let restrictions = regex::Regex::new(r"(?m)^\s*(?:forced_chatgpt_account_id|forced_login_method|cli_auth_credentials_store)\s*=.*\n?").map_err(|_| AccountSessionError::Persistence)?;
            let config = restrictions.replace_all(&config, "");
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(target) {
                Ok(mut file) => {
                    file.write_all(config.as_bytes())
                        .and_then(|()| file.sync_all())
                        .map_err(|_| AccountSessionError::Persistence)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(AccountSessionError::Persistence),
            }
        }
        #[cfg(unix)]
        for name in ["skills", "plugins", "rules", "AGENTS.md"] {
            let source = self.base_home.join(name);
            let target = home.join(name);
            if source.exists() {
                match std::os::unix::fs::symlink(source, target) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => return Err(AccountSessionError::Persistence),
                }
            }
        }
        Ok(home)
    }
}
impl AccountSessionPort for AccountSessionRuntime {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        self.session.as_ref()?.current_scope(account)
    }

    fn ensure_session<'a>(&'a self, account: &'a str) -> AccountSessionFuture<'a, ConnectionScope> {
        Box::pin(async move {
            let session = self.session.as_ref().ok_or(AccountSessionError::Requires(
                "provider-session/account-session",
            ))?;
            let scope = session.ensure_session(account.to_owned()).await?;
            if scope.account != account {
                return Err(AccountSessionError::Refused(
                    "Provider session account mismatch",
                ));
            }
            Ok(scope)
        })
    }

    fn claude_control_session(
        &self,
        account: &str,
    ) -> Result<Arc<dyn crate::claude_controls::service::ControlSession>, AccountSessionError> {
        self.session
            .as_ref()
            .ok_or(AccountSessionError::Requires(
                "provider-session/claude-controls",
            ))?
            .claude_control_session(account)
    }

    fn recover_connection<'a>(
        &'a self,
        account: &'a str,
        agent_id: &'a str,
    ) -> AccountSessionFuture<'a, Value> {
        Box::pin(async move {
            let session = self.session.as_ref().ok_or(AccountSessionError::Requires(
                "account-worker/recovery-history",
            ))?;
            let Some(expected) = session.current_scope(account) else {
                return Ok(serde_json::json!({"status":"superseded"}));
            };
            let result = session
                .recover_connection(account.to_owned(), agent_id.to_owned())
                .await?;
            if session.current_scope(account).as_ref() == Some(&expected) {
                Ok(result)
            } else {
                Err(AccountSessionError::Unknown)
            }
        })
    }

    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
            let session = self.session.as_ref().ok_or(AccountSessionError::Requires(
                "provider-session/native-request",
            ))?;
            let deadline = request.deadline;
            let operation = request.operation.clone();
            let account = request.account.clone();
            let expected_scope = request
                .expected_scope
                .clone()
                .or_else(|| session.current_scope(&account));
            if expected_scope
                .as_ref()
                .is_some_and(|scope| scope.account != account)
            {
                return Err(AccountSessionError::Refused(
                    "Provider scope does not belong to the account",
                ));
            }
            if expected_scope
                .as_ref()
                .is_some_and(|scope| session.current_scope(&account).as_ref() != Some(scope))
            {
                // A provider connection is process-local. Python settles work tied
                // to a disconnected connection as interrupted/unknown instead of
                // handing it to a replacement connection.
                return Err(AccountSessionError::Unknown);
            }
            let mut request = request;
            request.expected_scope = expected_scope.clone();
            match {
                tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    session.request(request),
                )
                .await
            } {
                Ok(Ok(receipt))
                    if provider_receipt_matches(
                        &receipt,
                        &account,
                        &operation,
                        expected_scope.as_ref(),
                    )
                    .is_ok() =>
                {
                    Ok(receipt)
                }
                Ok(Err(error)) => Err(error),
                _ => Err(AccountSessionError::Unknown),
            }
        })
    }

    fn deliver_vm_notification<'a>(
        &'a self,
        account: &'a str,
        agent_id: &'a str,
        epoch: u64,
        sequence: u64,
        frame: studio_runtime_providers::Frame,
    ) -> AccountSessionFuture<'a, ()> {
        Box::pin(async move {
            let session = self.session.as_ref().ok_or(AccountSessionError::Requires(
                "account-worker/vm-notification",
            ))?;
            let scope = session
                .current_scope(account)
                .ok_or(AccountSessionError::Unknown)?;
            session
                .deliver_vm_notification(
                    account.to_owned(),
                    agent_id.to_owned(),
                    epoch,
                    sequence,
                    frame,
                )
                .await?;
            {
                if session.current_scope(account).as_ref() == Some(&scope) {
                    Ok(())
                } else {
                    Err(AccountSessionError::Unknown)
                }
            }
        })
    }
    fn transfer_provider_request<'a>(
        &'a self,
        request: TransferProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
            let session = self.session.as_ref().ok_or(AccountSessionError::Requires(
                "account-transfer/source-session-preflight",
            ))?;
            let deadline = request.deadline;
            let operation = request.operation.clone();
            let account = request.source_account.clone();
            let scope = request.expected_scope.clone();
            match {
                tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    session.transfer_request(request),
                )
                .await
            } {
                Ok(Ok(receipt))
                    if receipt.operation == operation
                        && receipt.scope.account == account
                        && receipt.scope == scope =>
                {
                    Ok(receipt)
                }
                Ok(Err(error)) => Err(error),
                _ => Err(AccountSessionError::Unknown),
            }
        })
    }
    fn callback_barrier<'a>(
        &'a self,
        account: &'a str,
        expected_scope: &'a ConnectionScope,
        deadline: std::time::Instant,
    ) -> AccountSessionFuture<'a, ()> {
        Box::pin(async move {
            let session = self.session.as_ref().ok_or(AccountSessionError::Requires(
                "provider-session/callback-barrier",
            ))?;
            let wait = {
                tokio::time::timeout_at(
                    tokio::time::Instant::from_std(deadline),
                    session.callback_barrier(account.to_owned(), expected_scope.clone(), deadline),
                )
                .await
            };
            match wait {
                Ok(result) => result,
                Err(_) => Err(AccountSessionError::Unknown),
            }
        })
    }
    fn read_limits<'a>(&'a self, account: &'a str) -> AccountSessionFuture<'a, Value> {
        Box::pin(read_limits_with_freshness(self, account, false))
    }
    fn read_limits_if_stale<'a>(&'a self, account: &'a str) -> AccountSessionFuture<'a, Value> {
        Box::pin(read_limits_with_freshness(self, account, true))
    }
    fn persist_limits<'a>(
        &'a self,
        account: &'a str,
        value: &'a Value,
    ) -> AccountSessionFuture<'a, ()> {
        Box::pin(async move { self.persist_limits(account, value).await })
    }
    fn limits_snapshot(&self, account: &str) -> Result<Value, AccountSessionError> {
        if let Some(value) = self
            .refreshed_limits
            .lock()
            .map_err(|_| AccountSessionError::Persistence)?
            .get(account)
        {
            return Ok(value.clone());
        }
        self.limits
            .as_ref()
            .map(|policy| policy.limits(account))
            .ok_or(AccountSessionError::Requires(LIMITS_REQUIRED))
    }
    fn refresh_limits<'a>(&'a self, account: &'a str) -> AccountSessionFuture<'a, ()> {
        Box::pin(async move {
            let account = account.to_owned();
            {
                let mut pending = self
                    .pending_limit_refreshes
                    .lock()
                    .map_err(|_| AccountSessionError::Persistence)?;
                if !pending.insert(account.clone()) {
                    return Ok(());
                }
            }
            let runtime = self.clone();
            let pending = Arc::clone(&self.pending_limit_refreshes);
            let handle = runtime.runtime.clone();
            handle.spawn(async move {
                match AccountSessionPort::read_limits_if_stale(&runtime, &account).await {
                    Ok(_) => {}
                    Err(_) => {
                        // A refresh is best-effort; this request keeps its existing
                        // cached limits and the next read may retry after pending clears.
                    }
                }
                if let Ok(mut pending) = pending.lock() {
                    pending.remove(&account);
                }
            });
            Ok(())
        })
    }
    fn login<'a>(&'a self, request: LoginRequest) -> AccountSessionFuture<'a, Value> {
        let runtime = self.clone();
        Box::pin(async move {
            let task_runtime = runtime.clone();
            runtime
                .run_durable_operation(async move {
                    let runtime = task_runtime;
                    let mut request = request;
                    request.login_id = uuid::Uuid::parse_str(&request.login_id)
                        .map_err(|_| AccountSessionError::Refused("Supply a UUID request_id"))?
                        .to_string();
                    let driver = Arc::clone(&runtime.driver);
                    let request_for_receipt = request.clone();
                    if let Some(receipt) = spawn_account_driver(move || {
                        driver.codex_login_receipt(&request_for_receipt)
                    })
                    .await?
                    .map_err(mutation_error)?
                    {
                        let driver = Arc::clone(&runtime.driver);
                        let request_id = request.login_id.clone();
                        return spawn_account_driver(move || {
                            driver.reconcile_codex_login(&request_id)
                        })
                        .await?
                        .map(|reconciled| public_login(reconciled.unwrap_or(receipt)))
                        .map_err(mutation_error);
                    }
                    if runtime.session.is_none() {
                        return Err(AccountSessionError::Requires(
                            "provider-session/codex-login",
                        ));
                    }
                    let home = if request.account_key.is_none() {
                        let runtime = runtime.clone();
                        let request_id = request.login_id.clone();
                        spawn_account_driver(move || runtime.prepare_login_home(&request_id))
                            .await??
                    } else {
                        runtime.base_home.clone()
                    };
                    let driver = Arc::clone(&runtime.driver);
                    let request_for_reservation = request.clone();
                    let now = timestamp()?;
                    let reservation = spawn_account_driver(move || {
                        driver.reserve_codex_login(&request_for_reservation, &home, now)
                    })
                    .await?
                    .map_err(mutation_error)?;
                    let receipt = match reservation {
                        LoginReservation::Replay(receipt) => return Ok(public_login(receipt)),
                        LoginReservation::Dispatch(receipt) => receipt,
                    };
                    let account = receipt["accountKey"].as_str().ok_or_else(|| {
                        AccountSessionError::InvalidResponse("/accountKey".into())
                    })?;
                    let response = runtime
                        .exchange(
                            account,
                            request.account_key.is_some(),
                            request.login_id.clone(),
                            "account/login/start",
                            serde_json::json!({"type":"chatgptDeviceCode"}),
                            LOGIN_TIMEOUT,
                        )
                        .await;
                    let (result, sent) = match response {
                        Ok(response) => (
                            if response.certainty == RequestCertainty::Written {
                                match response.result {
                                    Some(ProviderResponse::Reply(value)) => Some(value),
                                    Some(ProviderResponse::Rejected(_)) | None => None,
                                }
                            } else {
                                None
                            },
                            response.certainty != RequestCertainty::NotSent,
                        ),
                        // The durable login reservation already exists. Any adapter error
                        // after this point lacks proof that the provider never received
                        // the request, so keep the attempt uncertain.
                        Err(_) => (None, true),
                    };
                    let driver = Arc::clone(&runtime.driver);
                    let request_id = request.login_id.clone();
                    let account = account.to_owned();
                    let finished_at = timestamp()?;
                    spawn_account_driver(move || {
                        driver.finish_codex_login(
                            &request_id,
                            &account,
                            result.as_ref(),
                            sent,
                            finished_at,
                        )
                    })
                    .await?
                    .map(public_login)
                    .map_err(mutation_error)
                })
                .await
        })
    }
    fn cancel_login<'a>(&'a self, request: &'a str) -> AccountSessionFuture<'a, Value> {
        let runtime = self.clone();
        let request = request.to_owned();
        Box::pin(async move {
            let task_runtime = runtime.clone();
            runtime
                .run_durable_operation(async move {
                    let runtime = task_runtime;
                    let driver = Arc::clone(&runtime.driver);
                    let request_id = request.to_owned();
                    if let Some(receipt) =
                        spawn_account_driver(move || driver.reconcile_codex_login(&request_id))
                            .await?
                            .map_err(mutation_error)?
                    {
                        return Ok(public_login(receipt));
                    }
                    if runtime.session.is_none() {
                        let driver = Arc::clone(&runtime.driver);
                        let receipt = spawn_account_driver(move || driver.snapshot())
                            .await?
                            .map_err(account_error)?
                            .logins
                            .into_iter()
                            .find(|receipt| receipt["requestId"] == request)
                            .ok_or(AccountSessionError::Refused("Unknown sign-in request"))?;
                        if matches!(
                            receipt["status"].as_str(),
                            Some("ready" | "duplicate" | "cancelled" | "error")
                        ) || (receipt["status"] == "uncertain"
                            && receipt.get("loginId").is_some_and(|id| !id.is_null()))
                        {
                            return Ok(public_login(receipt));
                        }
                        if receipt.get("loginId").is_none_or(Value::is_null) {
                            return Err(account_error(AccountDriverError::LoginResponseUnknown));
                        }
                        return Err(AccountSessionError::Requires(
                            "provider-session/codex-login",
                        ));
                    }
                    let driver = Arc::clone(&runtime.driver);
                    let request_id = request.to_owned();
                    let reservation =
                        spawn_account_driver(move || driver.reserve_codex_cancel(&request_id))
                            .await?
                            .map_err(mutation_error)?;
                    let receipt = match reservation {
                        LoginReservation::Replay(receipt) => return Ok(public_login(receipt)),
                        LoginReservation::Dispatch(receipt) => receipt,
                    };
                    let account = receipt["accountKey"].as_str().ok_or_else(|| {
                        AccountSessionError::InvalidResponse("/accountKey".into())
                    })?;
                    let response = runtime
                        .exchange(
                            account,
                            receipt.get("reauthAccountKey").is_some(),
                            format!("{request}:cancel"),
                            "account/login/cancel",
                            serde_json::json!({"loginId":receipt["loginId"]}),
                            CANCEL_TIMEOUT,
                        )
                        .await;
                    if response
                        .as_ref()
                        .is_ok_and(|receipt| receipt.certainty == RequestCertainty::NotSent)
                    {
                        let driver = Arc::clone(&runtime.driver);
                        let request_id = request.to_owned();
                        let account = account.to_owned();
                        return spawn_account_driver(move || {
                            driver.finish_codex_cancel_not_sent(&request_id, &account)
                        })
                        .await?
                        .map(public_login)
                        .map_err(mutation_error);
                    }
                    let confirmed = response.ok().is_some_and(|receipt| {
                        receipt.certainty == RequestCertainty::Written
                            && matches!(
                                receipt.result.as_ref().and_then(|result| match result {
                                    ProviderResponse::Reply(value) => value["status"].as_str(),
                                    ProviderResponse::Rejected(_) => None,
                                }),
                                Some("canceled" | "notFound")
                            )
                    });
                    let driver = Arc::clone(&runtime.driver);
                    let request_id = request.to_owned();
                    let account = account.to_owned();
                    spawn_account_driver(move || {
                        driver.finish_codex_cancel(&request_id, &account, confirmed)
                    })
                    .await?
                    .map(public_login)
                    .map_err(mutation_error)
                })
                .await
        })
    }
    fn shutdown_durable_operations<'a>(&'a self) -> AccountSessionFuture<'a, ()> {
        Box::pin(async move {
            self.drain_durable_operations().await?;
            Ok(())
        })
    }
    fn discover<'a>(&'a self) -> AccountSessionFuture<'a, Value> {
        Box::pin(async move {
            let runtime = self.clone();
            spawn_account_driver(move || runtime.discover_sync()).await?
        })
    }
    fn profile<'a>(&'a self, request: ProfileRequest) -> AccountSessionFuture<'a, Value> {
        Box::pin(async move {
            let runtime = self.clone();
            spawn_account_driver(move || runtime.profile_sync(request)).await?
        })
    }
}

async fn read_limits_with_freshness(
    runtime: &AccountSessionRuntime,
    account: &str,
    skip_if_fresh: bool,
) -> Result<Value, AccountSessionError> {
    let driver = Arc::clone(&runtime.driver);
    let snapshot = spawn_account_driver(move || driver.snapshot())
        .await?
        .map_err(account_error)?;
    if !snapshot
        .accounts
        .iter()
        .chain(&snapshot.archived_accounts)
        .any(|row| row["id"] == account)
    {
        return Err(AccountSessionError::Refused("Unknown Codex account"));
    }
    let read_lock = runtime.limits_read_lock(account)?;
    let _read_guard = read_lock.lock().await;
    let before = runtime.limits_snapshot(account)?;
    let before = LimitsSnapshot::read(account, before)?;
    if skip_if_fresh && before.is_fresh_for_provider(timestamp()?) {
        return Ok(before.into_value());
    }
    let before_value = before.into_value();
    let provider_read = async {
        let scope = AccountSessionPort::ensure_session(runtime, account).await?;
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let operation = format!(
            "limits-read:{account}:{}:{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| AccountSessionError::Persistence)?
                .as_nanos(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let receipt = AccountSessionPort::provider_request(
            runtime,
            ProviderRequest {
                account: account.to_owned(),
                operation: operation.clone(),
                actor: None,
                expected_scope: Some(scope.clone()),
                deleted_actor_session: None,
                method: Method::AccountRateLimitsRead,
                params: serde_json::json!({}),
                deadline: std::time::Instant::now() + Duration::from_secs(10),
            },
        )
        .await?;
        if receipt.scope != scope
            || receipt.operation != operation
            || receipt.certainty != RequestCertainty::Written
            || receipt.ticket.is_none()
        {
            return Err(AccountSessionError::Unknown);
        }
        match receipt.result {
            Some(ProviderResponse::Reply(data)) => Ok(data),
            Some(ProviderResponse::Rejected(_)) | None => {
                Err(AccountSessionError::Refused("Account limits read failed"))
            }
        }
    }
    .await;
    let data = match provider_read {
        Ok(data) => data,
        Err(error) => {
            let current = runtime.limits_snapshot(account)?;
            if current != before_value
                && current.get("error").is_none_or(Value::is_null)
                && current.get("data").is_some_and(Value::is_object)
            {
                return Ok(current);
            }
            let mut failed = current;
            failed["error"] = Value::String(error.to_string());
            failed["checkedAt"] = serde_json::json!(timestamp()?);
            let failed = LimitsSnapshot::read(account, failed)?.into_value();
            runtime.persist_limits(account, &failed).await?;
            return Err(error);
        }
    };
    let now = timestamp()?;
    let current = LimitsSnapshot::read(account, runtime.limits_snapshot(account)?)?.into_value();
    let mut value = serde_json::json!({
        "accountKey":account,
        "data":data,
        "at":now,
        "readAt":now,
        "error":null
    });
    if current != before_value
        && current.get("error").is_none_or(Value::is_null)
        && current.get("data").is_some_and(|data| !data.is_null())
    {
        let Some(current_data) = current.get("data").and_then(Value::as_object) else {
            return Ok(current.clone());
        };
        let Some(read_data) = value.get_mut("data").and_then(Value::as_object_mut) else {
            return Ok(current.clone());
        };
        if let Some(rate_limits) = current_data.get("rateLimits")
            && !rate_limits.is_null()
        {
            read_data.insert("rateLimits".into(), rate_limits.clone());
        }
        if let Some(live_by_id) = current_data
            .get("rateLimitsByLimitId")
            .and_then(Value::as_object)
        {
            let read_by_id = read_data
                .entry("rateLimitsByLimitId")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(read_by_id) = read_by_id.as_object_mut() {
                read_by_id.extend(live_by_id.clone());
            }
        }
    }
    let value = LimitsSnapshot::read(account, value)?.into_value();
    runtime.persist_limits(account, &value).await?;
    Ok(value)
}

fn timestamp() -> Result<f64, AccountSessionError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .map_err(|_| AccountSessionError::Persistence)
}
fn account_error(error: AccountDriverError) -> AccountSessionError {
    match error {
        AccountDriverError::UnknownAccount => AccountSessionError::Refused("Unknown account"),
        AccountDriverError::UnknownLogin => AccountSessionError::Refused("Unknown sign-in request"),
        AccountDriverError::LoginResponseUnknown => AccountSessionError::Refused(
            "The sign-in response is still unknown. Check its status first.",
        ),
        AccountDriverError::LoginCancellationUnavailable => {
            AccountSessionError::Requires("provider-session/codex-login")
        }
        _ => AccountSessionError::Persistence,
    }
}
fn mutation_error(error: AccountMutationError) -> AccountSessionError {
    match error {
        AccountMutationError::Driver(error) => account_error(error),
        AccountMutationError::Refused(message) => AccountSessionError::Refused(message),
    }
}
fn public_login(mut receipt: Value) -> Value {
    if let Some(object) = receipt.as_object_mut() {
        object.retain(|key, _| {
            matches!(
                key.as_str(),
                "requestId"
                    | "accountKey"
                    | "status"
                    | "loginId"
                    | "verificationUrl"
                    | "userCode"
                    | "error"
                    | "resolvedAccountKey"
                    | "email"
                    | "createdAt"
                    | "expiresAt"
            )
        });
    }
    receipt
}
pub fn normalize_profile(
    mut options: serde_json::Map<String, Value>,
    home: &Path,
) -> Result<serde_json::Map<String, Value>, AccountSessionError> {
    let invalid = || AccountSessionError::Refused("Invalid Claude profile options");
    if options.keys().any(|key| {
        !matches!(
            key.as_str(),
            "binaryPath" | "configDir" | "customModels" | "autoCompactWindow" | "launchArgs"
        )
    }) {
        return Err(invalid());
    }
    for field in ["configDir", "binaryPath"] {
        if let Some(value) = options.remove(field) {
            let value = value.as_str().ok_or_else(invalid)?.trim();
            if value.contains('\0') {
                return Err(invalid());
            }
            if !value.is_empty() {
                let value = if field == "configDir" || value.contains('/') {
                    let path = if value == "~" {
                        home.to_path_buf()
                    } else if let Some(value) = value.strip_prefix("~/") {
                        home.join(value)
                    } else {
                        PathBuf::from(value)
                    };
                    let path = std::path::absolute(path).map_err(|_| invalid())?;
                    crate::projects::resolve_nonstrict(&path)
                        .map_err(|_| invalid())?
                        .to_string_lossy()
                        .into_owned()
                } else {
                    value.into()
                };
                options.insert(field.into(), Value::String(value));
            }
        }
    }
    let models = options
        .entry("customModels")
        .or_insert_with(|| serde_json::json!([]))
        .as_array_mut()
        .ok_or_else(invalid)?;
    if models.len() > 100 {
        return Err(invalid());
    }
    let identifier =
        regex::Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:/\[\]-]{0,199}$").map_err(|_| invalid())?;
    let mut seen = std::collections::BTreeSet::new();
    for model in models {
        if let Some(id) = model.as_str() {
            *model = serde_json::json!({"id":id,"label":id});
        }
        let object = model.as_object_mut().ok_or_else(invalid)?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "id" | "label"))
        {
            return Err(invalid());
        }
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?
            .to_owned();
        if !identifier.is_match(&id) || !seen.insert(id.clone()) {
            return Err(invalid());
        }
        let label = match object.get("label") {
            None | Some(Value::Null) => id.as_str(),
            Some(Value::String(label)) if label.is_empty() => id.as_str(),
            Some(Value::String(label)) => label.as_str(),
            _ => return Err(invalid()),
        };
        if label.trim().is_empty() || label.chars().count() > 200 {
            return Err(invalid());
        }
        let label = label.trim().to_owned();
        object.insert("label".into(), Value::String(label));
    }
    if let Some(window) = options.remove("autoCompactWindow")
        && !window.is_null()
        && window != ""
    {
        let text = window
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| window.to_string());
        if !text.bytes().all(|value| value.is_ascii_digit()) {
            return Err(invalid());
        }
        let value: u64 = text.parse().map_err(|_| invalid())?;
        if !(100_000..=1_000_000).contains(&value) {
            return Err(invalid());
        }
        options.insert("autoCompactWindow".into(), serde_json::json!(value));
    }
    let args = match options.get("launchArgs") {
        None => "",
        Some(Value::String(args)) if args.chars().count() <= 4096 => args.as_str(),
        _ => return Err(invalid()),
    };
    let tokens = shlex::split(args).ok_or_else(invalid)?;
    let args = shlex::try_join(tokens.iter().map(String::as_str)).map_err(|_| invalid())?;
    options.insert("launchArgs".into(), Value::String(args));
    let value = Value::Object(options.clone());
    if !studio_runtime_schema::validate("account-registry/ClaudeOptionsRecord", &value)
        .map_err(|_| invalid())?
    {
        return Err(invalid());
    }
    crate::claude_bridge::bridge_options(&value).map_err(|_| invalid())?;
    Ok(options)
}

#[cfg(test)]
mod historical_receipt_tests {
    use super::{
        AccountSessionError, ConnectionScope, ProviderResponse, RequestCertainty, SessionReceipt,
        provider_receipt_matches,
    };
    use serde_json::json;

    fn scope(generation: u64) -> ConnectionScope {
        ConnectionScope {
            account: "default".into(),
            connection: generation.to_string(),
            generation,
        }
    }

    fn receipt(
        scope: ConnectionScope,
        certainty: RequestCertainty,
        replayed: bool,
    ) -> SessionReceipt {
        SessionReceipt {
            scope,
            operation: "branch:request-1".into(),
            ticket: None,
            certainty,
            result: (certainty == RequestCertainty::Written)
                .then(|| ProviderResponse::Reply(json!({"thread":{"id":"branch-thread"}}))),
            not_sent_reason: None,
            replayed,
        }
    }

    #[test]
    fn accepts_only_checkpoint_verified_historical_receipts_for_same_identity() {
        let old_scope = scope(1);
        let current_scope = scope(2);
        let replayed = receipt(old_scope.clone(), RequestCertainty::Written, true);

        assert!(
            provider_receipt_matches(
                &replayed,
                "default",
                "branch:request-1",
                Some(&current_scope),
            )
            .is_ok()
        );

        let fresh_request = receipt(old_scope.clone(), RequestCertainty::Written, false);
        assert!(matches!(
            provider_receipt_matches(
                &fresh_request,
                "default",
                "branch:request-1",
                Some(&current_scope),
            ),
            Err(AccountSessionError::Unknown)
        ));

        let mut different_identity = receipt(old_scope.clone(), RequestCertainty::Written, true);
        different_identity.operation = "branch:request-2".into();
        assert!(matches!(
            provider_receipt_matches(
                &different_identity,
                "default",
                "branch:request-1",
                Some(&current_scope),
            ),
            Err(AccountSessionError::Unknown)
        ));

        let beyond_checkpoint = receipt(old_scope, RequestCertainty::Unknown, true);
        assert!(matches!(
            provider_receipt_matches(
                &beyond_checkpoint,
                "default",
                "branch:request-1",
                Some(&current_scope),
            ),
            Err(AccountSessionError::Unknown)
        ));
    }
}

#[cfg(test)]
mod persistence_tests {
    #![allow(clippy::unwrap_used)]
    use super::{AccountSessionError, persist_limits};
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };
    use studio_runtime_store::{RuntimeStore, SyncDocumentsRepository};

    fn stored_workspace(store: &RuntimeStore) -> Value {
        let db = store.read().unwrap();
        let raw: String = db
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[tokio::test]
    async fn refreshed_limits_persist_and_preserve_other_accounts() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
        store
            .transaction(|tx| {
                SyncDocumentsRepository::put_entity(
                    tx,
                    "workspace",
                    "current",
                    json!({"rateLimitsByAccount":{"other":{"accountKey":"other"}}}),
                    false,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let cache = Arc::new(Mutex::new(BTreeMap::new()));
        let value = json!({"accountKey":"default","data":{"accountId":"fixture"},"at":1.0,"readAt":1.0,"error":null});

        persist_limits(
            Arc::clone(&store),
            Arc::clone(&cache),
            "default".into(),
            value.clone(),
            None,
        )
        .await
        .unwrap();

        let entity = stored_workspace(&store);
        assert_eq!(entity["value"]["rateLimits"], value);
        assert_eq!(entity["value"]["rateLimitsByAccount"]["default"], value);
        assert_eq!(
            entity["value"]["rateLimitsByAccount"]["other"]["accountKey"],
            "other"
        );
        assert_eq!(cache.lock().unwrap().get("default"), Some(&value));
    }

    #[tokio::test]
    async fn limits_persistence_failure_leaves_no_partial_write_or_cache_update() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
        store
            .transaction(|tx| {
                SyncDocumentsRepository::put_entity(
                    tx,
                    "workspace",
                    "current",
                    json!({"rateLimitsByAccount":"invalid-shape","untouched":true}),
                    false,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let before = stored_workspace(&store);
        let cache = Arc::new(Mutex::new(BTreeMap::new()));
        let value = json!({"accountKey":"default","data":{"accountId":"fixture"},"at":1.0,"readAt":1.0,"error":null});

        assert!(matches!(
            persist_limits(
                Arc::clone(&store),
                Arc::clone(&cache),
                "default".into(),
                value.clone(),
                None
            )
            .await,
            Err(AccountSessionError::Persistence)
        ));

        assert_eq!(stored_workspace(&store), before);
        assert!(cache.lock().unwrap().is_empty());
    }
}

#[cfg(test)]
mod limits_runtime_tests {
    #![allow(clippy::unwrap_used)]

    use super::{
        AccountSessionError, AccountSessionPort, AccountSessionRuntime, ConnectionScope,
        LoginRequest, ProviderRequest, ProviderResponse, RequestCertainty, RetainedAccountSession,
        SessionFuture, SessionFutureScope, SessionReceipt,
    };
    use crate::{account_driver::AccountDriver, account_policy::AccountPolicyState};
    use serde_json::{Value, json};
    use std::{
        collections::VecDeque,
        fs,
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::Duration,
    };
    use studio_runtime_providers::{RequestId, RequestTicket};
    use studio_runtime_store::RuntimeStore;

    enum Reply {
        Data(Value),
        Rejected,
    }

    struct FirstReadGate {
        released: Mutex<bool>,
        wake: Condvar,
        entered: mpsc::Sender<()>,
    }

    impl FirstReadGate {
        fn release(&self) {
            *self.released.lock().unwrap() = true;
            self.wake.notify_all();
        }
    }

    struct ReleaseFirstReadGate(Arc<FirstReadGate>);

    impl Drop for ReleaseFirstReadGate {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    struct FakeLimitsSession {
        calls: AtomicUsize,
        replies: Mutex<VecDeque<Reply>>,
        scope: ConnectionScope,
        first_read_gate: Option<Arc<FirstReadGate>>,
    }

    impl FakeLimitsSession {
        fn response(&self, request: ProviderRequest, call: usize) -> SessionFuture {
            let scope = self.scope.clone();
            let operation = request.operation.clone();
            let method = request.method;
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Reply::Data(json!({"accountId":"fixture-account"})));
            let gate = (call == 1).then(|| self.first_read_gate.clone()).flatten();
            Box::pin(async move {
                if let Some(gate) = gate {
                    gate.entered
                        .send(())
                        .expect("the cold-read test receiver remains alive until entry");
                    let mut released = gate.released.lock().unwrap();
                    while !*released {
                        released = gate.wake.wait(released).unwrap();
                    }
                }
                let result = match reply {
                    Reply::Data(data) => ProviderResponse::Reply(data),
                    Reply::Rejected => ProviderResponse::Rejected(json!({"message":"fixture"})),
                };
                Ok(SessionReceipt {
                    ticket: Some(RequestTicket {
                        generation: scope.generation,
                        id: RequestId::String(operation.clone()),
                        method,
                    }),
                    scope,
                    operation,
                    certainty: RequestCertainty::Written,
                    result: Some(result),
                    not_sent_reason: None,
                    replayed: false,
                })
            })
        }
    }

    impl RetainedAccountSession for FakeLimitsSession {
        fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
            (account == self.scope.account).then(|| self.scope.clone())
        }

        fn ensure_session(&self, account: String) -> SessionFutureScope {
            let result = if account == self.scope.account {
                Ok(self.scope.clone())
            } else {
                Err(AccountSessionError::Refused("Unknown Codex account"))
            };
            Box::pin(async move { result })
        }

        fn request(&self, request: ProviderRequest) -> SessionFuture {
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            self.response(request, call)
        }
    }

    struct Fixture {
        _root: tempfile::TempDir,
        runtime: Arc<AccountSessionRuntime>,
        provider: Arc<FakeLimitsSession>,
    }

    async fn fixture(
        replies: impl IntoIterator<Item = Reply>,
        gate: bool,
    ) -> (Fixture, mpsc::Receiver<()>) {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("codex-home");
        fs::create_dir_all(&home).unwrap();
        let registry_path = root.path().join("accounts/registry.json");
        fs::create_dir_all(registry_path.parent().unwrap()).unwrap();
        let registry = serde_json::to_vec(&json!({
            "version":1,
            "defaultAccountKey":"default",
            "accounts":{"default":{
                "id":"default","home":home,"label":"Codex","source":"Codex CLI","accountId":"fixture-account","status":"signedOut"
            }},
            "logins":{},"deleteReceipts":{}
        }))
        .unwrap();
        fs::write(&registry_path, &registry).unwrap();
        let driver = Arc::new(AccountDriver::open(&registry_path).unwrap());
        let policy = Arc::new(AccountPolicyState::from_startup(Some(&registry), []).unwrap());
        let scope = ConnectionScope {
            account: "default".into(),
            connection: "fixture-connection".into(),
            generation: 1,
        };
        let (entered, entered_rx) = mpsc::channel();
        let first_read_gate = gate.then(|| {
            Arc::new(FirstReadGate {
                released: Mutex::new(false),
                wake: Condvar::new(),
                entered,
            })
        });
        let provider = Arc::new(FakeLimitsSession {
            calls: AtomicUsize::new(0),
            replies: Mutex::new(replies.into_iter().collect()),
            scope,
            first_read_gate,
        });
        let store_path = root.path().join("store");
        fs::create_dir_all(&store_path).unwrap();
        let store = Arc::new(RuntimeStore::open(Some(&store_path)).await.unwrap());
        let runtime = Arc::new(
            AccountSessionRuntime::new(
                driver,
                Some(provider.clone()),
                tokio::runtime::Handle::current(),
                root.path().join("accounts"),
                home.clone(),
                home,
            )
            .with_limits(policy)
            .with_store(store),
        );
        (
            Fixture {
                _root: root,
                runtime,
                provider,
            },
            entered_rx,
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_stale_cold_reads_share_one_native_request() {
        let (fixture, entered) =
            fixture([Reply::Data(json!({"accountId":"fixture-account"}))], true).await;
        let gate = Arc::clone(fixture.provider.first_read_gate.as_ref().unwrap());
        let _release_on_exit = ReleaseFirstReadGate(Arc::clone(&gate));
        let runtime = Arc::clone(&fixture.runtime);
        let first = tokio::spawn(async move {
            AccountSessionPort::read_limits_if_stale(runtime.as_ref(), "default").await
        });
        let entered_result = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/account_session_runtime",
            move || entered.recv_timeout(Duration::from_secs(2)),
        )
        .await;

        let (started, started_rx) = mpsc::channel();
        let runtime = Arc::clone(&fixture.runtime);
        let second = tokio::spawn(async move {
            started
                .send(())
                .expect("the concurrency test receiver remains alive until the second read starts");
            AccountSessionPort::read_limits_if_stale(runtime.as_ref(), "default").await
        });
        let second_started = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/account_session_runtime",
            move || started_rx.recv_timeout(Duration::from_secs(2)),
        )
        .await;
        gate.release();

        let first_result = first.await.unwrap();
        let second_result = second.await.unwrap();
        assert!(entered_result.unwrap().is_ok());
        assert!(second_started.unwrap().is_ok());
        first_result.unwrap();
        second_result.unwrap();
        assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_provider_read_is_persisted_and_cooled_down_but_forced_read_runs() {
        let (fixture, _) = fixture(
            [
                Reply::Rejected,
                Reply::Data(json!({"accountId":"fixture-account"})),
            ],
            false,
        )
        .await;
        assert!(
            AccountSessionPort::read_limits_if_stale(fixture.runtime.as_ref(), "default")
                .await
                .is_err()
        );
        let failed =
            AccountSessionPort::limits_snapshot(fixture.runtime.as_ref(), "default").unwrap();
        assert_eq!(failed["error"], "Account limits read failed");
        assert!(failed["checkedAt"].as_f64().unwrap() > 0.0);

        let cooled = AccountSessionPort::read_limits_if_stale(fixture.runtime.as_ref(), "default")
            .await
            .unwrap();
        assert_eq!(cooled["error"], "Account limits read failed");
        assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 1);

        let forced = AccountSessionPort::read_limits(fixture.runtime.as_ref(), "default")
            .await
            .unwrap();
        assert_eq!(forced["data"]["accountId"], "fixture-account");
        assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn caller_cancellation_does_not_abandon_reserved_login_or_resend_it() {
        let (fixture, entered) = fixture(
            [Reply::Data(json!({
                "type":"chatgptDeviceCode",
                "loginId":"provider-login",
                "verificationUrl":"https://auth.openai.com/codex/device",
                "userCode":"ABCD"
            }))],
            true,
        )
        .await;
        let request = LoginRequest {
            login_id: "11000000-0000-4000-8000-000000000009".into(),
            account_key: Some("default".into()),
            email: None,
            label: None,
        };
        let runtime = Arc::clone(&fixture.runtime);
        let caller =
            tokio::spawn(async move { AccountSessionPort::login(runtime.as_ref(), request).await });
        let entered = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/account_session_runtime",
            move || entered.recv_timeout(Duration::from_secs(2)),
        )
        .await
        .unwrap();
        assert!(
            entered.is_ok(),
            "login reached its retained provider request"
        );

        caller.abort();
        assert!(
            caller
                .await
                .expect_err("caller task was aborted")
                .is_cancelled()
        );
        fixture
            .provider
            .first_read_gate
            .as_ref()
            .expect("test holds the login provider response")
            .release();

        // Join the retained operation before replaying its receipt. The provider
        // gate only controls the response handoff; it does not imply that the
        // durable receipt update has completed.
        let joined = {
            let mut owner = fixture.runtime.durable_operations.lock().await;
            tokio::time::timeout(
                Duration::from_secs(2),
                owner
                    .as_mut()
                    .expect("durable operation owner remains open")
                    .join_next(),
            )
            .await
            .expect("retained login finishes after provider response is released")
        };
        assert!(matches!(joined, Some(Ok(()))));

        let replay = AccountSessionPort::login(
            fixture.runtime.as_ref(),
            LoginRequest {
                login_id: "11000000-0000-4000-8000-000000000009".into(),
                account_key: Some("default".into()),
                email: None,
                label: None,
            },
        )
        .await
        .expect("a duplicate request id replays its durable receipt");
        assert_eq!(replay["status"], "pending");
        AccountSessionPort::shutdown_durable_operations(fixture.runtime.as_ref())
            .await
            .expect("shutdown joins the durable login task");

        let snapshot = fixture.runtime.driver.snapshot().unwrap();
        assert_eq!(snapshot.logins.len(), 1);
        assert_eq!(snapshot.logins[0]["status"], "pending");
        assert_eq!(snapshot.logins[0]["loginId"], "provider-login");
        assert_eq!(fixture.provider.calls.load(Ordering::SeqCst), 1);
    }
}
