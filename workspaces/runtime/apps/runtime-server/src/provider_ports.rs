//! Production provider adapters owned by runtime-server.
//!
//! Automatic title generation follows `codex_rename.py:64-115`: it uses the
//! live retained Codex or Claude session, sends one prompt, and reads the exact
//! turn to completion. Every provider operation has a durable, deterministic
//! identity; Unknown writes stop the flow and are never repeated.
//!
//! Diagnostics queue differences: Python's recovery executor, clock reply
//! queue, and per-AppServer callback/tool queues do not exist as Rust queues.
//! `recoveryPending` and `clock_replies` are therefore zero; callbacks are
//! counted from unresolved callback tasks and tool requests from
//! queued/running `runtime_tool_requests` rows.

use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_providers::{Method, ProviderKind};
use studio_runtime_schema::provider_lifecycle::CodexTurnStatus;
use studio_runtime_services::{
    account_driver::AccountDriver,
    account_session_runtime::{
        AccountSessionError, AccountSessionPort, ConnectionScope, ProviderRequest,
        ProviderResponse, RequestCertainty, SessionReceipt,
    },
    account_worker::retained::{DIAGNOSTICS_OPERATION_PREFIX, RequestActor},
    provider_versions::{ActiveProviderConnections, ProviderProfile},
    title_generation::{
        TitleReceipt, TitleRequest, TitleRequestPort, WriteOutcome, normalize_title,
    },
};
use studio_runtime_store::RuntimeStore;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_POLLS: u32 = 64;
// These projections read only title-generation fields; provider items and
// extensions remain opaque. Empty turn IDs retain their existing behavior.
// ThreadLifecycle owns the general provider thread identity observation; this
// partial wire view retains the title request's separate 1024-byte limit.
#[derive(Deserialize)]
struct TitleThreadIdentity<'a> {
    id: &'a str,
}

fn title_thread_id(value: &Value) -> Option<&str> {
    let thread = value.get("thread")?;
    if !thread.is_object() {
        return None;
    }
    TitleThreadIdentity::deserialize(thread)
        .ok()
        .map(|thread| thread.id)
        .filter(|id| !id.is_empty() && id.len() <= 1024)
}

#[derive(Deserialize)]
struct TitleTurnIdentity<'a> {
    id: &'a str,
}

#[derive(Deserialize)]
struct TitleTurnObservation<'a> {
    id: &'a str,
    status: &'a str,
}

#[derive(Deserialize)]
struct TitleTurnList {
    data: Vec<Value>,
}

#[derive(Deserialize)]
struct TitleItems {
    items: Vec<Value>,
}

#[derive(Deserialize)]
struct TitleMessage<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    phase: &'a str,
    text: &'a str,
}

fn title_turn_id(value: &Value) -> Option<&str> {
    let turn = value.get("turn")?;
    if !turn.is_object() {
        return None;
    }
    TitleTurnIdentity::deserialize(turn)
        .ok()
        .map(|turn| turn.id)
}

fn title_turn_complete(candidate: &Value, turn_id: &str) -> bool {
    if !candidate.is_object() {
        return false;
    }
    let Ok(turn) = TitleTurnObservation::deserialize(candidate) else {
        return false;
    };
    turn.id == turn_id
        && (CodexTurnStatus::observe(candidate).and_then(CodexTurnStatus::terminal).is_some()
            // Older title providers report cancelled; Codex's generated status
            // contract does not include it, so do not broaden that shared owner.
            || turn.status == "cancelled")
}

fn title_final_answer(turn: &Value) -> Option<String> {
    let items = TitleItems::deserialize(turn).ok()?;
    items.items.iter().rev().find_map(|item| {
        if !item.is_object() {
            return None;
        }
        let message = TitleMessage::deserialize(item).ok()?;
        (message.kind == "agentMessage" && message.phase == "final_answer")
            .then(|| message.text.to_owned())
    })
}

/// Time one account has to answer its diagnostics query before it is reported
/// unavailable; accounts are queried concurrently, so this bounds the whole phase.
const DIAGNOSTICS_ACCOUNT_DEADLINE: Duration = Duration::from_secs(1);
static DIAGNOSTIC_OPERATION: AtomicU64 = AtomicU64::new(0);

/// Captures live provider diagnostics with bounded, read-only requests through
/// the retained session. The HTTP route invokes this on its blocking pool.
pub struct ProviderDiagnostics {
    pub accounts: Arc<AccountDriver>,
    pub session: Arc<dyn AccountSessionPort>,
    pub store: Arc<RuntimeStore>,
}

/// Runtime-server knows whether supervisor mode was requested and selected.
/// The current host exposes no health feed or recovery notice, so those fields
/// stay absent instead of implying a health check ran.
pub struct SupervisorDiagnostics {
    pub mode: bool,
    pub fallback: bool,
    pub notice: Option<String>,
    pub state_dir: PathBuf,
}

impl studio_runtime_services::diagnostics::SupervisorDiagnosticsPort for SupervisorDiagnostics {
    fn snapshot(&self) -> studio_runtime_services::diagnostics::SupervisorDiagnosticsSnapshot {
        let health = if self.mode { Some(self.health()) } else { None };
        studio_runtime_services::diagnostics::SupervisorDiagnosticsSnapshot {
            mode: self.mode,
            fallback: self.fallback,
            notice: self.notice.clone(),
            health,
        }
    }
}

impl SupervisorDiagnostics {
    fn health(&self) -> Value {
        #[cfg(unix)]
        {
            use studio_runtime_process::{HealthRequest, SupervisorClient};
            let result =
                SupervisorClient::connect_operator(&self.state_dir, self.state_dir.clone())
                    .and_then(|mut client| client.health(HealthRequest::default()));
            match result {
                Ok(status) => supervisor_health_payload(status, &self.state_dir),
                Err(error) => json!({"error":format!("Supervisor health is unavailable: {error}")}),
            }
        }
        #[cfg(not(unix))]
        {
            json!({"error":"Supervisor health transport is unavailable on this platform."})
        }
    }
}

fn supervisor_health_payload(
    status: studio_runtime_process::HealthResult,
    expected_state: &std::path::Path,
) -> Value {
    if status.protocol != studio_runtime_process::SUPERVISOR_PROTOCOL_VERSION
        || expected_state.to_str() != Some(status.state_dir.as_str())
    {
        return json!({"error":"The supervisor identity is incompatible."});
    }
    if status.recovery.blocked.is_some() || status.recovery.degraded {
        return json!({"error":"Supervisor crash recovery needs operator attention."});
    }
    serde_json::to_value(status)
        .unwrap_or_else(|_| json!({"error":"invalid supervisor health response"}))
}

