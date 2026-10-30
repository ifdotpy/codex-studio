//! Claude session reads and durable settings/rollback controls.
use super::{
    extractors::{ApiJson, QueryParams},
    manifest::{OperationStatus, RouteGroup, get, post},
};
use crate::{error::ApiError, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_providers::ProviderKind;
use studio_runtime_services::claude_controls::{
    ControlError,
    service::{
        ControlBinding, ControlFuture, ControlInvocation, ControlReply, ControlSession, Controls,
    },
    store::StoredControls,
};
use studio_runtime_store::{
    AgentId, AgentRepository, RuntimeStore, workspace_operations::WorkspaceOperationsRepository,
};

// The bridge may include private diagnostic fields; deserialize only the
// evidence that this route is allowed to expose.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct RepairEvidence {
    status: String,
    skipped_rows: u64,
    invalid_sequence: bool,
    scope_confirmed: Option<bool>,
    native_history_confirmed: Option<bool>,
}

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/claude/repair-report",
            OperationStatus::Implemented,
            repair_report,
        ),
        post("/api/claude/session", OperationStatus::Implemented, session),
        post(
            "/api/claude/file-backups",
            OperationStatus::Implemented,
            file_backups,
        ),
    ])
}

async fn repair_report(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<crate::generated::ClaudeRepairReport>, ApiError> {
    use studio_runtime_services::claude_controls::service::ControlScope;
    let id = query
        .nonempty_str("id")
        .ok_or_else(|| ApiError::validation(vec![json!("query"), json!("id")]))?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let (agent, _) = load_agent(store, id.to_owned()).await?;
    require_claude_account(&state, &agent).await?;
    if agent.get("deletedAt").is_some_and(|v| !v.is_null()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "This conversation was deleted",
        ));
    }
    let Some(thread) = state_thread(&agent) else {
        return super::generated_json(json!({"status":"none","skippedRows":0,"invalidSequence":false,"scopeConfirmed":null,"nativeHistoryConfirmed":null})).map(Json);
    };
    let account = agent
        .pointer("/lazyAccountTransfer/sourceAccountKey")
        .and_then(Value::as_str)
        .or_else(|| agent.get("accountKey").and_then(Value::as_str))
        .unwrap_or("default");
    let session = state
        .account_session
        .as_ref()
        .ok_or_else(ApiError::internal)?
        .claude_control_session(account)
        .map_err(session_error)?;
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        session.call_scoped(
            ControlScope {
                agent: id.to_owned(),
                epoch: agent["epoch"].as_u64().unwrap_or(0),
                request_id: None,
            },
            ControlInvocation::RepairReport {
                thread_id: thread.to_owned(),
            },
        ),
    )
    .await
    .map_err(|_| {
        ApiError::new(
            StatusCode::GATEWAY_TIMEOUT,
            "Could not read the saved repair report",
        )
    })?
    .map_err(control_error)?;
    let ControlReply::RepairReport(result) = result else {
        return Err(ApiError::internal());
    };
    let evidence: RepairEvidence =
        serde_json::from_value(result).map_err(|_| ApiError::internal())?;
    if evidence.skipped_rows > studio_runtime_schema::shared_values::MAX_SAFE_INTEGER as u64 {
        return Err(ApiError::internal());
    }
    let safe = json!({
        "status": evidence.status, "skippedRows": evidence.skipped_rows,
        "invalidSequence": evidence.invalid_sequence,
        "scopeConfirmed": evidence.scope_confirmed,
        "nativeHistoryConfirmed": evidence.native_history_confirmed,
    });
    super::generated_json(safe).map(Json)
}

