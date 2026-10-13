//! Persisted agent creation and management route slice.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use studio_runtime_services::account_driver::{AccountDriverError, AccountStatus};
use studio_runtime_services::agents_create::{self, CreateError, RenameError, SettingsError};
use studio_runtime_services::event_write::python_json;
use studio_runtime_store::{RuntimeStore, SyncDocumentsRepository};

use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) fn routes() -> RouteGroup {
    const DELETE_DEPENDENCY: [Dependency; 1] =
        [Dependency::new("agent-management/conversation-termination")];
    RouteGroup::new(vec![
        post("/api/agents", OperationStatus::Implemented, create_agent),
        post("/api/configure", OperationStatus::Implemented, configure),
        post(
            "/api/conversation",
            OperationStatus::Implemented,
            conversation,
        ),
        post(
            "/api/conversation/delete",
            OperationStatus::partial(&DELETE_DEPENDENCY),
            delete_conversation,
        ),
        post("/api/leads", OperationStatus::Implemented, create_lead),
        post("/api/rename", OperationStatus::Implemented, rename),
    ])
}

const MAX_MUTATION_SYNC_ENTITIES: i64 = 500;
const DEFAULT_LEAD_MODEL: &str = "gpt-6-astra";
const DEFAULT_CONCURRENCY: i64 = 32;
const DEFAULT_MAX_AGENTS: i64 = 64;

async fn rename(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::RenameRequest>,
) -> Result<Json<Value>, ApiError> {
    let id = body.id;
    let request_id = body.request_id.flatten();
    let name = body.name.flatten().map(|name| name.trim().to_owned());
    if name.as_ref().is_some_and(String::is_empty) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "A name must have 1 to 80 characters",
        ));
    }
    if name.is_none() && request_id.is_none() {
        return Err(ApiError::validation(vec![Value::String("body".to_owned())]));
    }
    let signature = rename_signature(&id, name.as_deref());
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        let before_connection = store.read().map_err(|_| ApiError::internal())?;
        let sync_after = SyncDocumentsRepository::entity_sequence(&before_connection)
            .map_err(|_| ApiError::internal())?;
        drop(before_connection);
        let mut result = store
            .transaction_with_error(|transaction| {
                agents_create::rename(
                    transaction,
                    &id,
                    name.as_deref(),
                    request_id.as_deref(),
                    &signature,
                )
            })
            .map_err(rename_error)?;
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let rows = SyncDocumentsRepository::entities_after(
            &connection,
            sync_after,
            MAX_MUTATION_SYNC_ENTITIES + 1,
        )
        .map_err(|_| ApiError::internal())?;
        if !rows.is_empty() {
            result["_syncEntities"] = Value::Array(
                rows.iter()
                    .take(MAX_MUTATION_SYNC_ENTITIES as usize)
                    .map(|row| {
                        serde_json::json!({
                            "id": format!("entity:{}:{}", row.collection, row.id),
                            "seq": row.sequence,
                            "payload": row.payload,
                            "_deleted": row.deleted,
                        })
                    })
                    .collect(),
            );
            let tombstone_floor = SyncDocumentsRepository::entity_tombstone_floor(&connection)
                .map_err(|_| ApiError::internal())?;
            if rows.len() <= MAX_MUTATION_SYNC_ENTITIES as usize && tombstone_floor <= sync_after {
                result["_syncEntitiesAfter"] = serde_json::json!(sync_after);
            }
        }
        Ok::<_, ApiError>(result)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(super::generated_json::<
        crate::generated::RenameResponse,
    >(result)?))
}