impl studio_runtime_services::diagnostics::ProviderDiagnosticsPort for ProviderDiagnostics {
    fn snapshot<'a>(
        &'a self,
    ) -> futures_util::future::BoxFuture<
        'a,
        studio_runtime_services::diagnostics::ProviderDiagnosticsSnapshot,
    > {
        use studio_runtime_services::diagnostics::ProviderDiagnosticsSnapshot;
        let accounts = Arc::clone(&self.accounts);
        let session = Arc::clone(&self.session);
        Box::pin(async move {
            let active = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-server/provider-diagnostics-local",
                move || {
                    let registry = accounts
                        .snapshot()
                        .map_err(|_| AccountSessionError::Unknown)?;
                    let mut active = registry
                        .accounts
                        .iter()
                        .filter_map(|account| {
                            let key = account.id.as_str()?.to_owned();
                            let provider = account.provider_kind()?;
                            let scope = session.current_scope(&key)?;
                            (scope.account == key && !scope.connection.is_empty())
                                .then_some((key, provider, scope))
                        })
                        .collect::<Vec<_>>();
                    active.sort_by(|left, right| left.0.cmp(&right.0));
                    Ok::<_, AccountSessionError>(active)
                },
            )
            .await;
            let Ok(Ok(active)) = active else {
                return ProviderDiagnosticsSnapshot {
                    native_accounts: json!({}),
                    studio_loaded_threads: 0,
                    queues: serde_json::Map::new(),
                };
            };
            // The durable counts and every account's live read are independent;
            // each account has its own deadline, so the slowest bounds the phase.
            let accounts_keys = active
                .iter()
                .map(|(account, _, _)| account.clone())
                .collect::<Vec<_>>();
            let queue_counts = self.store.read_async(
                "runtime-server/provider-diagnostics-queues",
                move |connection| account_queue_counts(connection, &accounts_keys),
            );
            let deadline = Instant::now() + DIAGNOSTICS_ACCOUNT_DEADLINE;
            let account_reads =
                futures_util::future::join_all(active.iter().map(|(_, provider, scope)| {
                    let adapter = studio_runtime_services::provider_adapter::adapter(*provider);
                    provider_read(
                        self.session.as_ref(),
                        scope,
                        None,
                        adapter.diagnostics_method(),
                        adapter.diagnostics_params(),
                        deadline,
                    )
                }));
            let (queue_counts, account_reads) = tokio::join!(queue_counts, account_reads);
            let (local_queues, idle_limit_seconds) =
                queue_counts.unwrap_or_else(|_| (vec![(0, 0); active.len()], 0));
            let mut native_accounts = serde_json::Map::new();
            let mut queues = serde_json::Map::new();
            let mut loaded_threads = 0_u64;
            for (index, ((_, provider, _), result)) in active.iter().zip(account_reads).enumerate()
            {
                let alias = format!("account{}", index + 1);
                match (result, *provider) {
                    (Some(value), ProviderKind::Claude) => {
                        let mut row = serde_json::Map::new();
                        row.insert("provider".to_owned(), json!(ProviderKind::Claude.key()));
                        if let Some(fields) = value.as_object() {
                            row.extend(fields.clone());
                        }
                        // The server's idle sweep closes idle sessions; 0 never closes them.
                        row.insert("idleLimitSeconds".to_owned(), json!(idle_limit_seconds));
                        native_accounts.insert(alias.clone(), Value::Object(row));
                    }
                    (Some(value), ProviderKind::Codex) => {
                        let count = value
                            .get("data")
                            .and_then(Value::as_array)
                            .map_or(0, Vec::len);
                        loaded_threads = loaded_threads.saturating_add(count as u64);
                        native_accounts.insert(
                            alias.clone(),
                            json!({"provider":ProviderKind::Codex.key(), "loadedThreads":count}),
                        );
                    }
                    (None, ProviderKind::Codex | ProviderKind::Claude) => {
                        native_accounts.insert(
                            alias.clone(),
                            json!({"error":"ProviderDiagnosticsUnavailable"}),
                        );
                    }
                }
            }
            for (index, (callbacks, tool_requests)) in local_queues.into_iter().enumerate() {
                let alias = format!("account{}", index + 1);
                queues.insert(format!("{alias}.callbacks"), json!(callbacks));
                queues.insert(format!("{alias}.clock_replies"), json!(0));
                queues.insert(format!("{alias}.tool_requests"), json!(tool_requests));
            }
            queues.insert("recoveryPending".to_owned(), json!(0));
            ProviderDiagnosticsSnapshot {
                native_accounts: Value::Object(native_accounts),
                studio_loaded_threads: loaded_threads,
                queues,
            }
        })
    }
}

/// Callback records still owing an answer and queued or running tool requests of each account
/// (in the order given), and the idle-session limit shared by all of them.
fn account_queue_counts(
    connection: &rusqlite::Connection,
    accounts: &[String],
) -> studio_runtime_store::StoreResult<(Vec<(u64, u64)>, u64)> {
    let tool_requests =
        studio_runtime_store::StartupRecoveryRepository::unsettled_tool_request_counts_by_account(
            connection,
        )?;
    let counts = accounts
        .iter()
        .map(|account| {
            let callbacks = studio_runtime_services::provider_callbacks::pending_callback_count(
                connection, account,
            )?;
            Ok((
                callbacks as u64,
                tool_requests.get(account).copied().unwrap_or_default(),
            ))
        })
        .collect::<studio_runtime_store::StoreResult<Vec<_>>>()?;
    let idle_limit_seconds =
        studio_runtime_store::OrchestrationSettingsRepository::idle_session_unload(connection)
            .ok()
            .map_or(0, |delay| delay.map_or(0, |delay| delay.as_secs()));
    Ok((counts, idle_limit_seconds))
}

async fn provider_read(
    session: &dyn AccountSessionPort,
    scope: &ConnectionScope,
    actor: Option<RequestActor>,
    method: Method,
    params: Value,
    deadline: Instant,
) -> Option<Value> {
    let operation = format!(
        "{DIAGNOSTICS_OPERATION_PREFIX}{}:{}:{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_nanos(),
        DIAGNOSTIC_OPERATION.fetch_add(1, Ordering::Relaxed),
    );
    if Instant::now() >= deadline {
        return None;
    }
    let receipt = session
        .provider_request(ProviderRequest {
            root_request_id: String::new(),
            admitted_request: None,
            account: scope.account.clone(),
            operation: operation.clone(),
            actor,
            deleted_actor_session: None,
            expected_scope: Some(scope.clone()),
            method,
            params,
            deadline,
        })
        .await
        .ok()?;
    if receipt.scope != *scope
        || receipt.operation != operation
        || receipt.certainty() != RequestCertainty::Written
    {
        return None;
    }
    match receipt.into_response()? {
        ProviderResponse::Reply(value) => Some(value),
        ProviderResponse::Rejected(_) => None,
    }
}

