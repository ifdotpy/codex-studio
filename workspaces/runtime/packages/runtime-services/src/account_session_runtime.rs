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
use thiserror::Error;

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
    fn ensure_session(&self, _account: &str) -> Result<ConnectionScope, AccountSessionError> {
        Err(AccountSessionError::Requires(
            "provider-session/account-session",
        ))
    }
    /// Async callers should use this method so retained session owners can be
    /// awaited directly instead of crossing a blocking-pool/runtime bridge.
    fn ensure_session_async<'a>(
        &'a self,
        account: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ConnectionScope, AccountSessionError>> + Send + 'a>>
    {
        Box::pin(async move { self.ensure_session(account) })
    }
    fn current_scope(&self, _account: &str) -> Option<ConnectionScope> {
        None
    }
    fn provider_request(
        &self,
        _request: ProviderRequest,
    ) -> Result<SessionReceipt, AccountSessionError> {
        Err(AccountSessionError::Requires(
            "provider-session/native-request",
        ))
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
    fn transfer_provider_request(
        &self,
        _request: TransferProviderRequest,
    ) -> Result<SessionReceipt, AccountSessionError> {
        Err(AccountSessionError::Requires(
            "account-transfer/source-session-preflight",
        ))
    }
    fn callback_barrier(
        &self,
        _account: &str,
        _expected_scope: &ConnectionScope,
        _deadline: std::time::Instant,
    ) -> Result<(), AccountSessionError> {
        Err(AccountSessionError::Requires(
            "provider-session/callback-barrier",
        ))
    }
    fn recover_connection(
        &self,
        _account: &str,
        _agent_id: &str,
    ) -> Result<Value, AccountSessionError> {
        Err(AccountSessionError::Requires(
            "account-worker/recovery-history",
        ))
    }
    fn login(&self, _request: LoginRequest) -> Result<Value, AccountSessionError> {
        Err(AccountSessionError::Requires(
            "provider-session/codex-login",
        ))
    }
    fn cancel_login(&self, _request: &str) -> Result<Value, AccountSessionError> {
        Err(AccountSessionError::Requires(
            "provider-session/codex-login",
        ))
    }
    fn discover(&self) -> Result<Value, AccountSessionError> {
        Err(AccountSessionError::Requires(
            "provider-session/claude-discovery",
        ))
    }
    fn profile(&self, _request: ProfileRequest) -> Result<Value, AccountSessionError> {
        Err(AccountSessionError::Requires(
            "provider-session/claude-profile-auth",
        ))
    }
    fn limits_snapshot(&self, _account: &str) -> Result<Value, AccountSessionError> {
        Err(AccountSessionError::Requires(LIMITS_REQUIRED))
    }
    /// Coalesce by account in the session owner. Return after enqueueing a bounded
    /// read; repeat refreshes must not create unbounded tasks or native requests.
    fn refresh_limits(&self, _account: &str) -> Result<(), AccountSessionError> {
        Err(AccountSessionError::Requires(LIMITS_REQUIRED))
    }
    /// Read only `account/rateLimits/read`, then commit telemetry and publish the
    /// fresh snapshot through the existing notification/usage-resume adapters.
    fn read_limits(&self, _account: &str) -> Result<Value, AccountSessionError> {
        Err(AccountSessionError::Requires(LIMITS_REQUIRED))
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
    pub fn decision(&self, cached: bool) -> LimitsDecision {
        let data = self.0.get("data").is_some_and(|value| !value.is_null());
        let error = self
            .0
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
        if cached && (data || error) {
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

/// Same read timing as the Python HTTP edge: cached responses perform no effect,
/// existing data queues refresh, and an empty cache waits for the bounded read.
pub fn limits(
    port: &dyn AccountSessionPort,
    account: &str,
    cached: bool,
) -> Result<Value, AccountSessionError> {
    let snapshot = LimitsSnapshot::read(account, port.limits_snapshot(account)?)?;
    match snapshot.decision(cached) {
        LimitsDecision::Cached => Ok(snapshot.into_value()),
        LimitsDecision::Refresh => {
            port.refresh_limits(account)?;
            Ok(snapshot.into_value())
        }
        LimitsDecision::Read => {
            Ok(LimitsSnapshot::read(account, port.read_limits(account)?)?.into_value())
        }
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
}

fn persist_limits(
    store: &studio_runtime_store::RuntimeStore,
    refreshed_limits: &std::sync::Mutex<std::collections::BTreeMap<String, Value>>,
    account: &str,
    value: &Value,
) -> Result<(), AccountSessionError> {
    use rusqlite::OptionalExtension;
    use studio_runtime_store::SyncDocumentsRepository;
    let mut refreshed = refreshed_limits
        .lock()
        .map_err(|_| AccountSessionError::Persistence)?;
    let result = store.transaction(|transaction| {
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
        by_account.insert(account.to_owned(), value.clone());
        let mut patch = serde_json::json!({"rateLimitsByAccount":by_account});
        if account == "default" {
            patch["rateLimits"] = value.clone();
        }
        SyncDocumentsRepository::patch_entity(transaction, "workspace", "current", patch)
            .map_err(|_| studio_runtime_store::StoreError::InvalidSyncObject {
                scope: "workspace".into(),
                id: "current".into(),
            })
    });
    result.map_err(|_| AccountSessionError::Persistence)?;
    refreshed.insert(account.to_owned(), value.clone());
    Ok(())
}

impl AccountSessionRuntime {
    fn block_on_retained<F: std::future::Future>(&self, future: F) -> F::Output {
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::block_in_place(|| self.runtime.block_on(future))
        } else {
            self.runtime.block_on(future)
        }
    }

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
    fn persist_limits(&self, account: &str, value: &Value) -> Result<(), AccountSessionError> {
        let store = self
            .store
            .as_ref()
            .ok_or(AccountSessionError::Requires(LIMITS_REQUIRED))?;
        persist_limits(store, &self.refreshed_limits, account, value)
    }
    fn exchange(
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
        let receipt = self.block_on_retained(async {
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
        });
        match receipt {
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
    }
    fn accounts(&self) -> Result<Value, AccountSessionError> {
        let snapshot = self.driver.listing_snapshot().map_err(account_error)?;
        Ok(
            serde_json::json!({"accounts":snapshot.accounts,"archivedAccounts":snapshot.archived_accounts,"defaultAccountKey":snapshot.default_account_key,"logins":snapshot.logins,"supportsDisconnect":true,"supportsDelete":true}),
        )
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
                    return Err(AccountSessionError::Persistence);
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

    fn ensure_session(&self, account: &str) -> Result<ConnectionScope, AccountSessionError> {
        let session = self.session.as_ref().ok_or(AccountSessionError::Requires(
            "provider-session/account-session",
        ))?;
        let scope = self.block_on_retained(session.ensure_session(account.to_owned()))?;
        if scope.account != account {
            return Err(AccountSessionError::Refused(
                "Provider session account mismatch",
            ));
        }
        Ok(scope)
    }

    fn ensure_session_async<'a>(
        &'a self,
        account: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<ConnectionScope, AccountSessionError>> + Send + 'a>>
    {
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

    fn recover_connection(
        &self,
        account: &str,
        agent_id: &str,
    ) -> Result<Value, AccountSessionError> {
        let session = self.session.as_ref().ok_or(AccountSessionError::Requires(
            "account-worker/recovery-history",
        ))?;
        let expected = session
            .current_scope(account)
            .ok_or(AccountSessionError::Unknown)?;
        self.block_on_retained(session.recover_connection(account.to_owned(), agent_id.to_owned()))
            .and_then(|result| {
                if session.current_scope(account).as_ref() == Some(&expected) {
                    Ok(result)
                } else {
                    Err(AccountSessionError::Unknown)
                }
            })
    }

    fn provider_request(
        &self,
        request: ProviderRequest,
    ) -> Result<SessionReceipt, AccountSessionError> {
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
        match self.block_on_retained(async {
            tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                session.request(request),
            )
            .await
        }) {
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
    }
    fn transfer_provider_request(
        &self,
        request: TransferProviderRequest,
    ) -> Result<SessionReceipt, AccountSessionError> {
        let session = self.session.as_ref().ok_or(AccountSessionError::Requires(
            "account-transfer/source-session-preflight",
        ))?;
        let deadline = request.deadline;
        let operation = request.operation.clone();
        let account = request.source_account.clone();
        let scope = request.expected_scope.clone();
        match self.block_on_retained(async {
            tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                session.transfer_request(request),
            )
            .await
        }) {
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
    }
    fn callback_barrier(
        &self,
        account: &str,
        expected_scope: &ConnectionScope,
        deadline: std::time::Instant,
    ) -> Result<(), AccountSessionError> {
        let session = self.session.as_ref().ok_or(AccountSessionError::Requires(
            "provider-session/callback-barrier",
        ))?;
        let wait = self.block_on_retained(async {
            tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                session.callback_barrier(account.to_owned(), expected_scope.clone(), deadline),
            )
            .await
        });
        match wait {
            Ok(result) => result,
            Err(_) => Err(AccountSessionError::Unknown),
        }
    }
    fn read_limits(&self, account: &str) -> Result<Value, AccountSessionError> {
        let snapshot = self.driver.snapshot().map_err(account_error)?;
        if !snapshot
            .accounts
            .iter()
            .chain(&snapshot.archived_accounts)
            .any(|row| row["id"] == account)
        {
            return Err(AccountSessionError::Refused("Unknown Codex account"));
        }
        let scope = self.ensure_session(account)?;
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let operation = format!(
            "limits-read:{account}:{}:{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| AccountSessionError::Persistence)?
                .as_nanos(),
            SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let receipt = self.provider_request(ProviderRequest {
            account: account.to_owned(),
            operation: operation.clone(),
            actor: None,
            expected_scope: Some(scope.clone()),
            deleted_actor_session: None,
            method: Method::AccountRateLimitsRead,
            params: serde_json::json!({}),
            deadline: std::time::Instant::now() + Duration::from_secs(10),
        })?;
        if receipt.scope != scope
            || receipt.operation != operation
            || receipt.certainty != RequestCertainty::Written
            || receipt.ticket.is_none()
        {
            return Err(AccountSessionError::Unknown);
        }
        let data = match receipt.result {
            Some(ProviderResponse::Reply(data)) => data,
            Some(ProviderResponse::Rejected(_)) | None => {
                return Err(AccountSessionError::Refused("Account limits read failed"));
            }
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| AccountSessionError::Persistence)?
            .as_secs_f64();
        let value = LimitsSnapshot::read(
            account,
            serde_json::json!({"accountKey":account,"data":data,"at":now,"readAt":now,"error":null}),
        )?
        .into_value();
        self.persist_limits(account, &value)?;
        Ok(value)
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
    fn refresh_limits(&self, account: &str) -> Result<(), AccountSessionError> {
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
        self.runtime.spawn(async move {
            let refresh_account = account.clone();
            let _ = tokio::task::spawn_blocking(move || {
                AccountSessionPort::read_limits(&runtime, &refresh_account)
            })
            .await;
            if let Ok(mut pending) = pending.lock() {
                pending.remove(&account);
            }
        });
        Ok(())
    }
    fn login(&self, mut request: LoginRequest) -> Result<Value, AccountSessionError> {
        request.login_id = uuid::Uuid::parse_str(&request.login_id)
            .map_err(|_| AccountSessionError::Refused("Supply a UUID request_id"))?
            .to_string();
        if let Some(receipt) = self
            .driver
            .codex_login_receipt(&request)
            .map_err(mutation_error)?
        {
            return self
                .driver
                .reconcile_codex_login(&request.login_id)
                .map(|reconciled| public_login(reconciled.unwrap_or(receipt)))
                .map_err(mutation_error);
        }
        if self.session.is_none() {
            return Err(AccountSessionError::Requires(
                "provider-session/codex-login",
            ));
        }
        let home = if request.account_key.is_none() {
            self.prepare_login_home(&request.login_id)?
        } else {
            self.base_home.clone()
        };
        let reservation = self
            .driver
            .reserve_codex_login(&request, &home, timestamp()?)
            .map_err(mutation_error)?;
        let receipt = match reservation {
            LoginReservation::Replay(receipt) => return Ok(public_login(receipt)),
            LoginReservation::Dispatch(receipt) => receipt,
        };
        let account = receipt["accountKey"]
            .as_str()
            .ok_or_else(|| AccountSessionError::InvalidResponse("/accountKey".into()))?;
        let response = self.exchange(
            account,
            request.account_key.is_some(),
            request.login_id.clone(),
            "account/login/start",
            serde_json::json!({"type":"chatgptDeviceCode"}),
            LOGIN_TIMEOUT,
        );
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
        self.driver
            .finish_codex_login(
                &request.login_id,
                account,
                result.as_ref(),
                sent,
                timestamp()?,
            )
            .map(public_login)
            .map_err(mutation_error)
    }
    fn cancel_login(&self, request: &str) -> Result<Value, AccountSessionError> {
        if let Some(receipt) = self
            .driver
            .reconcile_codex_login(request)
            .map_err(mutation_error)?
        {
            return Ok(public_login(receipt));
        }
        if self.session.is_none() {
            let receipt = self
                .driver
                .snapshot()
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
        let receipt = match self
            .driver
            .reserve_codex_cancel(request)
            .map_err(mutation_error)?
        {
            LoginReservation::Replay(receipt) => return Ok(public_login(receipt)),
            LoginReservation::Dispatch(receipt) => receipt,
        };
        let account = receipt["accountKey"]
            .as_str()
            .ok_or_else(|| AccountSessionError::InvalidResponse("/accountKey".into()))?;
        let response = self.exchange(
            account,
            receipt.get("reauthAccountKey").is_some(),
            format!("{request}:cancel"),
            "account/login/cancel",
            serde_json::json!({"loginId":receipt["loginId"]}),
            CANCEL_TIMEOUT,
        );
        if response
            .as_ref()
            .is_ok_and(|receipt| receipt.certainty == RequestCertainty::NotSent)
        {
            return self
                .driver
                .finish_codex_cancel_not_sent(request, account)
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
        self.driver
            .finish_codex_cancel(request, account, confirmed)
            .map(public_login)
            .map_err(mutation_error)
    }
    fn discover(&self) -> Result<Value, AccountSessionError> {
        self.driver.discover_codex().map_err(account_error)?;
        if AccountDriver::<SystemProviderAuthReader>::claude_discovery_available() {
            let home = self.home.join(".claude");
            let metadata = SystemProviderAuthReader.read(&AuthProfile {
                provider: "claude".into(),
                home: home.clone(),
                claude_options: None,
            });
            self.driver
                .discover_claude_verified(&home, &metadata)
                .map_err(mutation_error)?;
        }
        self.accounts()
    }
    fn profile(&self, request: ProfileRequest) -> Result<Value, AccountSessionError> {
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
            let metadata = SystemProviderAuthReader.read(&AuthProfile {
                provider: "claude".into(),
                home: home.clone(),
                claude_options: Some(Value::Object(options.clone())),
            });
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
    use super::{AccountSessionError, persist_limits};
    use serde_json::{Value, json};
    use std::{collections::BTreeMap, sync::Mutex};
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

    #[test]
    fn refreshed_limits_persist_and_preserve_other_accounts() {
        let root = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(root.path())).unwrap();
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
            .unwrap();
        let cache = Mutex::new(BTreeMap::new());
        let value = json!({"accountKey":"default","data":{"accountId":"fixture"},"at":1.0,"readAt":1.0,"error":null});

        persist_limits(&store, &cache, "default", &value).unwrap();

        let entity = stored_workspace(&store);
        assert_eq!(entity["value"]["rateLimits"], value);
        assert_eq!(entity["value"]["rateLimitsByAccount"]["default"], value);
        assert_eq!(
            entity["value"]["rateLimitsByAccount"]["other"]["accountKey"],
            "other"
        );
        assert_eq!(cache.lock().unwrap().get("default"), Some(&value));
    }

    #[test]
    fn limits_persistence_failure_leaves_no_partial_write_or_cache_update() {
        let root = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(root.path())).unwrap();
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
            .unwrap();
        let before = stored_workspace(&store);
        let cache = Mutex::new(BTreeMap::new());
        let value = json!({"accountKey":"default","data":{"accountId":"fixture"},"at":1.0,"readAt":1.0,"error":null});

        assert!(matches!(
            persist_limits(&store, &cache, "default", &value),
            Err(AccountSessionError::Persistence)
        ));

        assert_eq!(stored_workspace(&store), before);
        assert!(cache.lock().unwrap().is_empty());
    }
}
