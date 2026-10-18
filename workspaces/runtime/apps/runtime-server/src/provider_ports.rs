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
//! counted from unresolved durable callback receipts and tool requests from
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

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_providers::Method;
use studio_runtime_services::{
    account_driver::AccountDriver,
    account_session_runtime::{
        AccountSessionError, AccountSessionPort, ConnectionScope, ProviderRequest,
        ProviderResponse, RequestCertainty, SessionReceipt,
    },
    account_worker::retained::RequestActor,
    provider_versions::{ActiveProviderConnections, ProviderProfile},
    title_generation::{
        TitleReceipt, TitleRequest, TitleRequestPort, WriteOutcome, normalize_title,
    },
};
use studio_runtime_store::RuntimeStore;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const MAX_POLLS: u32 = 64;
static DIAGNOSTIC_OPERATION: AtomicU64 = AtomicU64::new(0);

/// Captures live provider diagnostics with bounded, read-only requests through
/// the retained session. The HTTP route invokes this on its blocking pool.
pub(crate) struct ProviderDiagnostics {
    pub accounts: Arc<AccountDriver>,
    pub session: Arc<dyn AccountSessionPort>,
    pub store: Arc<RuntimeStore>,
}

/// Runtime-server knows whether supervisor mode was requested and selected.
/// The current host exposes no health feed or recovery notice, so those fields
/// stay absent instead of implying a health check ran.
pub(crate) struct SupervisorDiagnostics {
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
    if status.protocol != 1 || expected_state.to_str() != Some(status.state_dir.as_str()) {
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
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            let local = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-server/provider-diagnostics-local",
                move || {
                    let registry = accounts
                        .snapshot()
                        .map_err(|_| AccountSessionError::Unknown)?;
                    let mut active = registry
                        .accounts
                        .iter()
                        .filter_map(|account| {
                            let key = account.get("id")?.as_str()?.to_owned();
                            let provider =
                                studio_runtime_providers::ProviderKind::of_record(account).ok()?.key().to_owned();
                            let scope = session.current_scope(&key)?;
                            (scope.account == key && !scope.connection.is_empty())
                                .then_some((key, provider, scope))
                        })
                        .collect::<Vec<_>>();
                    active.sort_by(|left, right| left.0.cmp(&right.0));
                    let queues = active
                        .iter()
                        .enumerate()
                        .map(|(index, (account, _, _))| {
                            let alias = format!("account{}", index + 1);
                            let (callbacks, tool_requests) = account_queue_counts(&store, account);
                            (alias, callbacks, tool_requests)
                        })
                        .collect::<Vec<_>>();
                    let idle_limit_seconds = store
                        .read()
                        .ok()
                        .and_then(|connection| {
                            studio_runtime_store::OrchestrationSettingsRepository::idle_session_unload(
                                &connection,
                            )
                            .ok()
                        })
                        .map_or(0, |delay| delay.map_or(0, |delay| delay.as_secs()));
                    Ok::<_, AccountSessionError>((active, queues, idle_limit_seconds))
                },
            )
            .await;
            let Ok(Ok((active, local_queues, idle_limit_seconds))) = local else {
                return ProviderDiagnosticsSnapshot {
                    native_accounts: json!({}),
                    studio_loaded_threads: 0,
                    queues: serde_json::Map::new(),
                };
            };
            let mut native_accounts = serde_json::Map::new();
            let mut queues = serde_json::Map::new();
            let mut loaded_threads = 0_u64;
            let deadline = Instant::now() + Duration::from_secs(5);
            for (index, (_, provider, scope)) in active.iter().enumerate() {
                let alias = format!("account{}", index + 1);
                let method = if provider == "claude" {
                    Method::ClaudeDiagnostics
                } else {
                    Method::ThreadLoadedList
                };
                let result = provider_read(
                    self.session.as_ref(),
                    scope,
                    None,
                    method,
                    json!({}),
                    deadline,
                )
                .await;
                match result {
                    Some(value) if provider == "claude" => {
                        let mut row = serde_json::Map::new();
                        row.insert("provider".to_owned(), json!("claude"));
                        if let Some(fields) = value.as_object() {
                            row.extend(fields.clone());
                        }
                        // The server's idle sweep closes idle sessions; 0 never closes them.
                        row.insert("idleLimitSeconds".to_owned(), json!(idle_limit_seconds));
                        native_accounts.insert(alias.clone(), Value::Object(row));
                    }
                    Some(value) => {
                        let count = value
                            .get("data")
                            .and_then(Value::as_array)
                            .map_or(0, Vec::len);
                        loaded_threads = loaded_threads.saturating_add(count as u64);
                        native_accounts.insert(
                            alias.clone(),
                            json!({"provider":"codex", "loadedThreads":count}),
                        );
                    }
                    None => {
                        native_accounts.insert(
                            alias.clone(),
                            json!({"error":"ProviderDiagnosticsUnavailable"}),
                        );
                    }
                }
            }
            for (alias, callbacks, tool_requests) in local_queues {
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

fn account_queue_counts(store: &RuntimeStore, account: &str) -> (u64, u64) {
    let Ok(connection) = store.read() else {
        return (0, 0);
    };
    let callbacks = connection
        .query_row(
            "SELECT COUNT(*) FROM runtime_operation_receipts submitted
             WHERE submitted.id>=?1 AND submitted.id<?2
               AND substr(submitted.id,-10)=':submitted'
               AND NOT EXISTS (SELECT 1 FROM runtime_operation_receipts settled
                 WHERE settled.id=substr(submitted.id,1,length(submitted.id)-10)||':written'
                    OR settled.id=substr(submitted.id,1,length(submitted.id)-10)||':unknown')",
            [
                format!("account-worker:callback:{account}:"),
                format!("account-worker:callback:{account}:~"),
            ],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or_default()
        .max(0) as u64;
    let tool_requests = connection
        .query_row(
            "SELECT COUNT(*) FROM runtime_tool_requests r
             JOIN runtime_agents a ON a.id=json_extract(r.record,'$.agent')
             WHERE COALESCE(json_extract(a.record,'$.accountKey'),'default')=?1
               AND json_extract(r.record,'$.stage') IN ('queued','running')",
            [account],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or_default()
        .max(0) as u64;
    (callbacks, tool_requests)
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
        "diagnostics:{}:{}:{}",
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
        || receipt.certainty != RequestCertainty::Written
    {
        return None;
    }
    match receipt.result? {
        ProviderResponse::Reply(value) => Some(value),
        ProviderResponse::Rejected(_) => None,
    }
}

/// Builds provider-version inputs from registry metadata joined to the same
/// retained live session broker used for provider requests. Registry-only and
/// offline accounts are deliberately excluded.
pub(crate) struct LiveProviderConnections {
    pub accounts: Arc<AccountDriver>,
    pub session: Arc<dyn AccountSessionPort>,
    pub session_binaries:
        Arc<std::sync::Mutex<std::collections::BTreeMap<String, SessionExecutables>>>,
    pub changes: tokio::sync::watch::Receiver<u64>,
}

/// The provider executable a retained account session was launched with.
#[derive(Clone, Debug)]
pub(crate) struct SessionExecutables {
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
                let account_key = account.get("id")?.as_str()?.to_owned();
                let scope = self.session.current_scope(&account_key)?;
                if scope.account != account_key || scope.connection.is_empty() {
                    return None;
                }
                let kind = studio_runtime_providers::ProviderKind::of_record(account).ok()?;
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
                        studio_runtime_services::account_driver::claude::resolve_claude_binary(
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
pub(crate) struct ProviderTitleRequestPort {
    session: Arc<dyn AccountSessionPort>,
}

impl ProviderTitleRequestPort {
    pub(crate) fn new(session: Arc<dyn AccountSessionPort>) -> Self {
        Self { session }
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
            let claude = request.provider.as_deref() == Some("claude");
            let operation_root = operation_root(&request.request_id);
            let start_operation = format!("{operation_root}:thread");
            // Cancellation or process loss during a submitted write must leave
            // the same cwd available when the durable receipt is inspected.
            home.retain();
            let thread_params = if claude {
                json!({"cwd":home.path(),"model":request.model,"ephemeral":true})
            } else {
                json!({
                    "cwd": home.path(),
                    "model": request.model,
                    "ephemeral": true,
                    "approvalPolicy": "never",
                    "sandbox": "read-only",
                })
            };
            let started = self
                .call(
                    &request.account_key,
                    &start_operation,
                    Some(actor.clone()),
                    Method::ThreadStart,
                    thread_params,
                    deadline,
                    None,
                )
                .await;
            let Some(started) = started else {
                home.retain();
                return fallback();
            };
            match started.certainty {
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
            if matches!(started.result.as_ref(), Some(ProviderResponse::Rejected(_))) {
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
                .and_then(|value| value.pointer("/thread/id"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty() && value.len() <= 1024)
                .map(str::to_owned)
            else {
                home.retain();
                return fallback();
            };
            let mut turn_params = json!({
                "threadId": thread_id,
                "input": [{"type": "text", "text": request.prompt}],
                "model": request.model,
            });
            if claude {
                turn_params["clientUserMessageId"] = json!(format!("{operation_root}:turn"));
            }
            if let Some(effort) = request.effort.as_deref() {
                turn_params["effort"] = json!(effort);
            }
            let turn_operation = format!("{operation_root}:turn");
            let turn = self
                .call(
                    &request.account_key,
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
            match turn.certainty {
                RequestCertainty::NotSent => {
                    // Keep the exact cwd for a retry that inspects ThreadStart.
                    return title_receipt(&request, WriteOutcome::NotSent, None);
                }
                RequestCertainty::Unknown => {
                    home.retain();
                    return fallback();
                }
                RequestCertainty::Written => {}
            }
            if matches!(turn.result.as_ref(), Some(ProviderResponse::Rejected(_))) {
                home.release();
                return title_receipt(&request, WriteOutcome::Written, None);
            }
            let turn_id = turn
                .pipe_response_value()
                .and_then(|value| value.pointer("/turn/id"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            if turn_id.is_none() {
                home.retain();
                return fallback();
            }
            for poll in 0..MAX_POLLS {
                if Instant::now() >= deadline {
                    home.retain();
                    return fallback();
                }
                let operation = format!("{operation_root}:poll:{poll}");
                let listed = self
                    .call(
                        &request.account_key,
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
                    && listed.certainty == RequestCertainty::Written
                    && let Some(turns) = listed
                        .pipe_response_value()
                        .and_then(|value| value.get("data"))
                        .and_then(Value::as_array)
                {
                    let completed = turns.iter().find(|candidate| {
                        let id_matches = turn_id.as_deref().is_none_or(|id| {
                            candidate.get("id").and_then(Value::as_str) == Some(id)
                        });
                        id_matches
                            && candidate.get("status").and_then(Value::as_str).is_some_and(
                                |status| {
                                    matches!(
                                        status,
                                        "completed" | "failed" | "interrupted" | "cancelled"
                                    )
                                },
                            )
                    });
                    if let Some(turn) = completed {
                        let raw_title =
                            turn.get("items")
                                .and_then(Value::as_array)
                                .and_then(|items| {
                                    items.iter().rev().find_map(|item| {
                                        (item.get("type").and_then(Value::as_str)
                                            == Some("agentMessage")
                                            && item.get("phase").and_then(Value::as_str)
                                                == Some("final_answer"))
                                        .then(|| item.get("text").and_then(Value::as_str))
                                        .flatten()
                                    })
                                });
                        let title = raw_title.and_then(|value| normalize_title(value).ok());
                        home.release();
                        return title_receipt(&request, WriteOutcome::Written, title);
                    }
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
        account: &str,
        operation: &str,
        actor: Option<RequestActor>,
        method: Method,
        params: Value,
        deadline: Instant,
        expected_scope: Option<&ConnectionScope>,
    ) -> Option<SessionReceipt> {
        let session = Arc::clone(&self.session);
        let expected_account = account.to_owned();
        let expected_operation = operation.to_owned();
        let request = ProviderRequest {
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
        match self.result.as_ref()? {
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
    use serde_json::json;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime};
    use studio_runtime_providers::Method;
    use studio_runtime_services::account_session_runtime::{
        AccountSessionFuture, AccountSessionPort, ConnectionScope, ProviderRequest,
        ProviderResponse, RequestCertainty, SessionReceipt,
    };
    use studio_runtime_services::account_worker::retained::RequestActor;
    use studio_runtime_services::title_generation::{TitleRequest, TitleRequestPort, WriteOutcome};

    struct Session {
        requests: Mutex<Vec<ProviderRequest>>,
        unknown_start: bool,
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
                requests.push(request);
                let certainty = if self.unknown_start && method == Method::ThreadStart {
                    RequestCertainty::Unknown
                } else {
                    RequestCertainty::Written
                };
                let result = match method {
                    Method::ThreadStart => Some(ProviderResponse::Reply(
                        json!({"thread":{"id":"title-thread"}}),
                    )),
                    Method::TurnStart => {
                        Some(ProviderResponse::Reply(json!({"turn":{"id":"title-turn"}})))
                    }
                    Method::ThreadTurnsList => Some(ProviderResponse::Reply(json!({"data":[{
                        "id":"title-turn", "status":"completed", "items":[{
                            "type":"agentMessage", "phase":"final_answer", "text":"A Useful Title"
                        }]
                    }]}))),
                    _ => None,
                };
                Ok(SessionReceipt {
                    scope,
                    operation,
                    ticket: None,
                    certainty,
                    result,
                    not_sent_reason: None,
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
            unknown_start: false,
        });
        let port = ProviderTitleRequestPort::new(session.clone());
        let request = TitleRequest {
            request_id: "title-request-fixture".into(),
            agent_id: "agent-fixture".into(),
            epoch: 3,
            account_key: "account-fixture".into(),
            provider: Some("codex".into()),
            model: "gpt-mini".into(),
            effort: Some("low".into()),
            prompt: "Write a title".into(),
            deadline: SystemTime::now() + Duration::from_secs(10),
        };
        let receipt = port.request(request).await;
        assert_eq!(receipt.outcome, WriteOutcome::Written);
        assert_eq!(receipt.title.as_deref(), Some("A Useful Title"));
        let calls = session.requests.lock().expect("fixture request list");
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

    #[tokio::test]
    async fn unknown_title_start_is_never_followed_by_another_write() {
        let session = Arc::new(Session {
            requests: Mutex::new(Vec::new()),
            unknown_start: true,
        });
        let port = ProviderTitleRequestPort::new(session.clone());
        let receipt = port
            .request(TitleRequest {
                request_id: "title-request-unknown".into(),
                agent_id: "agent-fixture".into(),
                epoch: 3,
                account_key: "account-fixture".into(),
                provider: Some("codex".into()),
                model: "gpt-mini".into(),
                effort: None,
                prompt: "Write a title".into(),
                deadline: SystemTime::now() + Duration::from_secs(10),
            })
            .await;
        assert_eq!(receipt.outcome, WriteOutcome::Unknown);
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
    async fn claude_title_uses_stable_message_identity_on_retained_session() {
        let session = Arc::new(Session {
            requests: Mutex::new(Vec::new()),
            unknown_start: false,
        });
        let port = ProviderTitleRequestPort::new(session.clone());
        let receipt = port
            .request(TitleRequest {
                request_id: "claude-title-request".into(),
                agent_id: "agent-fixture".into(),
                epoch: 9,
                account_key: "claude-account".into(),
                provider: Some("claude".into()),
                model: "claude-sonnet".into(),
                effort: Some("low".into()),
                prompt: "Write a title".into(),
                deadline: SystemTime::now() + Duration::from_secs(10),
            })
            .await;
        assert_eq!(receipt.outcome, WriteOutcome::Written);
        let calls = session.requests.lock().expect("fixture request list");
        assert_eq!(calls[0].method, Method::ThreadStart);
        assert_eq!(calls[1].method, Method::TurnStart);
        assert_eq!(calls[0].params["ephemeral"], true);
        assert!(calls[0].params["cwd"].as_str().is_some());
        assert_eq!(calls[0].params["model"], "claude-sonnet");
        assert_eq!(
            calls[1].params["clientUserMessageId"],
            json!(format!(
                "{}:turn",
                super::operation_root("claude-title-request")
            ))
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

    #[test]
    fn supervisor_health_rejects_incompatible_or_degraded_status() {
        let state = PathBuf::from("/tmp/studio-supervisor-test");
        let status = studio_runtime_process::HealthResult {
            protocol: 1,
            state_dir: state.display().to_string(),
            handles: Vec::new(),
            journal_limit_bytes: 0,
            output_limit_bytes_per_handle: 0,
            durability: studio_runtime_process::Durability::SqliteFullSyncPerRequest,
            recovery: studio_runtime_process::RecoveryStatus {
                degraded: false,
                fallback_ready: true,
                blocked: None,
                extra: Default::default(),
            },
            extra: Default::default(),
        };
        assert!(super::supervisor_health_payload(status.clone(), &state)["error"].is_null());
        let mut degraded = status;
        degraded.recovery.degraded = true;
        assert_eq!(
            super::supervisor_health_payload(degraded, &state)["error"],
            "Supervisor crash recovery needs operator attention."
        );
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
                    [r#"{"id":"agent-a","accountKey":"account-a"}"#],
                )?;
                tx.execute(
                    "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'submitted','true')",
                    ["account-worker:callback:account-a:1:3:submitted"],
                )?;
                tx.execute(
                    "INSERT INTO runtime_tool_requests(id,record) VALUES('tool-a',?1)",
                    [r#"{"id":"tool-a","agent":"agent-a","stage":"queued"}"#],
                )?;
                Ok::<(), studio_runtime_store::StoreError>(())
            })
            .await
            .expect("seed durable pending work");
        assert_eq!(account_queue_counts(&store, "account-a"), (1, 1));
        assert_eq!(account_queue_counts(&store, "account-b"), (0, 0));
    }
}