fn rename_signature(id: &str, name: Option<&str>) -> String {
    let name = name
        .map(|value| python_json(&Value::String(value.to_owned())))
        .unwrap_or_else(|| "null".to_owned());
    let body = format!(
        "{{\"chat\": {}, \"name\": {name}, \"operation\": \"rename\"}}",
        python_json(&Value::String(id.to_owned()))
    );
    Sha256::digest(body.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn rename_error(error: RenameError) -> ApiError {
    match error {
        RenameError::UnknownAgent => {
            ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent")
        }
        RenameError::Deleted => {
            ApiError::new(StatusCode::BAD_REQUEST, "This conversation was deleted")
        }
        RenameError::Conflict => ApiError::new(
            StatusCode::BAD_REQUEST,
            "This request id has different content",
        ),
        RenameError::RequestIdRequired => {
            ApiError::new(StatusCode::BAD_REQUEST, "A rename request id is required")
        }
        RenameError::TitleGenerationUnavailable => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires provider-session/title-generation",
        ),
        RenameError::InvalidStoredObject { object } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}"),
        ),
        RenameError::Store(_) => ApiError::internal(),
    }
}

async fn create_lead(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    for field in ["model", "effort", "fast_mode", "daybreak_enabled"] {
        if body.get(field).is_some_and(|value| !value.is_null()) {
            return Err(unavailable("provider-session/model-list"));
        }
    }
    if body
        .get("worker_defaults")
        .is_some_and(|value| !value.is_null())
        || body
            .get("review_defaults")
            .is_some_and(|value| !value.is_null())
    {
        return Err(unavailable("worker-driver/settings-catalog"));
    }
    if ["project_id", "project_server_id", "project_folder"]
        .iter()
        .any(|field| body.get(*field).is_some_and(|value| !value.is_null()))
    {
        return Err(unavailable("project-driver/chat-binding"));
    }
    if body.get("previous").is_some_and(|value| !value.is_null())
        || body.get("reuse_empty").and_then(Value::as_bool) == Some(true)
    {
        return Err(unavailable("agent-management/empty-chat-reuse"));
    }
    let workspace_mode =
        body.get("workspaceMode")
            .and_then(Value::as_str)
            .unwrap_or(if cfg!(target_os = "macos") {
                "layr"
            } else {
                "worktree"
            });
    if workspace_mode != "worktree" {
        return Err(unavailable("workspace-state/lead-workspace"));
    }
    if state.account_driver.is_none() {
        return Err(unavailable("account-driver/unavailable"));
    }
    let cwd = project_directory(&state, body.get("cwd"))?;
    let account = account_key(&state, body.get("account_key"), &cwd)?;
    refresh_account(&state, &account)?;
    let id = match body.get("id").and_then(Value::as_str) {
        Some(id) => id.to_owned(),
        None => agents_create::new_id().map_err(create_error)?,
    };
    let yolo = body
        .get("yolo_mode")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let created = unix_time()?;
    let request_signature = signature(&body);
    let record = merge_records([
        json!({
          "id": id,
          "threadId": null,
          "accountKey": account,
          "executionSettingsAccountKey": account,
          "provider": provider(&state, &account),
          "yoloMode": yolo,
          "name": "New chat",
          "prompt": "",
          "cwd": cwd,
          "role": "orchestrator",
          "isLead": true,
          "needsTitle": true,
          "parentId": null,
          "rootId": id,
          "subagentConcurrencyVersion": 2,
          "maxAgentsExplicit": false,
          "model": if provider(&state, &account) == "claude" { "default" } else { DEFAULT_LEAD_MODEL },
          "effort": "medium",
          "fastMode": false,
          "daybreakEnabled": false,
          "cyberAccessProgram": "standard",
          "concurrency": DEFAULT_CONCURRENCY,
          "maxAgents": DEFAULT_MAX_AGENTS.max(DEFAULT_CONCURRENCY + 2),
          "tokenBudget": null,
          "status": "idle",
          "autoWake": true,
          "epoch": 0,
          "turnId": null,
          "inFlight": false,
          "turnEpoch": 0,
          "usageResumeEnabled": true,
          "tokensUsed": 0,
          "compactions": 0,
          "compactionsObservedOnly": false,
          "contextUsage": null,
          "events": 0,
          "created": created,
          "error": null,
        }),
        json!({
          "profileId": null,
          "profileInstructions": "",
          "tail": "",
          "worktree": false,
          "workspaceMode": workspace_mode,
          "executionMode": "native",
          "hostProjectPath": cwd,
          "layrReady": false,
          "workspaceInGit": false,
          "worktreeReady": false,
          "environment": "host",
          "imageWorkspace": false,
          "imageWorkspaceReady": false,
          "workerDefaults": {"model": null, "effort": null, "fastMode": false, "daybreakEnabled": false, "accountKey": null, "delegationDepth": 1},
          "reviewDefaults": {"model": null, "effort": null},
        }),
        json!({
          "agentMode": "multi",
          "agentModeRevision": 0,
          "agentModeSupported": true,
          "quickCreate": true,
          "quickCreateRequest": request_signature,
          "quickCreateRequestBody": body
        }),
    ]);
    persist_created(&state, id, "", record).await.map(Json)
}