async fn session(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::ClaudeSessionApiClaudeSessionPostRequest>,
) -> Result<Json<Value>, ApiError> {
    use crate::generated::{
        ClaudeSessionApiClaudeSessionPostRequest as SessionRequest, ClaudeSessionStateRequestAction,
    };
    let id = match &body {
        SessionRequest::ClaudeSessionSettingsRequest(request)
        | SessionRequest::ClaudeSessionRollbackRequest(request) => request.id(),
        SessionRequest::ClaudeSessionCommandRequest(request) => &request.id,
        SessionRequest::ClaudeSessionStateRequest(request) => &request.id,
        SessionRequest::ClaudeSessionStopTaskRequest(request) => &request.id,
    };
    if id.is_empty() {
        return Err(ApiError::validation(vec![json!("body"), json!("id")]));
    }
    let id = id.to_owned();
    let is_state = matches!(&body, SessionRequest::ClaudeSessionStateRequest(request) if request.action == ClaudeSessionStateRequestAction::State);
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let (agent, operation) = load_agent(store.clone(), id.clone()).await?;
    require_claude_account(&state, &agent).await?;
    let snapshot_operation = match &body {
        SessionRequest::ClaudeSessionCommandRequest(_) => None,
        SessionRequest::ClaudeSessionSettingsRequest(_)
        | SessionRequest::ClaudeSessionRollbackRequest(_)
        | SessionRequest::ClaudeSessionStateRequest(_)
        | SessionRequest::ClaudeSessionStopTaskRequest(_) => operation.clone(),
    };
    let snapshot = studio_runtime_services::claude_controls::ControlSnapshot::from_stored(
        agent.clone(),
        snapshot_operation.as_ref(),
    )
    .map_err(control_error)?;
    let account = if is_state {
        agent
            .get("lazyAccountTransfer")
            .and_then(|lazy| lazy.get("sourceAccountKey"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                agent
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
            })
    } else {
        agent
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
    };
    let no_thread = agent
        .get("threadId")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty);
    let local_only = (is_state && state_thread(&agent).is_none())
        || (matches!(&body, SessionRequest::ClaudeSessionSettingsRequest(_)) && no_thread);
    if matches!(
        &body,
        SessionRequest::ClaudeSessionRollbackRequest(_)
            | SessionRequest::ClaudeSessionStopTaskRequest(_)
    ) && no_thread
    {
        return Err(control_error(ControlError::Request));
    }
    // Native commands use the message service and need no provider control session.
    let controls = || -> Result<Controls, ApiError> {
        let session: Arc<dyn ControlSession> = if local_only {
            Arc::new(LocalControlSession {
                binding: ControlBinding::new(account.to_owned(), "local-no-thread".into(), 1)
                    .map_err(control_error)?,
            })
        } else {
            let broker = state
                .account_session
                .as_ref()
                .ok_or_else(|| ApiError::dependency("provider-session/claude-controls"))?;
            broker
                .claude_control_session(account)
                .map_err(session_error)?
        };
        Ok(Controls::new(
            Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
            session,
        ))
    };
    let result = match body {
        SessionRequest::ClaudeSessionCommandRequest(request) => {
            let body = serde_json::to_value(request).map_err(|_| ApiError::internal())?;
            return command(state, body).await;
        }
        SessionRequest::ClaudeSessionSettingsRequest(request)
        | SessionRequest::ClaudeSessionRollbackRequest(request) => {
            controls()?.execute(request).await
        }
        request @ (SessionRequest::ClaudeSessionStateRequest(_)
        | SessionRequest::ClaudeSessionStopTaskRequest(_)) => {
            let body = serde_json::to_value(request).map_err(|_| ApiError::internal())?;
            controls()?.read_or_stop_snapshot(&snapshot, &body).await
        }
    };
    state.event_wake.notify_one();
    let mut result = result.map_err(control_error)?;
    if is_state {
        let workspace = studio_runtime_services::checkpoints::capture_context(&store, &id)
            .await
            .map_err(|_| ApiError::internal())?;
        let reason = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/claude-controls/restore-availability",
            move || {
                studio_runtime_services::checkpoints::ensure_workspace_idle(&workspace, None)
                    .err()
                    .map(|error| error.to_string())
            },
        )
        .await
        .map_err(|_| ApiError::internal())?;
        if let Some(reason) = reason
            && let Some(turns) = result.get_mut("turns").and_then(Value::as_array_mut)
        {
            for turn in turns {
                turn["canRestoreFiles"] = json!(false);
                turn["restoreFilesUnavailableReason"] = json!(reason);
            }
        }
    }
    Ok(Json(result))
}

