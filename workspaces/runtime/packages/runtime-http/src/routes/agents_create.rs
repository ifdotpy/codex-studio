//! Persisted agent creation and management route slice.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use studio_runtime_services::account_driver::{AccountDriverError, AccountStatus};
use studio_runtime_services::agents_create::{self, CreateError, RenameError, SettingsError};
use studio_runtime_services::event_write::python_json;
use studio_runtime_store::{RuntimeStore, SyncDocumentsRepository};

use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) fn routes() -> RouteGroup {
    const LEAD_DEPENDENCIES: [Dependency; 5] = [
        Dependency::new("account-driver/unavailable"),
        Dependency::new("agent-management/empty-chat-reuse"),
        Dependency::new("project-driver/chat-binding"),
        Dependency::new("provider-session/model-list"),
        Dependency::new("worker-driver/settings-catalog"),
    ];
    const AGENT_DEPENDENCIES: [Dependency; 6] = [
        Dependency::new("account-driver/unavailable"),
        Dependency::new("provider-session/model-list"),
        Dependency::new("worker-driver/settings-catalog"),
        Dependency::new("worker-driver/work-assignment"),
        Dependency::new("worker-driver/workspace-preparation"),
        Dependency::new("workspace-state/lead-workspace"),
    ];
    const CONFIGURE_DEPENDENCIES: [Dependency; 1] =
        [Dependency::new("agent-management/agent-mode-settings")];
    const CONVERSATION_DEPENDENCIES: [Dependency; 1] =
        [Dependency::new("agent-management/conversation-settings")];
    const DELETE_DEPENDENCY: [Dependency; 1] =
        [Dependency::new("agent-management/conversation-termination")];
    RouteGroup::new(vec![
        post(
            "/api/agents",
            OperationStatus::partial(&AGENT_DEPENDENCIES),
            create_agent,
        ),
        post(
            "/api/configure",
            OperationStatus::partial(&CONFIGURE_DEPENDENCIES),
            configure,
        ),
        post(
            "/api/conversation",
            OperationStatus::partial(&CONVERSATION_DEPENDENCIES),
            conversation,
        ),
        post(
            "/api/conversation/delete",
            OperationStatus::partial(&DELETE_DEPENDENCY),
            delete_conversation,
        ),
        post(
            "/api/leads",
            OperationStatus::partial(&LEAD_DEPENDENCIES),
            create_lead,
        ),
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
        RenameError::InvalidStoredObject { object, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}/{id}"),
        ),
        RenameError::Store(_) => ApiError::internal(),
    }
}

async fn create_lead(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let workspace_mode =
        body.get("workspaceMode")
            .and_then(Value::as_str)
            .unwrap_or(if cfg!(target_os = "macos") {
                "layr"
            } else {
                "worktree"
            });
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or(agents_create::new_id().map_err(create_error)?);
    let signature_cwd = if body.get("cwd").and_then(Value::as_str).is_some() {
        Some(project_directory(&state, body.get("cwd"))?)
    } else {
        None
    };
    let signature = lead_signature(&body, workspace_mode, signature_cwd.as_deref());
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let replay = store.read().map_err(|_| ApiError::internal())?;
    let existing = agents_create::replay_lead(&replay, &id, &signature, |value| {
        validators.validate_stored("AgentRecordResponse", value)
    })
    .map_err(create_error)?;
    drop(replay);
    if let Some(existing) = existing {
        let existing_id = existing
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(ApiError::internal)?
            .to_owned();
        let projected = projected_response(&state, &existing_id, existing)?;
        return Ok(Json(super::generated_json::<
            crate::generated::AgentResponse,
        >(projected)?));
    }
    for field in ["effort", "fast_mode", "daybreak_enabled"] {
        if body.get(field).is_some() {
            return Err(unavailable("provider-session/model-list"));
        }
    }
    if body.get("model").is_some_and(|value| !value.is_null()) {
        return Err(unavailable("provider-session/model-list"));
    }
    if body.get("worker_defaults").is_some() || body.get("review_defaults").is_some() {
        return Err(unavailable("worker-driver/settings-catalog"));
    }
    if ["project_id", "project_server_id", "project_folder"]
        .iter()
        .any(|field| body.get(*field).is_some_and(|value| !value.is_null()))
    {
        return Err(unavailable("project-driver/chat-binding"));
    }
    if body.get("previous").is_some_and(|value| !value.is_null()) {
        return Err(unavailable("agent-management/empty-chat-reuse"));
    }
    if workspace_mode != "worktree" {
        return Err(unavailable("workspace-state/lead-workspace"));
    }
    if state.account_driver.is_none() {
        return Err(unavailable("account-driver/unavailable"));
    }
    let cwd = project_directory(&state, body.get("cwd"))?;
    let account = account_key(&state, body.get("account_key"), &cwd)?;
    refresh_account(&state, &account)?;
    let yolo = body
        .get("yolo_mode")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let created = unix_time()?;
    let record = build_agent_record(
        &id,
        "New chat",
        "",
        &cwd,
        &account,
        provider(&state, &account),
        yolo,
        "orchestrator",
        true,
        "idle",
        workspace_mode,
        Some(DEFAULT_CONCURRENCY),
        DEFAULT_MAX_AGENTS.max(DEFAULT_CONCURRENCY + 2),
        Value::Null,
        true,
        created,
        Some(signature),
    );
    persist_created(&state, id, "", record).await.map(Json)
}