async fn create_agent(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    if body.get("parent").is_some_and(|value| !value.is_null()) {
        return Err(unavailable("worker-driver/workspace-preparation"));
    }
    for field in [
        "model",
        "effort",
        "fast_mode",
        "daybreak_enabled",
        "worker_defaults",
        "profile_id",
    ] {
        if body.get(field).is_some_and(|value| !value.is_null()) {
            return Err(unavailable(if field == "worker_defaults" {
                "worker-driver/settings-catalog"
            } else {
                "provider-session/model-list"
            }));
        }
    }
    let role = body
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("orchestrator");
    if role != "orchestrator" {
        return Err(unavailable("worker-driver/work-assignment"));
    }
    let workspace_mode = body
        .get("workspaceMode")
        .and_then(Value::as_str)
        .unwrap_or("worktree");
    if workspace_mode != "worktree" {
        return Err(unavailable("workspace-state/lead-workspace"));
    }
    if state.account_driver.is_none() {
        return Err(unavailable("account-driver/unavailable"));
    }
    let cwd = project_directory(&state, body.get("cwd"))?;
    let account = account_key(&state, body.get("account_key"), &cwd)?;
    refresh_account(&state, &account)?;
    let id = match body.get("id").and_then(Value::as_str) {
        Some(id) => id.to_owned(),
        None => agents_create::new_id().map_err(create_error)?,
    };
    let prompt = body
        .get("prompt")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::validation(vec![Value::String("body".into())]))?
        .trim()
        .to_owned();
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Lead")
        .trim()
        .to_owned();
    let yolo = body
        .get("yolo_mode")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let concurrency = body
        .get("concurrency")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_CONCURRENCY);
    let requested_max_agents = body
        .get("maxAgents")
        .and_then(Value::as_i64)
        .unwrap_or(DEFAULT_MAX_AGENTS);
    let max_agents = if body.get("maxAgents").is_some_and(|value| !value.is_null()) {
        requested_max_agents
    } else {
        requested_max_agents.max(concurrency + 2)
    };
    let token_budget = body.get("tokenBudget").cloned().unwrap_or(Value::Null);
    let created = unix_time()?;
    let record = merge_records([
        json!({
          "id": id,
          "threadId": null,
          "accountKey": account,
          "executionSettingsAccountKey": account,
          "provider": provider(&state, &account),
          "yoloMode": yolo,
          "name": name,
          "prompt": prompt,
          "cwd": cwd,
          "role": "orchestrator",
          "isLead": true,
          "needsTitle": false,
          "parentId": null,
          "rootId": id,
          "subagentConcurrencyVersion": 2,
          "maxAgentsExplicit": body.get("maxAgents").is_some_and(|v| !v.is_null()),
          "model": if provider(&state, &account) == "claude" { "default" } else { DEFAULT_LEAD_MODEL },
          "effort": "medium",
          "fastMode": false,
          "daybreakEnabled": false,
          "cyberAccessProgram": "standard",
          "concurrency": concurrency,
          "maxAgents": max_agents,
          "tokenBudget": token_budget,
          "status": "queued",
          "autoWake": true,
          "epoch": 0,
          "turnId": null,
          "inFlight": false,
          "turnEpoch": 0,
          "usageResumeEnabled": true,
          "tokensUsed": 0,
          "compactions": 0,
          "compactionsObservedOnly": false,
          "contextUsage": null,
          "events": 0,
          "created": created,
          "error": null,
        }),
        json!({
          "profileId": null,
          "profileInstructions": "",
          "tail": "",
          "worktree": false,
          "workspaceMode": workspace_mode,
          "executionMode": "native",
          "hostProjectPath": cwd,
          "layrReady": false,
          "workspaceInGit": false,
          "worktreeReady": false,
          "environment": "host",
          "imageWorkspace": false,
          "imageWorkspaceReady": false,
          "workerDefaults": {"model": null, "effort": null, "fastMode": false, "daybreakEnabled": false, "accountKey": null, "delegationDepth": 1},
          "reviewDefaults": {"model": null, "effort": null},
        }),
        json!({
          "agentMode": "multi",
          "agentModeRevision": 0,
          "agentModeSupported": true
        }),
    ]);
    persist_created(&state, id, &prompt, record).await.map(Json)
}