async fn file_backups(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::ClaudeFileBackupsRequest>,
) -> Result<Json<crate::generated::ClaudeFileBackupsResponse>, ApiError> {
    tokio::spawn(async move {
        use studio_runtime_services::claude_controls::service::ControlScope;
        use studio_runtime_store::ClaudeControlRepository;
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let _execution = studio_runtime_services::checkpoints::RestoreExecution::acquire(format!(
            "{}:{}",
            store.state_dir().display(),
            body.id
        ))
        .map_err(|_| control_error(ControlError::Busy))?;
        let id = body.id.clone();
        let (agent, settled) = store
            .read_async("claude/file-backups", move |tx| {
                let agent = AgentRepository::get(tx, &AgentId(id))?
                    .ok_or_else(|| studio_runtime_store::StoreError::InvalidStoredRecord {
                        object: "agent",
                        id: "missing".into(),
                    })?
                    .value();
                let settled = ClaudeControlRepository::settled_file_rewinds(tx, &agent)?;
                Ok::<_, studio_runtime_store::StoreError>((agent, settled))
            })
            .await
            .map_err(|_| ApiError::internal())?;
        require_claude_account(&state, &agent).await?;
        let snapshot = studio_runtime_services::claude_controls::ControlSnapshot::from_records(
            agent.clone(),
            None,
        )
        .map_err(control_error)?;
        let thread = agent["threadId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| control_error(ControlError::Request))?;
        let account = agent["accountKey"].as_str().unwrap_or("default");
        let session = state
            .account_session
            .as_ref()
            .ok_or_else(ApiError::internal)?
            .claude_control_session(account)
            .map_err(session_error)?;
        let result = session
            .call_scoped(
                ControlScope {
                    agent: body.id,
                    epoch: snapshot.epoch(),
                    request_id: Some(body.request_id.clone()),
                },
                ControlInvocation::FileBackups {
                    thread_id: thread.to_owned(),
                    settled_request_ids: if body.release { Some(settled) } else { None },
                    request_id: body.request_id,
                },
            )
            .await
            .map_err(control_error)?;
        let ControlReply::FileBackups(result) = result else {
            return Err(ApiError::internal());
        };
        super::generated_json::<crate::generated::ClaudeFileBackupsResponse>(result).map(Json)
    })
    .await
    .map_err(|_| ApiError::internal())?
}

async fn require_claude_account(state: &HttpState, agent: &Value) -> Result<(), ApiError> {
    let account_key = agent
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default")
        .to_owned();
    let driver = state.account_driver.clone();
    let policy = state.account_policy.clone();
    let accounts = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/claude_controls/account",
        move || match driver {
            Some(driver) => driver.snapshot().map_err(|_| ApiError::internal()),
            None => policy
                .map(|policy| policy.accounts())
                .ok_or_else(ApiError::internal),
        },
    )
    .await
    .map_err(|_| ApiError::internal())??;
    let provider = accounts
        .accounts
        .iter()
        .find(|account| account.id.as_str() == Some(account_key.as_str()))
        .and_then(|account| account.provider.as_str());
    if provider != Some(ProviderKind::Claude.key()) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Select a Claude Code chat",
        ));
    }
    Ok(())
}

fn state_thread(agent: &Value) -> Option<&str> {
    let transfer = agent
        .get("lazyAccountTransfer")
        .filter(|transfer| transfer.as_object().is_some_and(|value| !value.is_empty()));
    if let Some(transfer) = transfer {
        transfer
            .get("sourceThreadId")
            .and_then(Value::as_str)
            .filter(|thread| !thread.is_empty())
    } else {
        agent
            .get("threadId")
            .and_then(Value::as_str)
            .filter(|thread| !thread.is_empty())
    }
}

struct LocalControlSession {
    binding: ControlBinding,
}

impl ControlSession for LocalControlSession {
    fn binding(&self) -> ControlBinding {
        self.binding.clone()
    }