async fn create_agent(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    if state.store.is_none() && state.account_driver.is_none() {
        return Err(unavailable("account-driver/unavailable"));
    }
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or(agents_create::new_id().map_err(create_error)?);
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let replay = store.read().map_err(|_| ApiError::internal())?;
    if let Some(existing) = agents_create::replay_agent(&replay, &id, &body, |value| {
        validators.validate_stored("AgentRecordResponse", value)
    })
    .map_err(create_error)?
    {
        let existing_id = existing
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(ApiError::internal)?
            .to_owned();
        let projected = projected_response(&state, &existing_id, existing)?;
        return Ok(Json(super::generated_json::<
            crate::generated::AgentResponse,
        >(projected)?));
    }
    drop(replay);
    if body.get("parent").is_some_and(|value| !value.is_null()) {
        return Err(unavailable("worker-driver/workspace-preparation"));
    }
    for field in [
        "model",
        "effort",
        "fast_mode",
        "daybreak_enabled",
        "worker_defaults",
    ] {
        if body.get(field).is_some() {
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
    if !matches!(role, "orchestrator" | "reviewer" | "implementer") {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Invalid agent role"));
    }
    let workspace_mode = body
        .get("workspaceMode")
        .and_then(Value::as_str)
        .unwrap_or("worktree");
    if workspace_mode != "worktree" {
        return Err(unavailable("workspace-state/lead-workspace"));
    }
    if body.get("concurrency").is_some_and(Value::is_null) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Concurrency must be an integer from 0 to 512",
        ));
    }
    if body.get("maxAgents").is_some_and(Value::is_null) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Team size must be an integer from 1 to 1024",
        ));
    }
    if body.get("yolo_mode").is_some_and(Value::is_null) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "yolo_mode must be a boolean",
        ));
    }
    if state.account_driver.is_none() {
        return Err(unavailable("account-driver/unavailable"));
    }
    if body
        .get("cwd")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
    {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Select an existing project directory",
        ));
    }
    let cwd = project_directory(&state, body.get("cwd"))?;
    let account = account_key(&state, body.get("account_key"), &cwd)?;
    refresh_account(&state, &account)?;
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
    let mut record = build_agent_record(
        &id,
        &name,
        &prompt,
        &cwd,
        &account,
        provider(&state, &account),
        yolo,
        role,
        role == "orchestrator",
        "queued",
        workspace_mode,
        Some(concurrency),
        max_agents,
        token_budget,
        false,
        created,
        None,
    );
    if role == "orchestrator" {
        record["maxAgentsExplicit"] = json!(body.get("maxAgents").is_some());
    }
    if let Some(profile_id) = body.get("profile_id") {
        record["profileId"] = profile_id.clone();
    }
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
    let projected = projected_response(state, &id, result)?;
    super::generated_json::<crate::generated::AgentResponse>(projected)
}