async fn persist_created(
    state: &HttpState,
    id: String,
    prompt: &str,
    record: Value,
) -> Result<Value, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let before = {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        SyncDocumentsRepository::entity_sequence(&connection).map_err(|_| ApiError::internal())?
    };
    let prompt = prompt.to_owned();
    let created = record
        .get("created")
        .and_then(Value::as_f64)
        .ok_or_else(ApiError::internal)?;
    let result = store
        .transaction_with_error(|transaction| {
            agents_create::create_agent(transaction, &id, &prompt, record, created, |value| {
                validators.validate_stored("AgentRecordResponse", value)
            })
        })
        .map_err(create_error)?;
    let result = with_sync_delta(&store, before, result)?;
    state.event_wake.notify_one();
    super::generated_json::<crate::generated::AgentResponse>(result)
}

async fn configure(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    if body.get("concurrency").is_some() {
        if body.get("maxAgents").is_some() || body.get("tokenBudget").is_some() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Set subagent concurrency separately from team storage and token limits",
            ));
        }
        return Err(unavailable("agent-management/agent-mode-settings"));
    }
    if body.get("expected_mode_revision").is_some() || body.get("request_id").is_some() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Configure only team storage and token limits",
        ));
    }
    let id = required_string(&body, "id")?;
    let max_agents = body.get("maxAgents").and_then(Value::as_i64);
    let token_budget = body.get("tokenBudget").map(|value| value.as_i64());
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let before = {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        SyncDocumentsRepository::entity_sequence(&connection).map_err(|_| ApiError::internal())?
    };
    let result = store
        .transaction_with_error(|transaction| {
            agents_create::configure_limits(transaction, &id, max_agents, token_budget, |value| {
                validators.validate_stored("AgentRecordResponse", value)
            })
        })
        .map_err(settings_error)?;
    let mut result = result;
    result = with_sync_delta(&store, before, result)?;
    Ok(Json(super::generated_json::<
        crate::generated::ConfigureResponse,
    >(result)?))
}

async fn conversation(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    if [
        "cwd",
        "yolo_mode",
        "model",
        "effort",
        "fast_mode",
        "daybreak_enabled",
        "next_turn",
        "request_id",
        "worker_defaults",
        "review_defaults",
        "agent_mode",
        "subagent_concurrency",
        "expected_mode_revision",
    ]
    .iter()
    .any(|field| {
        if matches!(
            *field,
            "agent_mode" | "subagent_concurrency" | "expected_mode_revision"
        ) {
            body.get(*field).is_some()
        } else {
            body.get(*field).is_some_and(|value| !value.is_null())
        }
    }) {
        return Err(unavailable("agent-management/conversation-settings"));
    }
    let id = required_string(&body, "id")?;
    let expected = body.get("expected_account_key").and_then(Value::as_str);
    let excluded = body
        .get("excluded_account_keys")
        .and_then(Value::as_array)
        .map(|items| {
            let mut keys = items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect::<Vec<_>>();
            keys.sort();
            keys
        });
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let before = {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        SyncDocumentsRepository::entity_sequence(&connection).map_err(|_| ApiError::internal())?
    };
    let result = store
        .transaction_with_error(|transaction| {
            agents_create::update_exclusions(
                transaction,
                &id,
                expected,
                excluded.as_deref(),
                |value| validators.validate_stored("AgentRecordResponse", value),
            )
        })
        .map_err(settings_error)?;
    let result = with_sync_delta(&store, before, result)?;
    Ok(Json(super::generated_json::<
        crate::generated::AgentResponse,
    >(result)?))
}