/// Builds provider-version inputs from registry metadata joined to the same
/// retained live session broker used for provider requests. Registry-only and
/// offline accounts are deliberately excluded.
pub struct LiveProviderConnections {
    pub accounts: Arc<AccountDriver>,
    pub session: Arc<dyn AccountSessionPort>,
    pub session_binaries:
        Arc<std::sync::Mutex<std::collections::BTreeMap<String, SessionExecutables>>>,
    pub changes: tokio::sync::watch::Receiver<u64>,
}

/// The provider executable a retained account session was launched with.
#[derive(Clone, Debug)]
pub struct SessionExecutables {
    pub generation: u64,
    pub kind: studio_runtime_providers::ProviderKind,
    pub executable: Option<String>,
}

impl ActiveProviderConnections for LiveProviderConnections {
    fn subscribe_changes(&self) -> tokio::sync::watch::Receiver<u64> {
        self.changes.clone()
    }

    fn snapshot(&self) -> Vec<ProviderProfile> {
        let Ok(snapshot) = self.accounts.snapshot() else {
            return Vec::new();
        };
        snapshot
            .accounts
            .iter()
            .filter_map(|account| {
                let account_key = account.id.as_str()?.to_owned();
                let scope = self.session.current_scope(&account_key)?;
                if scope.account != account_key || scope.connection.is_empty() {
                    return None;
                }
                let kind = account.provider_kind()?;
                let launched_binary = leased_session_binary(
                    &self.session_binaries,
                    &account_key,
                    scope.generation,
                    kind,
                );
                let configured_binary = match kind {
                    studio_runtime_providers::ProviderKind::Codex => launched_binary.clone(),
                    // A Claude account without launch-time executable metadata is
                    // not a probeable retained session; never substitute today's
                    // profile setting for the binary used by that session.
                    studio_runtime_providers::ProviderKind::Claude => {
                        launched_binary.as_ref()?;
                        studio_runtime_services::account_driver::claude::resolve_public_claude_binary(
                            account,
                        )
                    }
                };
                let provider = kind.key().to_owned();
                Some(ProviderProfile {
                    account_key,
                    provider,
                    launched_binary,
                    configured_binary,
                    server_identity: scope.connection,
                    connection_id: Some(scope.generation.to_string()),
                })
            })
            .collect()
    }
}

fn leased_session_binary(
    binaries: &std::sync::Mutex<std::collections::BTreeMap<String, SessionExecutables>>,
    account: &str,
    generation: u64,
    kind: studio_runtime_providers::ProviderKind,
) -> Option<String> {
    let executables = binaries.lock().ok()?.get(account)?.clone();
    (executables.generation == generation && executables.kind == kind)
        .then_some(executables.executable)
        .flatten()
}

/// Uses the account's already-retained provider session. It never launches a
/// CLI or opens a second connection.
pub struct ProviderTitleRequestPort {
    session: Arc<dyn AccountSessionPort>,
    store: Arc<RuntimeStore>,
}

impl ProviderTitleRequestPort {
    pub fn new(session: Arc<dyn AccountSessionPort>, store: Arc<RuntimeStore>) -> Self {
        Self { session, store }
    }
}

impl TitleRequestPort for ProviderTitleRequestPort {
    fn request<'a>(
        &'a self,
        request: TitleRequest,
    ) -> Pin<Box<dyn Future<Output = TitleReceipt> + Send + 'a>> {
        Box::pin(async move {
            let fallback = || title_receipt(&request, WriteOutcome::Unknown, None);
            let deadline = request
                .deadline
                .duration_since(SystemTime::now())
                .ok()
                .and_then(|remaining| Instant::now().checked_add(remaining));
            let Some(deadline) = deadline else {
                return title_receipt(&request, WriteOutcome::NotSent, None);
            };
            let Ok(mut home) = TitleHome::new(&request.request_id) else {
                return title_receipt(&request, WriteOutcome::NotSent, None);
            };
            let actor = RequestActor {
                agent: request.agent_id.clone(),
                epoch: request.epoch,
            };
            let Ok(provider) = request
                .provider
                .as_deref()
                .map_or(Ok(ProviderKind::Codex), str::parse::<ProviderKind>)
            else {
                return title_receipt(&request, WriteOutcome::NotSent, None);
            };
            let operation_root = operation_root(&request.request_id);
            let start_operation = format!("{operation_root}:thread");
            // Cancellation or process loss during a submitted write must leave
            // the same cwd available when the durable receipt is inspected.
            home.retain();
            let mut thread_params = serde_json::Map::new();
            thread_params.insert("cwd".into(), json!(home.path()));
            thread_params.insert("model".into(), json!(request.model));
            thread_params.insert("ephemeral".into(), json!(true));
            studio_runtime_services::provider_adapter::adapter(provider)
                .shape_title_thread_params(&mut thread_params);
            let started = self
                .call(
                    &request,
                    &start_operation,
                    Some(actor.clone()),
                    Method::ThreadStart,
                    Value::Object(thread_params),
                    deadline,
                    None,
                )
                .await;
            let Some(started) = started else {
                home.retain();
                return fallback();
            };
            match started.certainty() {
                RequestCertainty::NotSent => {
                    home.release();
                    return title_receipt(&request, WriteOutcome::NotSent, None);
                }
                RequestCertainty::Unknown => {
                    home.retain();
                    return fallback();
                }
                RequestCertainty::Written => {}
            }
            if matches!(started.response(), Some(ProviderResponse::Rejected(_))) {
                home.release();
                return title_receipt(&request, WriteOutcome::Written, None);
            }
            let connection = started.scope.clone();
            if connection.connection.is_empty() {
                home.retain();
                return fallback();
            }
            let Some(thread_id) = started
                .pipe_response_value()
                .and_then(title_thread_id)
                .map(str::to_owned)
            else {
                home.retain();
                return fallback();
            };
            let mut turn_params = json!({
                "threadId": thread_id,
                "input": [{"type": "text", "text": request.prompt}],
                "model": request.model,
                "clientUserMessageId": format!("{operation_root}:turn"),
            });
            if let Some(effort) = request.effort.as_deref() {
                turn_params["effort"] = json!(effort);
            }
            let turn_operation = format!("{operation_root}:turn");
            let turn = self
                .call(
                    &request,
                    &turn_operation,
                    Some(actor.clone()),
                    Method::TurnStart,
                    turn_params,
                    deadline,
                    Some(&connection),
                )
                .await;
            let Some(turn) = turn else {
                home.retain();
                return fallback();
            };
            match turn.certainty() {
                RequestCertainty::NotSent => {
                    // The thread was already created. Retrying the whole title
                    // after this point could create another thread after replay expiry.
                    return fallback();
                }
                RequestCertainty::Unknown => {
                    home.retain();
                    return fallback();
                }
                RequestCertainty::Written => {}
            }
            if matches!(turn.response(), Some(ProviderResponse::Rejected(_))) {
                home.release();
                return title_receipt(&request, WriteOutcome::Written, None);
            }
            let Some(turn_id) = turn
                .pipe_response_value()
                .and_then(title_turn_id)
                .map(str::to_owned)
            else {
                home.retain();
                return fallback();
            };
            for poll in 0..MAX_POLLS {
                if Instant::now() >= deadline {
                    home.retain();
                    return fallback();
                }
                let operation = format!("{operation_root}:poll:{poll}");
                let listed = self
                    .call(
                        &request,
                        &operation,
                        Some(actor.clone()),
                        Method::ThreadTurnsList,
                        json!({
                            "threadId": thread_id,
                            "limit": 1,
                            "sortDirection": "desc",
                            "itemsView": "full",
                        }),
                        deadline,
                        Some(&connection),
                    )
                    .await;
                if let Some(listed) = listed
                    && listed.certainty() == RequestCertainty::Written
                    && let Some(value) = listed.pipe_response_value()
                    && value.is_object()
                    && let Ok(turns) = TitleTurnList::deserialize(value)
                    && let Some(turn) = turns
                        .data
                        .iter()
                        .find(|candidate| title_turn_complete(candidate, &turn_id))
                {
                    let title =
                        title_final_answer(turn).and_then(|value| normalize_title(&value).ok());
                    home.release();
                    return title_receipt(&request, WriteOutcome::Written, title);
                }
                let delay = deadline
                    .saturating_duration_since(Instant::now())
                    .min(POLL_INTERVAL);
                if delay.is_zero() {
                    home.retain();
                    return fallback();
                }
                tokio::time::sleep(delay).await;
            }
            home.retain();
            fallback()
        })
    }
}