    fn call_scoped(
        &self,
        _scope: studio_runtime_services::claude_controls::service::ControlScope,
        _call: ControlInvocation,
    ) -> ControlFuture<'_, ControlReply> {
        Box::pin(async { Err(ControlError::Request) })
    }
}

async fn load_agent(
    store: Arc<RuntimeStore>,
    id: String,
) -> Result<
    (
        Value,
        Option<studio_runtime_store::WorkspaceOperationRecord>,
    ),
    ApiError,
> {
    let loaded = store
        .read_async("runtime-http/routes/claude_controls", move |tx| {
            let Some(agent) = AgentRepository::get(tx, &AgentId::from(id.as_str()))? else {
                return Ok(None);
            };
            let value = agent.value();
            let operation = value
                .get("workspaceReservationId")
                .and_then(Value::as_str)
                .filter(|id| id.starts_with("claude-control:"))
                .map(|id| WorkspaceOperationsRepository::operation(tx, id))
                .transpose()?
                .flatten();
            Ok::<_, studio_runtime_store::StoreError>(Some((value, operation)))
        })
        .await
        .map_err(|_| ApiError::internal())?;
    loaded.ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent"))
}

async fn command(state: HttpState, body: Value) -> Result<Json<Value>, ApiError> {
    let command = body
        .get("command")
        .and_then(Value::as_str)
        .and_then(|command| {
            studio_runtime_services::claude_controls::service::native_command(command).ok()
        })
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "Supply one explicit Claude command",
            )
        })?;
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ApiError::validation(vec![json!("body"), json!("id")]))?;
    let request_id = body
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            ApiError::new(StatusCode::BAD_REQUEST, "Supply a command request identity")
        })?;
    let message = serde_json::from_value::<crate::generated::ManagedMessageBody>(json!({
        "id":request_id,"room":id,"text":command,"delivery":studio_runtime_schema::DeliveryMoment::CurrentTurn
    }))
    .map_err(|_| ApiError::validation(vec![json!("body")]))?;
    let receipt = super::messages::post_message(State(state), ApiJson(message)).await?;
    serde_json::to_value(receipt.0)
        .map(Json)
        .map_err(|_| ApiError::internal())
}

fn control_error(error: ControlError) -> ApiError {
    match error {
        ControlError::Invalid(_) | ControlError::Unavailable => ApiError::internal(),
        ControlError::Request => {
            ApiError::new(StatusCode::BAD_REQUEST, "Invalid Claude control request")
        }
        ControlError::Busy => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Pause Claude before changing its session",
        ),
        ControlError::Changed => ApiError::new(
            StatusCode::CONFLICT,
            "Claude session changed before the action",
        ),
        ControlError::Conflict => ApiError::new(
            StatusCode::CONFLICT,
            "This request identity has different content",
        ),
        ControlError::Unknown => ApiError::new(
            StatusCode::CONFLICT,
            "Claude control outcome is unknown; inspect workspace recovery",
        ),
        ControlError::RewindNotApplied(reason) | ControlError::SettingsNotApplied(reason) => {
            ApiError::new(StatusCode::BAD_REQUEST, reason)
        }
        ControlError::RewindRejected(reason) => ApiError::new(
            StatusCode::CONFLICT,
            format!("Claude file restore requires recovery: {reason}"),
        ),
        ControlError::NotSent => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Claude control request was not sent",
        ),
    }
}

fn session_error(
    error: studio_runtime_services::account_session_runtime::AccountSessionError,
) -> ApiError {
    use studio_runtime_services::account_session_runtime::AccountSessionError as Error;
    match error {
        Error::Expired => ApiError::new(StatusCode::GONE, error.to_string()),
        Error::Requires(dependency) => ApiError::dependency(dependency),
        Error::Refused(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        Error::InvalidResponse(_) | Error::Persistence | Error::Unknown => ApiError::internal(),
    }
}

inventory::submit! {crate::routes::manifest::RouteModuleRegistration{name:"claude_controls",routes:Some(routes),dependencies:&[]}}