async fn delete_conversation(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(unavailable("agent-management/conversation-termination"))
}

fn account_key(
    state: &HttpState,
    requested: Option<&Value>,
    cwd: &str,
) -> Result<String, ApiError> {
    if let Some(account) = requested.and_then(Value::as_str) {
        return Ok(account.to_owned());
    }
    let default = state
        .account_policy
        .as_ref()
        .and_then(|policy| {
            policy
                .accounts()
                .default_account_key
                .as_str()
                .map(str::to_owned)
        })
        .ok_or_else(ApiError::internal)?;
    let Some(store) = &state.store else {
        return Ok(default);
    };
    let connection = store.read().map_err(|_| ApiError::internal())?;
    let has_projects: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_projects')",
            [],
            |row| row.get(0),
        )
        .map_err(|_| ApiError::internal())?;
    if !has_projects {
        return Ok(default);
    }
    let mut statement = connection
        .prepare("SELECT id,record FROM runtime_projects")
        .map_err(|_| ApiError::internal())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| ApiError::internal())?;
    let mut selected: Option<(usize, String)> = None;
    for row in rows {
        let (id, raw) = row.map_err(|_| ApiError::internal())?;
        let record: Value = serde_json::from_str(&raw).map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid stored object runtime_projects/{id}"),
            )
        })?;
        let project_path = record.get("path").and_then(Value::as_str);
        let project_account = record
            .get("accountKey")
            .filter(|value| !value.is_null())
            .and_then(Value::as_str);
        if record.get("id").and_then(Value::as_str) != Some(id.as_str())
            || !record.get("name").is_some_and(Value::is_string)
            || !record.get("created").is_some_and(Value::is_number)
            || record
                .get("accountKey")
                .is_some_and(|value| !value.is_null() && !value.is_string())
            || project_path.is_none()
        {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid stored object runtime_projects/{id}"),
            ));
        }
        let path = PathBuf::from(project_path.unwrap_or_default());
        if PathBuf::from(cwd).starts_with(&path)
            && let Some(account) = project_account
        {
            let depth = path.components().count();
            if selected
                .as_ref()
                .is_none_or(|(old_depth, _)| depth > *old_depth)
            {
                selected = Some((depth, account.to_owned()));
            }
        }
    }
    Ok(selected.map_or(default, |(_, account)| account))
}

fn provider(state: &HttpState, account: &str) -> &'static str {
    state
        .account_policy
        .as_ref()
        .and_then(|policy| {
            policy
                .accounts()
                .accounts
                .into_iter()
                .find(|row| row.get("id").and_then(Value::as_str) == Some(account))
        })
        .and_then(|row| {
            row.get("provider")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|provider| provider == "claude")
        .map_or("codex", |_| "claude")
}

fn refresh_account(state: &HttpState, account: &str) -> Result<(), ApiError> {
    let Some(driver) = &state.account_driver else {
        return Err(unavailable("account-driver/unavailable"));
    };
    match driver.get(account) {
        Ok(AccountStatus::Unavailable) => {
            let account = state.account_policy.as_ref().and_then(|policy| {
                policy
                    .accounts()
                    .accounts
                    .into_iter()
                    .find(|row| row.get("id").and_then(Value::as_str) == Some(account))
            });
            let message = if account
                .as_ref()
                .and_then(|row| row.get("deleted"))
                .and_then(Value::as_bool)
                == Some(true)
            {
                "This account was deleted. Select another account for new chats"
            } else {
                "Reconnect this account before creating a chat"
            };
            Err(ApiError::new(StatusCode::BAD_REQUEST, message))
        }
        Ok(_) => Ok(()),
        Err(AccountDriverError::UnknownAccount) => {
            Err(ApiError::new(StatusCode::BAD_REQUEST, "Unknown account"))
        }
        Err(_) => Err(ApiError::internal()),
    }
}