impl ProviderTitleRequestPort {
    #[allow(clippy::too_many_arguments)]
    async fn call(
        &self,
        title: &TitleRequest,
        operation: &str,
        actor: Option<RequestActor>,
        method: Method,
        params: Value,
        deadline: Instant,
        expected_scope: Option<&ConnectionScope>,
    ) -> Option<SessionReceipt> {
        let session = Arc::clone(&self.session);
        let expected_account = title.account_key.clone();
        let expected_operation = operation.to_owned();
        let scope = expected_scope
            .cloned()
            .or_else(|| session.current_scope(&expected_account))?;
        let intent = studio_runtime_services::account_worker::retained::ProviderCallIntent {
            root_request_id: title.root_request_id.clone(),
            admitted_request: None,
            scope: scope.clone(),
            operation: expected_operation.clone(),
            actor: actor.clone(),
            transfer: None,
            deleted_actor_session: None,
            method,
            params: params.clone(),
        };
        let title = title.clone();
        let root_request_id = title.root_request_id.clone();
        let admitted_request =
            if studio_runtime_services::account_worker::retained::ProviderReadMethod::from_request(
                method, &params,
            )
            .is_some()
            {
                None
            } else {
                Some(self
            .store
            .transaction_with(move |tx| {
                studio_runtime_services::title_generation::validate_dispatch(tx, &title).map_err(
                    |_| {
                        studio_runtime_services::account_worker::WorkerError::PreparationRefused(
                            "title/request-owner-changed".into(),
                        )
                    },
                )?;
                studio_runtime_services::account_worker::retained::reserve_provider_call(
                    tx,
                    &title.admitted_request,
                    intent,
                )
            })
            .await
            .ok()?)
            };
        let request = ProviderRequest {
            root_request_id,
            admitted_request,
            account: expected_account.clone(),
            operation: expected_operation.clone(),
            actor,
            deleted_actor_session: None,
            expected_scope: expected_scope.cloned(),
            method,
            params,
            deadline,
        };
        let receipt = session.provider_request(request).await.ok()?;
        (receipt.scope.account == expected_account
            && receipt.operation == expected_operation
            && expected_scope.is_none_or(|scope| &receipt.scope == scope))
        .then_some(receipt)
    }
}

trait SessionReceiptExt {
    fn pipe_response_value(&self) -> Option<&Value>;
}

impl SessionReceiptExt for SessionReceipt {
    fn pipe_response_value(&self) -> Option<&Value> {
        match self.response()? {
            ProviderResponse::Reply(value) => Some(value),
            ProviderResponse::Rejected(_) => None,
        }
    }
}

fn title_receipt(
    request: &TitleRequest,
    outcome: WriteOutcome,
    title: Option<String>,
) -> TitleReceipt {
    TitleReceipt {
        request_id: request.request_id.clone(),
        agent_id: request.agent_id.clone(),
        epoch: request.epoch,
        account_key: request.account_key.clone(),
        outcome,
        title,
    }
}

fn operation_root(request_id: &str) -> String {
    let digest = Sha256::digest(request_id.as_bytes());
    format!("title:{}", data_encoding::HEXLOWER.encode(&digest))
}

struct TitleHome {
    path: PathBuf,
    retain: bool,
}

impl TitleHome {
    fn new(request_id: &str) -> Result<Self, AccountSessionError> {
        let digest = Sha256::digest(request_id.as_bytes());
        let path = std::env::temp_dir().join(format!(
            "studio-title-{}",
            data_encoding::HEXLOWER.encode(&digest[..12])
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                        .map_err(|_| AccountSessionError::Persistence)?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = std::fs::symlink_metadata(&path)
                    .map_err(|_| AccountSessionError::Persistence)?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(AccountSessionError::Persistence);
                }
            }
            Err(_) => return Err(AccountSessionError::Persistence),
        }
        Ok(Self {
            path,
            retain: false,
        })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }

    fn retain(&mut self) {
        self.retain = true;
    }

    fn release(&mut self) {
        self.retain = false;
    }
}

