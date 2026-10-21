//! Claude session reads and durable settings/rollback controls.
use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, post},
};
use crate::{error::ApiError, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_services::claude_controls::{
    ControlError, ControlRequest,
    service::{ControlBinding, ControlFuture, ControlSession, Controls},
    store::StoredControls,
};
use studio_runtime_store::{
    AgentId, AgentRepository, RuntimeStore, workspace_operations::WorkspaceOperationsRepository,
};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/claude/session",
        OperationStatus::Implemented,
        session,
    )])
}

async fn session(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if action == "command" {
        let id = body
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ApiError::validation(vec![json!("body"), json!("id")]))?;
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let (agent, _) = load_agent(store, id.to_owned()).await?;
        require_claude_account(&state, &agent).await?;
        studio_runtime_services::claude_controls::ControlSnapshot::from_records(agent, None)
            .map_err(control_error)?;
        return command(state, body).await;
    }
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| ApiError::validation(vec![json!("body"), json!("id")]))?
        .to_owned();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let (agent, operation) = load_agent(store.clone(), id.clone()).await?;
    require_claude_account(&state, &agent).await?;
    let snapshot = studio_runtime_services::claude_controls::ControlSnapshot::from_records(
        agent.clone(),
        operation.clone(),
    )
    .map_err(control_error)?;
    let control_request = if matches!(action, "settings" | "rollback") {
        Some(ControlRequest::from_value(&body).map_err(control_error)?)
    } else {
        None
    };
    let account = if action == "state" {
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
    let local_only = (action == "state" && state_thread(&agent).is_none())
        || (action == "settings" && no_thread);
    if matches!(action, "rollback" | "stop_task") && no_thread {
        return Err(control_error(ControlError::Request));
    }
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
    let controls = Controls::new(
        Arc::new(StoredControls::new(store.clone(), Arc::new(|_| {}))),
        session,
    );
    let result = match action {
        "settings" | "rollback" => match control_request {
            Some(request) => controls.execute(request).await,
            None => return Err(ApiError::internal()),
        },
        "state" | "commands" | "stop_task" => {
            controls
                .read_or_stop(snapshot.agent(), &body, operation)
                .await
        }
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Unknown Claude action",
            ));
        }
    };
    state.event_wake.notify_one();
    let mut result = result.map_err(control_error)?;
    if action == "state" {
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
        .find(|account| account.get("id").and_then(Value::as_str) == Some(account_key.as_str()))
        .and_then(|account| account.get("provider"))
        .and_then(Value::as_str);
    if provider != Some("claude") {
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

    fn call(&self, _method: &str, _params: Value) -> ControlFuture<'_, Value> {
        Box::pin(async { Err(ControlError::Request) })
    }
}

async fn load_agent(
    store: Arc<RuntimeStore>,
    id: String,
) -> Result<(Value, Option<Value>), ApiError> {
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
        Error::Requires(dependency) => ApiError::dependency(dependency),
        Error::Refused(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        Error::InvalidResponse(_) | Error::Persistence | Error::Unknown => ApiError::internal(),
    }
}

inventory::submit! {crate::routes::manifest::RouteModuleRegistration{name:"claude_controls",routes:Some(routes),dependencies:&[]}}