fn project_directory(state: &HttpState, requested: Option<&Value>) -> Result<String, ApiError> {
    let path = if let Some(requested) = requested.and_then(Value::as_str) {
        PathBuf::from(requested)
    } else if let Some(configured) = std::env::var_os("CODEX_CANVAS_CWD") {
        PathBuf::from(configured)
    } else {
        std::env::current_dir().map_err(|_| ApiError::internal())?
    };
    let path = path.canonicalize().map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "Select an existing project directory",
        )
    })?;
    if !path.is_dir() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Select an existing project directory",
        ));
    }
    let _ = state;
    Ok(path.to_string_lossy().into_owned())
}

fn required_string(body: &Value, field: &str) -> Result<String, ApiError> {
    body.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            ApiError::validation(vec![
                Value::String("body".into()),
                Value::String(field.into()),
            ])
        })
}

fn signature(body: &Value) -> String {
    Sha256::digest(python_json(body).as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn merge_records<const N: usize>(parts: [Value; N]) -> Value {
    let mut merged = serde_json::Map::new();
    for part in parts {
        if let Some(fields) = part.as_object() {
            merged.extend(fields.clone());
        }
    }
    Value::Object(merged)
}

fn with_sync_delta(store: &RuntimeStore, after: i64, mut result: Value) -> Result<Value, ApiError> {
    let connection = store.read().map_err(|_| ApiError::internal())?;
    let rows =
        SyncDocumentsRepository::entities_after(&connection, after, MAX_MUTATION_SYNC_ENTITIES + 1)
            .map_err(|_| ApiError::internal())?;
    if !rows.is_empty() {
        result["_syncEntities"] = Value::Array(
            rows.iter()
                .take(MAX_MUTATION_SYNC_ENTITIES as usize)
                .map(|row| {
                    json!({
                        "id": format!("entity:{}:{}", row.collection, row.id),
                        "seq": row.sequence,
                        "payload": row.payload,
                        "_deleted": row.deleted,
                    })
                })
                .collect(),
        );
        let floor = SyncDocumentsRepository::entity_tombstone_floor(&connection)
            .map_err(|_| ApiError::internal())?;
        if rows.len() <= MAX_MUTATION_SYNC_ENTITIES as usize && floor <= after {
            result["_syncEntitiesAfter"] = json!(after);
        }
    }
    Ok(result)
}

fn unix_time() -> Result<f64, ApiError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .map_err(|_| ApiError::internal())
}

fn unavailable(dependency: &'static str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        format!("Not implemented: requires {dependency}"),
    )
}

fn create_error(error: CreateError) -> ApiError {
    match error {
        CreateError::Conflict => ApiError::new(StatusCode::BAD_REQUEST, error.to_string()),
        CreateError::Deleted => ApiError::new(StatusCode::BAD_REQUEST, error.to_string()),
        CreateError::InvalidStoredObject { .. } => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
        CreateError::Store(_) => ApiError::internal(),
    }
}

fn settings_error(error: SettingsError) -> ApiError {
    match error {
        SettingsError::UnknownAgent => ApiError::new(StatusCode::BAD_REQUEST, error.to_string()),
        SettingsError::LeadRequired
        | SettingsError::Deleted
        | SettingsError::Conflict
        | SettingsError::Stale => ApiError::new(StatusCode::BAD_REQUEST, error.to_string()),
        SettingsError::CurrentAccountExcluded => {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
        SettingsError::InvalidStoredObject { .. } => {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
        }
        SettingsError::Store(_) => ApiError::internal(),
    }
}

#[cfg(test)]
mod tests {
    use super::rename_signature;
    use sha2::{Digest, Sha256};

    #[test]
    fn rename_signature_matches_python_sorted_json_encoding() {
        let body = r#"{"chat": "chat-1", "name": "Caf\u00e9", "operation": "rename"}"#;
        let expected = Sha256::digest(body.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(rename_signature("chat-1", Some("Café")), expected);
    }
}