impl Drop for TitleHome {
    fn drop(&mut self) {
        if !self.retain {
            let _ignored = std::fs::remove_dir_all(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::{
        ProviderTitleRequestPort, SupervisorDiagnostics, TitleHome, account_queue_counts,
        leased_session_binary,
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};
    use studio_runtime_providers::Method;
    use studio_runtime_services::account_session_runtime::SessionOutcome as ReceiptOutcome;
    use studio_runtime_services::account_session_runtime::{
        AccountSessionFuture, AccountSessionPort, ConnectionScope, ProviderRequest,
        ProviderResponse, RequestCertainty, SessionReceipt,
    };
    use studio_runtime_services::account_worker::retained::RequestActor;
    use studio_runtime_services::title_generation::{TitleResult, generate};

    async fn run_title(
        session: Arc<Session>,
        provider: &str,
        epoch: i64,
    ) -> (tempfile::TempDir, String, TitleResult) {
        let root = tempfile::tempdir().expect("title state");
        let store = Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(root.path()))
                .await
                .expect("store"),
        );
        let model = if provider == "claude" {
            "claude-sonnet"
        } else {
            "gpt-mini"
        };
        let account = if provider == "claude" {
            "claude-account"
        } else {
            "account-fixture"
        };
        let agent = json!({"id":"agent-fixture","rootId":"agent-fixture","isLead":true,"epoch":epoch,"status":"idle","name":"New chat","needsTitle":true,"provider":provider,"model":model,"accountKey":account});
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES('agent-fixture',?1)",
                    [agent.to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("agent");
        let catalog = studio_runtime_schema::model_catalog::ModelCatalog::parse(json!({"data":[{"model":model,"supportedReasoningEfforts":[{"reasoningEffort":"low"}]}]})).expect("catalog");
        let request_id = studio_runtime_schema::persistence::RequestIdentity::issue_root();
        let port = ProviderTitleRequestPort::new(session, Arc::clone(&store));
        let result = generate(
            &store,
            &port,
            "agent-fixture",
            &catalog,
            &request_id,
            SystemTime::now() + Duration::from_secs(10),
        )
        .await
        .expect("title owner flow");
        (root, request_id, result)
    }

    enum ReplyScenario {
        Ready,
        UnknownThread,
        NotSentTurn,
        Listed(Value),
        ThreadReply(Value),
    }

    struct Session {
        requests: Mutex<Vec<ProviderRequest>>,
        scenario: ReplyScenario,
    }

    impl AccountSessionPort for Session {
        fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
            Some(ConnectionScope {
                account: account.to_owned(),
                connection: "session-1".to_owned(),
                generation: 7,
            })
        }

        fn provider_request<'a>(
            &'a self,
            request: ProviderRequest,
        ) -> AccountSessionFuture<'a, SessionReceipt> {
            Box::pin(async move {
                let method = request.method;
                let scope = request.expected_scope.clone().unwrap_or_else(|| {
                    self.current_scope(&request.account)
                        .expect("live fixture scope")
                });
                let operation = request.operation.clone();
                let mut requests = self.requests.lock().expect("fixture request list");
                requests.push(request.clone());
                let certainty = if matches!(self.scenario, ReplyScenario::UnknownThread)
                    && method == Method::ThreadStart
                {
                    RequestCertainty::Unknown
                } else if matches!(self.scenario, ReplyScenario::NotSentTurn)
                    && method == Method::TurnStart
                {
                    RequestCertainty::NotSent
                } else {
                    RequestCertainty::Written
                };
                let result = match method {
                    Method::ThreadStart => Some(ProviderResponse::Reply(match &self.scenario {
                        ReplyScenario::ThreadReply(value) => value.clone(),
                        ReplyScenario::Ready
                        | ReplyScenario::UnknownThread
                        | ReplyScenario::NotSentTurn
                        | ReplyScenario::Listed(_) => json!({"thread":{"id":"title-thread"}}),
                    })),
                    Method::TurnStart => {
                        Some(ProviderResponse::Reply(json!({"turn":{"id":"title-turn"}})))
                    }
                    Method::ThreadTurnsList => {
                        Some(ProviderResponse::Reply(match &self.scenario {
                            ReplyScenario::Listed(value) => value.clone(),
                            ReplyScenario::Ready
                            | ReplyScenario::UnknownThread
                            | ReplyScenario::NotSentTurn
                            | ReplyScenario::ThreadReply(_) => json!({"data":[{
                                "id":"title-turn", "status":"completed", "items":[{
                                    "type":"agentMessage", "phase":"final_answer", "text":"A Useful Title"
                                }]
                            }]}),
                        }))
                    }
                    Method::Initialize
                    | Method::Initialized
                    | Method::Ack
                    | Method::AccountLoginCancel
                    | Method::AccountLoginStart
                    | Method::AccountRateLimitResetCreditConsume
                    | Method::ThreadLoadedList
                    | Method::ConfigRead
                    | Method::SkillsExtraRootsSet
                    | Method::ModelList
                    | Method::AccountRead
                    | Method::AccountRateLimitsRead
                    | Method::ThreadRealtimeStart
                    | Method::ThreadRealtimeStop
                    | Method::ThreadRealtimeAppendSpeech
                    | Method::ThreadRealtimeItemStarted
                    | Method::ThreadRealtimeTranscriptDelta
                    | Method::ThreadRealtimeTranscriptDone
                    | Method::ThreadRealtimeSdp
                    | Method::ThreadRealtimeError
                    | Method::ThreadRealtimeClosed
                    | Method::ThreadResume
                    | Method::ThreadRead
                    | Method::TurnSteer
                    | Method::TurnInterrupt
                    | Method::ThreadList
                    | Method::ThreadArchive
                    | Method::ThreadFork
                    | Method::ThreadNameSet
                    | Method::ThreadMetadataUpdate
                    | Method::ThreadSettingsUpdate
                    | Method::ThreadCompactStart
                    | Method::ThreadRollback
                    | Method::ThreadItemsList
                    | Method::ThreadGoalSet
                    | Method::ThreadGoalUpdated
                    | Method::ThreadUnarchive
                    | Method::ThreadUnsubscribe
                    | Method::ProcessSpawn
                    | Method::ProcessWriteStdin
                    | Method::ProcessKill
                    | Method::ProcessResizePty
                    | Method::CommandExec
                    | Method::CommandExecWrite
                    | Method::CommandExecResize
                    | Method::CommandExecTerminate
                    | Method::ThreadBackgroundTerminalsList
                    | Method::ThreadBackgroundTerminalsTerminate
                    | Method::ThreadBackgroundTerminalsClean
                    | Method::ThreadCommandExecutionBackground
                    | Method::ThreadQueueList
                    | Method::ClaudeState
                    | Method::ClaudeRepairReport
                    | Method::ClaudeFileBackups
                    | Method::ClaudeSettings
                    | Method::ClaudeActivity
                    | Method::ClaudeStopTask
                    | Method::ClaudeShellPolicy
                    | Method::ClaudeDiagnostics
                    | Method::ClaudeMoveVersions
                    | Method::McpServerReconnect
                    | Method::McpServerStatusList
                    | Method::SkillsList
                    | Method::OperationStatus
                    | Method::ReviewStart
                    | Method::BrowserAnswer
                    | Method::BrowserCallback
                    | Method::BrowserClose
                    | Method::BrowserWait
                    | Method::ClaudeCommands
                    | Method::ClaudeMoveExport
                    | Method::ClaudeMoveIdentity
                    | Method::ClaudeMovePreflight
                    | Method::ClaudeMoveProof
                    | Method::Detach
                    | Method::EventRead
                    | Method::Exec
                    | Method::Generate
                    | Method::Health
                    | Method::Info
                    | Method::LayrHealth
                    | Method::Next
                    | Method::Open
                    | Method::ProjectEnsure
                    | Method::ProjectExec
                    | Method::ProjectImport
                    | Method::ProviderList
                    | Method::Replay
                    | Method::ResponseStatus
                    | Method::RetireCommand
                    | Method::SecretsList
                    | Method::SecretsRemove
                    | Method::SecretsSet
                    | Method::ShareConfigure
                    | Method::ShareName
                    | Method::ShareStatus
                    | Method::Sign
                    | Method::Status
                    | Method::SyncMacApply
                    | Method::SyncMacHandoff
                    | Method::SyncMacHashes
                    | Method::SyncMacHead
                    | Method::SyncMacRead
                    | Method::SyncMacWorktree
                    | Method::VaultConfigure
                    | Method::VaultPush
                    | Method::VaultStatus
                    | Method::UploadBegin
                    | Method::UploadChunk
                    | Method::UploadCommit
                    | Method::Verify
                    | Method::Write
                    | Method::CurrentTimeRead
                    | Method::ItemToolCall
                    | Method::ItemCommandExecutionRequestApproval
                    | Method::ItemFileChangeRequestApproval
                    | Method::ItemPermissionsRequestApproval
                    | Method::McpServerElicitationRequest
                    | Method::ItemToolRequestUserInput
                    | Method::ItemStarted
                    | Method::ItemCompleted
                    | Method::ItemAgentMessageDelta
                    | Method::ItemReasoningTextDelta
                    | Method::ItemCommandExecutionOutputDelta
                    | Method::CommandExecOutputDelta
                    | Method::TurnStarted
                    | Method::TurnCompleted
                    | Method::TurnPlanUpdated
                    | Method::TurnDiffUpdated
                    | Method::ThreadClosed
                    | Method::ThreadStatusChanged
                    | Method::ThreadTokenUsageUpdated
                    | Method::ThreadShellPolicyUpdated
                    | Method::ThreadIdleCompactionUpdated
                    | Method::AccountLoginCompleted
                    | Method::AccountUpdated
                    | Method::AccountRateLimitsUpdated
                    | Method::ModelSafetyBufferingUpdated
                    | Method::ProviderGenerationStarted
                    | Method::ProtocolError => None,
                };
                Ok(SessionReceipt {
                    outcome: {
                        let ticket = studio_runtime_providers::RequestTicket {
                            generation: request
                                .expected_scope
                                .as_ref()
                                .map_or(1, |scope| scope.generation),
                            id: studio_runtime_providers::RequestId::String(
                                request.operation.clone(),
                            ),
                            method: request.method,
                        };
                        match (certainty, result) {
                            (RequestCertainty::Written, Some(response)) => {
                                ReceiptOutcome::Written { ticket, response }
                            }
                            (RequestCertainty::Written, None) | (RequestCertainty::Unknown, _) => {
                                ReceiptOutcome::Unknown {
                                    ticket: studio_runtime_providers::RequestTicket {
                                        generation: request
                                            .expected_scope
                                            .as_ref()
                                            .map_or(1, |scope| scope.generation),
                                        id: studio_runtime_providers::RequestId::String(
                                            request.operation.clone(),
                                        ),
                                        method: request.method,
                                    },
                                }
                            }
                            (RequestCertainty::NotSent, _) => ReceiptOutcome::NoLiveAccountWorker,
                        }
                    },
                    scope,
                    operation,
                    replayed: false,
                })
            })
        }
    }

    #[test]
    fn ephemeral_working_directory_is_stable_for_operation_identity() {
        let first = TitleHome::new("title-request-test").expect("first title home");
        let expected = first.path().to_owned();
        drop(first);
        let second = TitleHome::new("title-request-test").expect("recovered title home");
        assert_eq!(second.path(), expected);
    }

    #[tokio::test]
    async fn title_uses_the_retained_thread_turn_and_read_receipts() {
        let session = Arc::new(Session {
            requests: Mutex::new(Vec::new()),
            scenario: ReplyScenario::Ready,
        });
        let (_root, _request_id, result) = run_title(Arc::clone(&session), "codex", 3).await;
        assert_eq!(
            result,
            TitleResult::Applied {
                title: "A Useful Title".into()
            }
        );
        let calls = session.requests.lock().expect("fixture request list");
        let provider_root = &calls[0].root_request_id;
        assert!(
            studio_runtime_schema::persistence::RequestIdentity::new(
                "title",
                &[],
                provider_root,
                &[]
            )
            .is_ok()
        );
        assert!(
            calls
                .iter()
                .all(|call| &call.root_request_id == provider_root
                    && (call.admitted_request.is_some()
                        == (call.method != Method::ThreadTurnsList)))
        );
        assert_eq!(
            calls.iter().map(|call| call.method).collect::<Vec<_>>(),
            [
                Method::ThreadStart,
                Method::TurnStart,
                Method::ThreadTurnsList
            ]
        );
        assert!(calls.iter().all(|call| call.actor.as_ref()
            == Some(&RequestActor {
                agent: "agent-fixture".into(),
                epoch: 3,
            })));
        assert!(
            calls
                .windows(2)
                .all(|pair| pair[0].operation != pair[1].operation)
        );
        assert_eq!(
            calls[1].expected_scope.as_ref(),
            Some(
                &calls[0]
                    .expected_scope
                    .clone()
                    .unwrap_or_else(|| session.current_scope("account-fixture").unwrap())
            )
        );
        assert_eq!(calls[2].expected_scope, calls[1].expected_scope);
    }

    #[test]
    fn title_projection_matches_legacy_partial_provider_observations() {
        for id in ["", "turn", "other"] {
            for status in [
                json!("completed"),
                json!("failed"),
                json!("interrupted"),
                json!("cancelled"),
                json!("inProgress"),
                json!("unknown"),
                json!(null),
                json!(1),
            ] {
                for item in [
                    json!({"type":"agentMessage","phase":"final_answer","text":"valid"}),
                    json!({"type":"agentMessage","phase":"analysis","text":"hidden"}),
                    json!({"type":"agentMessage","phase":"final_answer","text":false}),
                    json!(["agentMessage", "final_answer", "wrong"]),
                    json!(null),
                ] {
                    let candidate = json!({"id":id,"status":status,"items":[{"type":"agentMessage","phase":"final_answer","text":"first"},item]});
                    let expected = candidate.get("id").and_then(Value::as_str) == Some("turn")
                        && candidate
                            .get("status")
                            .and_then(Value::as_str)
                            .is_some_and(|s| {
                                matches!(s, "completed" | "failed" | "interrupted" | "cancelled")
                            });
                    assert_eq!(
                        super::title_turn_complete(&candidate, "turn"),
                        expected,
                        "{candidate}"
                    );
                    let expected_text = candidate["items"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .rev()
                        .find_map(|item| {
                            (item.get("type").and_then(Value::as_str) == Some("agentMessage")
                                && item.get("phase").and_then(Value::as_str)
                                    == Some("final_answer"))
                            .then(|| item.get("text").and_then(Value::as_str))
                            .flatten()
                        });
                    assert_eq!(
                        super::title_final_answer(&candidate).as_deref(),
                        expected_text,
                        "{candidate}"
                    );
                }
            }
        }
        assert_eq!(super::title_turn_id(&json!({"turn":{"id":""}})), Some(""));
        assert_eq!(super::title_turn_id(&json!({"turn":["turn"]})), None);
        assert!(!super::title_turn_complete(
            &json!(["turn", "completed"]),
            "turn"
        ));
        assert!(super::title_final_answer(&json!({"items":false})).is_none());
    }

    #[tokio::test]
    async fn title_cancelled_turn_uses_exact_id_and_last_final_answer() {
        let session = Arc::new(Session {
            requests: Mutex::new(Vec::new()),
            scenario: ReplyScenario::Listed(json!({"data":[
                {"id":"other", "status":"completed", "items":[{"type":"agentMessage","phase":"final_answer","text":"Wrong"}]},
                {"id":"title-turn", "status":"cancelled", "items":[
                    {"type":"agentMessage","phase":"final_answer","text":"First"},
                    {"type":"agentMessage","phase":"analysis","text":"Hidden"},
                    {"type":"other","phase":"final_answer","text":"Wrong"},
                    {"type":"agentMessage","phase":"final_answer","text":"  Chosen Title  "},
                    {"type":"agentMessage","phase":"final_answer","text":false}
                ]}
            ]})),
        });
        let (_root, _request_id, result) = run_title(Arc::clone(&session), "codex", 3).await;
        assert_eq!(
            result,
            TitleResult::Applied {
                title: "Chosen Title".into()
            }
        );
        assert_eq!(session.requests.lock().unwrap().len(), 3);
    }

    #[test]
    fn title_thread_reply_projection_matches_legacy_admission() {
        let boundary = "a".repeat(1024);
        let over = "é".repeat(513);
        let cases = [
            json!(null),
            json!([]),
            json!({}),
            json!({"thread": null}),
            json!({"thread": ["id"]}),
            json!({"thread": {"id": null}}),
            json!({"thread": {"id": 12}}),
            json!({"thread": {"id": ""}}),
            json!({"thread": {"id": boundary, "extra": {"any": true}}, "future": 9}),
            json!({"thread": {"id": over}}),
            serde_json::from_str::<Value>(r#"{"thread":{"id":"first","id":"last"}}"#).unwrap(),
            serde_json::from_str::<Value>(r#"{"thread":{"id":"old"},"thread":{"id":false}}"#)
                .unwrap(),
        ];
        for value in cases {
            let legacy = value
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 1024);
            assert_eq!(super::title_thread_id(&value), legacy, "{value}");
        }
    }

    #[tokio::test]
    async fn title_invalid_thread_reply_retains_home_and_unknown_receipt() {
        let session = Arc::new(Session {
            requests: Mutex::new(Vec::new()),
            scenario: ReplyScenario::ThreadReply(json!({"thread":{"id":null}})),
        });
        let (_root, request_id, result) = run_title(Arc::clone(&session), "codex", 3).await;
        assert_eq!(result, TitleResult::HeldUnknown);
        let calls = session.requests.lock().expect("fixture requests");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, Method::ThreadStart);
        let path = std::path::PathBuf::from(calls[0].params["cwd"].as_str().expect("title cwd"));
        assert!(path.is_dir(), "the original title home was retained");
        drop(calls);
        let home = TitleHome::new(&request_id).expect("retained title home");
        assert_eq!(home.path(), path);
        drop(home);
    }

    #[tokio::test]
    async fn unknown_title_start_is_never_followed_by_another_write() {
        let _cleanup =
            super::TitleHome::new("title-request-unknown").expect("owned retained title fixture");
        let session = Arc::new(Session {
            requests: Mutex::new(Vec::new()),
            scenario: ReplyScenario::UnknownThread,
        });
        let (_root, _request_id, result) = run_title(Arc::clone(&session), "codex", 3).await;
        assert_eq!(result, TitleResult::HeldUnknown);
        let calls = session.requests.lock().expect("fixture request list");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, Method::ThreadStart);
    }

    #[test]
    fn operation_identity_is_bounded_and_deterministic() {
        let first = super::operation_root(&"r".repeat(200));
        assert_eq!(first, super::operation_root(&"r".repeat(200)));
        assert!(first.len() < 80);
    }

    #[tokio::test]
    async fn title_with_written_thread_and_unsent_turn_remains_unknown() {
        let session = Arc::new(Session {
            requests: Mutex::new(Vec::new()),
            scenario: ReplyScenario::NotSentTurn,
        });
        let (_root, _request_id, result) = run_title(Arc::clone(&session), "codex", 3).await;
        assert_eq!(result, TitleResult::HeldUnknown);
        let calls = session.requests.lock().expect("request list");
        assert_eq!(
            calls.iter().map(|call| call.method).collect::<Vec<_>>(),
            [Method::ThreadStart, Method::TurnStart]
        );
    }

    #[tokio::test]
    async fn claude_title_uses_stable_message_identity_on_retained_session() {
        let session = Arc::new(Session {
            requests: Mutex::new(Vec::new()),
            scenario: ReplyScenario::Ready,
        });
        let (_root, request_id, result) = run_title(Arc::clone(&session), "claude", 9).await;
        assert_eq!(
            result,
            TitleResult::Applied {
                title: "A Useful Title".into()
            }
        );
        let calls = session.requests.lock().expect("fixture request list");
        assert_eq!(calls[0].method, Method::ThreadStart);
        assert_eq!(calls[1].method, Method::TurnStart);
        assert_eq!(calls[0].params["ephemeral"], true);
        assert!(calls[0].params["cwd"].as_str().is_some());
        assert_eq!(calls[0].params["model"], "claude-sonnet");
        assert_eq!(
            calls[1].params["clientUserMessageId"],
            format!("{}:turn", super::operation_root(&request_id))
        );
        assert_eq!(calls[1].params["model"], "claude-sonnet");
        assert_eq!(calls[2].method, Method::ThreadTurnsList);
    }

    #[test]
    fn version_probe_uses_binary_captured_for_live_session() {
        use studio_runtime_providers::ProviderKind;
        let binaries = Mutex::new(std::collections::BTreeMap::from([(
            "account-a".to_owned(),
            super::SessionExecutables {
                generation: 7,
                kind: ProviderKind::Claude,
                executable: Some("/immutable/leased/claude".to_owned()),
            },
        )]));
        assert_eq!(
            leased_session_binary(&binaries, "account-a", 7, ProviderKind::Claude).as_deref(),
            Some("/immutable/leased/claude")
        );
        assert_eq!(
            leased_session_binary(&binaries, "account-a", 7, ProviderKind::Codex),
            None
        );
        assert_eq!(
            leased_session_binary(&binaries, "account-a", 8, ProviderKind::Claude),
            None
        );
        assert_eq!(
            leased_session_binary(&binaries, "account-b", 7, ProviderKind::Claude),
            None
        );
    }

    #[test]
    fn supervisor_mode_reports_health_lookup_error_instead_of_null() {
        let directory = tempfile::tempdir().expect("isolated supervisor state");
        let snapshot = studio_runtime_services::diagnostics::SupervisorDiagnosticsPort::snapshot(
            &SupervisorDiagnostics {
                mode: true,
                fallback: false,
                notice: None,
                state_dir: directory.path().join("missing-supervisor"),
            },
        );
        assert!(
            snapshot
                .health
                .is_some_and(|value| value["error"].is_string())
        );
    }

    fn supervisor_health_status(state: &std::path::Path) -> studio_runtime_process::HealthResult {
        studio_runtime_process::HealthResult {
            protocol: studio_runtime_process::SUPERVISOR_PROTOCOL_VERSION,
            state_dir: state.display().to_string(),
            handles: vec![
                serde_json::from_value(json!({
                    "id":"account-fixture", "pid":4123, "signature":"exact-provider-signature",
                    "startTime":"fixture-process-start", "jobIdentity":"fixture-job",
                    "sequence":19, "acknowledged":17, "bufferedBytes":128,
                    "backpressure":false, "stdoutReaderAlive":true, "stdoutReaderError":null,
                    "persistenceErrors":{}
                }))
                .expect("complete supervisor handle"),
            ],
            journal_limit_bytes: 0,
            output_limit_bytes_per_handle: 0,
            durability: studio_runtime_process::Durability::SqliteFullSyncPerRequest,
            recovery: studio_runtime_process::RecoveryStatus {
                degraded: false,
                fallback_ready: false,
                blocked: None,
                extra: Default::default(),
            },
            extra: Default::default(),
        }
    }

    #[test]
    fn supervisor_health_accepts_current_protocol_and_preserves_exact_handles() {
        let fixture = tempfile::tempdir().expect("supervisor health fixture");
        let state = fixture.path().join("state");
        let status = supervisor_health_status(&state);
        let expected = serde_json::to_value(status.clone()).expect("health document");
        assert_eq!(super::supervisor_health_payload(status, &state), expected);
    }

    #[test]
    fn supervisor_health_rejects_incompatible_or_degraded_status() {
        let fixture = tempfile::tempdir().expect("supervisor health fixture");
        let state = fixture.path().join("state");
        let status = supervisor_health_status(&state);
        for protocol in [
            studio_runtime_process::SUPERVISOR_PROTOCOL_VERSION - 1,
            studio_runtime_process::SUPERVISOR_PROTOCOL_VERSION + 1,
        ] {
            let mut incompatible = status.clone();
            incompatible.protocol = protocol;
            assert_eq!(
                super::supervisor_health_payload(incompatible, &state)["error"],
                "The supervisor identity is incompatible."
            );
        }
        assert_eq!(
            super::supervisor_health_payload(status.clone(), &state.join("foreign"))["error"],
            "The supervisor identity is incompatible."
        );
        for blocked in [false, true] {
            let mut degraded = status.clone();
            degraded.recovery.degraded = !blocked;
            degraded.recovery.blocked = blocked.then(|| "fixture recovery block".into());
            assert_eq!(
                super::supervisor_health_payload(degraded, &state)["error"],
                "Supervisor crash recovery needs operator attention."
            );
        }
    }

    #[tokio::test]
    async fn queue_fields_count_matching_persisted_work() {
        let directory = tempfile::tempdir().expect("isolated diagnostics state");
        let store = studio_runtime_store::RuntimeStore::open(Some(directory.path()))
            .await
            .expect("open diagnostics store");
        store
            .transaction_with(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES('agent-a',?1)",
                    [serde_json::json!({"id":"agent-a","accountKey":"account-a","provider":studio_runtime_schema::ProviderKind::Codex}).to_string()],
                )?;
                tx.execute(
                    "INSERT INTO runtime_tool_requests(id,record) VALUES('callback-a',?1)",
                    [json!({"id":"callback-a","kind":studio_runtime_schema::persistence::ToolRequestKind::ProviderCallback,"status":studio_runtime_schema::persistence::ToolRequestStatus::Queued,"payload":{"accepted":{"account":"account-a"}}}).to_string()],
                )?;
                tx.execute(
                    "INSERT INTO runtime_tool_requests(id,record) VALUES('tool-a',?1)",
                    [json!({"id":"tool-a","agent":"agent-a","kind":studio_runtime_schema::persistence::ToolRequestKind::ManagedTool,"status":studio_runtime_schema::persistence::ToolRequestStatus::Queued,"stage":"queued"}).to_string()],
                )?;
                Ok::<(), studio_runtime_store::StoreError>(())
            })
            .await
            .expect("seed durable pending work");
        let accounts = vec!["account-a".to_owned(), "account-b".to_owned()];
        let (counts, _idle) = store
            .read_async("test", move |connection| {
                account_queue_counts(connection, &accounts)
            })
            .await
            .expect("queue counts");
        assert_eq!(counts, vec![(1, 1), (0, 0)]);
    }
}