async fn configure(
    State(state): State<HttpState>,
    ApiJson(request): ApiJson<crate::generated::ConfigureRequest>,
) -> Result<Json<Value>, ApiError> {
    let body = serde_json::to_value(request).map_err(|_| ApiError::internal())?;
    if body.get("concurrency").is_some() {
        if body.get("maxAgents").is_some() || body.get("tokenBudget").is_some() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Set subagent concurrency separately from team storage and token limits",
            ));
        }
        if body.get("concurrency").is_some_and(Value::is_null) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Subagent concurrency must be an integer from 0 to 512",
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
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(ApiError::internal)?
        .to_owned();
    if body.get("maxAgents").is_some_and(Value::is_null) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Team size must be an integer from 1 to 1024",
        ));
    }
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
    ApiJson(request): ApiJson<crate::generated::ConversationRequest>,
) -> Result<Json<Value>, ApiError> {
    let body = serde_json::to_value(request).map_err(|_| ApiError::internal())?;
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
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(ApiError::internal)?
        .to_owned();
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
    let result_id = result
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or(&id)
        .to_owned();
    let projected = projected_response(&state, &result_id, result)?;
    Ok(Json(super::generated_json::<
        crate::generated::AgentResponse,
    >(projected)?))
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
    let expand_home = |value: &str| {
        if value == "~" || value.starts_with("~/") {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| {
                    if value == "~" {
                        home
                    } else {
                        home.join(&value[2..])
                    }
                })
                .unwrap_or_else(|| PathBuf::from(value))
        } else {
            PathBuf::from(value)
        }
    };
    let path = if let Some(requested) = requested.and_then(Value::as_str) {
        expand_home(requested)
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

fn lead_signature(body: &Value, workspace_mode: &str, cwd: Option<&str>) -> String {
    let mut settings = serde_json::Map::new();
    settings.insert(
        "model".into(),
        body.get("model").cloned().unwrap_or(Value::Null),
    );
    settings.insert(
        "previous".into(),
        body.get("previous").cloned().unwrap_or(Value::Null),
    );
    settings.insert(
        "workspaceMode".into(),
        body.get("workspaceMode")
            .cloned()
            .unwrap_or_else(|| json!(workspace_mode)),
    );
    for field in [
        "effort",
        "fast_mode",
        "daybreak_enabled",
        "worker_defaults",
        "review_defaults",
        "account_key",
        "reuse_empty",
        "project_folder",
        "project_id",
        "project_server_id",
        "yolo_mode",
    ] {
        if let Some(value) = body.get(field) {
            settings.insert(field.to_owned(), value.clone());
        }
    }
    if body.get("cwd").and_then(Value::as_str).is_some() {
        settings.insert("cwd".into(), json!(cwd));
    }
    python_json(&Value::Object(settings))
}

#[allow(clippy::too_many_arguments)]
fn build_agent_record(
    id: &str,
    name: &str,
    prompt: &str,
    cwd: &str,
    account: &str,
    provider: &str,
    yolo: bool,
    role: &str,
    is_lead: bool,
    status: &str,
    workspace_mode: &str,
    concurrency: Option<i64>,
    max_agents: i64,
    token_budget: Value,
    draft: bool,
    created: f64,
    quick_create_signature: Option<String>,
) -> Value {
    let mut record = merge_records([
        json!({
          "id":id,"threadId":null,"accountKey":account,"executionSettingsAccountKey":account,
          "provider":provider,"yoloMode":yolo,"name":name,"prompt":prompt,"cwd":cwd,
          "role":role,"isLead":is_lead,"needsTitle":draft,"parentId":null,"rootId":id,
          "model":if provider == "claude" {"default"} else {DEFAULT_LEAD_MODEL},"effort":"medium",
          "fastMode":false,"daybreakEnabled":false,"cyberAccessProgram":"standard",
          "maxAgents":max_agents,"tokenBudget":token_budget,"status":status,"autoWake":true,
          "epoch":0,"turnId":null,"inFlight":false,"turnEpoch":0,"usageResumeEnabled":true,
          "tokensUsed":0,"compactions":0,"compactionsObservedOnly":false,"contextUsage":null,
          "events":0,"created":created,"error":null,"profileId":null,"profileInstructions":""
        }),
        json!({
          "tail":"","worktree":false,"workspaceMode":workspace_mode,"executionMode":"native",
          "hostProjectPath":cwd,"layrReady":false,"workspaceInGit":false,"worktreeReady":false,
          "environment":"host","imageWorkspace":false,"imageWorkspaceReady":false,
          "imageWorkspacePhase":null,"imageWorkspaceRepo":null,"imageWorkspaceSubpath":".",
          "imageWorkspaceBaseRef":null,"imageWorkspaceHasGit":false,"imageWorkspaceCreatedAt":null,
          "imageWorkspaceError":null,"workspaceBackend":null,"workspaceModeExplicit":false,
          "worktreeWarning":null,"workerBaseRef":null,"workerBaseCommit":null,
          "workerBaseBehindMain":null,"workerBaseMainRef":null
        }),
    ]);
    if let Some(concurrency) = concurrency {
        record["concurrency"] = json!(concurrency);
    }
    if is_lead {
        record["subagentConcurrencyVersion"] = json!(2);
        record["maxAgentsExplicit"] = json!(false);
        record["agentMode"] = json!("multi");
        record["agentModeRevision"] = json!(0);
        record["agentModeSupported"] = json!(true);
        record["workerDefaults"] = json!({"model":"gpt-6-luna","effort":"high","fastMode":false,"daybreakEnabled":false,"delegationDepth":1});
        record["reviewDefaults"] = json!({"model":null,"effort":null});
    }
    if draft {
        record["quickCreate"] = json!(true);
        record["quickCreateRequest"] = json!(quick_create_signature);
    }
    record
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

fn projected_response(state: &HttpState, id: &str, result: Value) -> Result<Value, ApiError> {
    let store = state.store.as_ref().ok_or_else(ApiError::internal)?;
    let connection = store.read().map_err(|_| ApiError::internal())?;
    let raw = connection
        .query_row(
            "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| ApiError::internal())?;
    let envelope: Value = serde_json::from_str(&raw).map_err(|_| ApiError::internal())?;
    let mut projected = envelope
        .get("value")
        .cloned()
        .ok_or_else(ApiError::internal)?;
    if let Some(object) = projected.as_object_mut() {
        for field in ["_syncEntities", "_syncEntitiesAfter"] {
            if let Some(value) = result.get(field) {
                object.insert(field.to_owned(), value.clone());
            }
        }
    }
    Ok(projected)
}

fn unix_time() -> Result<f64, ApiError> {
    studio_runtime_services::event_write::unix_time().map_err(|_| ApiError::internal())
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
        | SettingsError::InFlight
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
