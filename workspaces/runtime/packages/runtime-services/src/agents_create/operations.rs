//! Synchronous orchestration of persisted agent operations.
use super::*;
use crate::account_driver::{AccountDriver, AccountDriverError, AccountStatus};
use crate::account_policy::AccountPolicyState;
use crate::event_write::python_json;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use studio_runtime_store::RuntimeStore;

pub struct Context<'a> {
    pub store: &'a RuntimeStore,
    pub accounts: Option<&'a AccountPolicyState>,
    pub driver: Option<&'a AccountDriver>,
    pub validate: &'a dyn Fn(&str, &Value) -> bool,
}

#[derive(Debug, Error)]
pub enum OperationError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Not implemented: requires {0}")]
    Unavailable(&'static str),
    #[error(transparent)]
    Create(#[from] CreateError),
    #[error(transparent)]
    Rename(#[from] RenameError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Internal server error")]
    Internal,
}

#[derive(Default)]
pub struct Creation {
    pub id: Option<String>,
    pub cwd: Option<String>,
    pub account: Option<String>,
    pub name: Option<String>,
    pub prompt: String,
    pub role: Option<String>,
    pub workspace: Option<String>,
    pub yolo: Option<Option<bool>>,
    pub concurrency: Option<Option<i64>>,
    pub max_agents: Option<Option<i64>>,
    pub token_budget: Option<i64>,
    pub profile: Option<String>,
    pub parent: Option<String>,
    pub previous: Option<String>,
    pub reuse_empty: Option<Option<bool>>,
    pub catalog: bool,
    pub worker_catalog: bool,
    pub project_binding: bool,
    pub intent: Value,
}

pub struct Configuration {
    pub id: String,
    pub concurrency: Option<Option<i64>>,
    pub max_agents: Option<Option<i64>>,
    pub token_budget: Option<Option<i64>>,
    pub mode_receipt: bool,
}

pub struct Conversation {
    pub id: String,
    pub expected_account: Option<String>,
    pub excluded: Option<Vec<String>>,
    pub settings: bool,
}

const DEFAULT_LEAD_MODEL: &str = "gpt-6-astra";
const DEFAULT_CONCURRENCY: i64 = 32;
const DEFAULT_MAX_AGENTS: i64 = 64;

pub fn lead(context: &Context<'_>, request: Creation) -> Result<Value, OperationError> {
    create(context, request, true)
}
pub fn agent(context: &Context<'_>, request: Creation) -> Result<Value, OperationError> {
    create(context, request, false)
}
fn create(
    context: &Context<'_>,
    mut request: Creation,
    draft: bool,
) -> Result<Value, OperationError> {
    let id = request.id.clone().map_or_else(new_id, Ok)?;
    let workspace = request
        .workspace
        .as_deref()
        .unwrap_or(if draft && cfg!(target_os = "macos") {
            "layr"
        } else {
            "worktree"
        });
    let requested_cwd = request.cwd.as_deref().map(project_directory).transpose()?;
    if !draft {
        request
            .intent
            .as_object_mut()
            .ok_or(OperationError::Internal)?
            .remove("id");
        request.intent["prompt"] = json!(request.prompt.trim());
        request.intent["name"] = json!(request.name.as_deref().unwrap_or("Lead").trim());
        if let Some(cwd) = &requested_cwd {
            request.intent["cwd"] = json!(cwd);
        }
    }
    let signature = lead_signature(&request.intent, workspace, requested_cwd.as_deref());
    let read = context.store.read()?;
    let validate = |value: &Value| (context.validate)("AgentRecordResponse", value);
    let existing = if draft {
        replay_lead(&read, &id, &signature, validate)?
    } else {
        replay_agent(&read, &id, &request.intent, validate)?
    };
    drop(read);
    if let Some(existing) = existing {
        return projected_response(context.store, &existing);
    }
    if request.parent.is_some() {
        return Err(OperationError::Unavailable(
            "worker-driver/workspace-preparation",
        ));
    }
    if request.catalog {
        return Err(OperationError::Unavailable("provider-session/model-list"));
    }
    if request.worker_catalog {
        return Err(OperationError::Unavailable(
            "worker-driver/settings-catalog",
        ));
    }
    if request.project_binding {
        return Err(OperationError::Unavailable("project-driver/chat-binding"));
    }
    if request.previous.is_some() {
        return Err(OperationError::Unavailable(
            "agent-management/empty-chat-reuse",
        ));
    }
    if workspace != "worktree" {
        return Err(OperationError::Unavailable(
            "workspace-state/lead-workspace",
        ));
    }
    if !draft && request.cwd.as_deref().is_none_or(str::is_empty) {
        return Err(OperationError::Invalid(
            "Select an existing project directory",
        ));
    }
    let cwd = requested_cwd.map_or_else(|| project_directory_default(), Ok)?;
    let account = account_key(context, request.account.as_deref(), &cwd)?;
    let yolo = non_null(request.yolo, true)?;
    let concurrency = non_null(request.concurrency, DEFAULT_CONCURRENCY)?;
    let requested_max_agents = non_null(request.max_agents, DEFAULT_MAX_AGENTS)?;
    let max_agents = if request.max_agents.is_some() {
        requested_max_agents
    } else {
        requested_max_agents.max(concurrency + 2)
    };
    let role = request.role.as_deref().unwrap_or("orchestrator");
    let prompt = request.prompt.trim();
    let created = crate::event_write::unix_time().map_err(|_| OperationError::Internal)?;
    let mut record = build_agent_record(
        &id,
        if draft {
            "New chat"
        } else {
            request.name.as_deref().unwrap_or("Lead").trim()
        },
        prompt,
        &cwd,
        &account,
        provider(context, &account),
        yolo,
        role,
        role == "orchestrator",
        if draft { "idle" } else { "queued" },
        workspace,
        Some(concurrency),
        max_agents,
        json!(request.token_budget),
        draft,
        created,
        draft.then_some(signature),
    );
    if !draft && role == "orchestrator" {
        record["maxAgentsExplicit"] = json!(request.max_agents.is_some());
    }
    record["profileId"] = json!(request.profile);
    if !draft {
        record["creationIntent"] = request.intent;
    }
    let before = SyncDocumentsRepository::entity_sequence(&context.store.read()?)?;
    let result = context.store.transaction_with_error(|tx| {
        {
            // Replay is rechecked while holding the writer lock, before admission.
            let existing = if draft {
                replay_lead(
                    tx,
                    &id,
                    record["quickCreateRequest"]
                        .as_str()
                        .ok_or(OperationError::Internal)?,
                    validate,
                )?
            } else {
                replay_agent(tx, &id, &record["creationIntent"], validate)?
            };
            if let Some(existing) = existing {
                return Ok::<_, OperationError>(existing);
            }
            refresh_account(context, &account)?;
            Ok(super::create_agent(
                tx, &id, prompt, record, created, validate,
            )?)
        }
    })?;
    let projected = projected_response(context.store, &result)?;
    with_sync_delta(context.store, before, projected)
}

fn non_null<T>(value: Option<Option<T>>, default: T) -> Result<T, OperationError> {
    value
        .unwrap_or(Some(default))
        .ok_or(OperationError::Invalid("A value is required"))
}

pub fn configure(context: &Context<'_>, request: Configuration) -> Result<Value, OperationError> {
    if request.concurrency.is_some() {
        if request.max_agents.is_some() || request.token_budget.is_some() {
            return Err(OperationError::Invalid(
                "Configure only team storage and token limits",
            ));
        }
        non_null(request.concurrency, 32)?;
        return Err(OperationError::Unavailable(
            "agent-management/agent-mode-settings",
        ));
    }
    if request.mode_receipt {
        return Err(OperationError::Invalid(
            "Configure only team storage and token limits",
        ));
    }
    let max_agents = request
        .max_agents
        .map(|value| value.ok_or(OperationError::Invalid("A value is required")))
        .transpose()?;
    let before = SyncDocumentsRepository::entity_sequence(&context.store.read()?)?;
    let result = context.store.transaction_with_error(|tx| {
        configure_limits(tx, &request.id, max_agents, request.token_budget, |value| {
            (context.validate)("AgentRecordResponse", value)
        })
    })?;
    with_sync_delta(context.store, before, result)
}
pub fn conversation(context: &Context<'_>, request: Conversation) -> Result<Value, OperationError> {
    if request.settings {
        return Err(OperationError::Unavailable(
            "agent-management/conversation-settings",
        ));
    }
    let before = SyncDocumentsRepository::entity_sequence(&context.store.read()?)?;
    let result = context.store.transaction_with_error(|tx| {
        update_exclusions(
            tx,
            &request.id,
            request.expected_account.as_deref(),
            request.excluded.as_deref(),
            |value| (context.validate)("AgentRecordResponse", value),
        )
    })?;
    let projected = projected_response(context.store, &result)?;
    with_sync_delta(context.store, before, projected)
}
pub fn rename_operation(
    context: &Context<'_>,
    id: &str,
    name: Option<&str>,
    request_id: Option<&str>,
) -> Result<Value, OperationError> {
    let name = name.map(str::trim);
    if name.is_some_and(str::is_empty) {
        return Err(OperationError::Invalid(
            "A name must have 1 to 80 characters",
        ));
    }
    let signature = rename_signature(id, name);
    let before = SyncDocumentsRepository::entity_sequence(&context.store.read()?)?;
    let result = context
        .store
        .transaction_with_error(|tx| rename(tx, id, name, request_id, &signature))?;
    with_sync_delta(context.store, before, result)
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

const MAX_MUTATION_SYNC_ENTITIES: i64 = 500;
fn account_key(
    context: &Context<'_>,
    requested: Option<&str>,
    cwd: &str,
) -> Result<String, OperationError> {
    if let Some(account) = requested {
        return Ok(account.to_owned());
    }
    let default = context
        .accounts
        .as_ref()
        .and_then(|policy| {
            policy
                .accounts()
                .default_account_key
                .as_str()
                .map(str::to_owned)
        })
        .ok_or_else(|| OperationError::Internal)?;
    let Some(store) = &Some(context.store) else {
        return Ok(default);
    };
    let connection = store.read().map_err(|_| OperationError::Internal)?;
    let has_projects: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='runtime_projects')",
            [],
            |row| row.get(0),
        )
        .map_err(|_| OperationError::Internal)?;
    if !has_projects {
        return Ok(default);
    }
    let mut statement = connection
        .prepare("SELECT id,record FROM runtime_projects")
        .map_err(|_| OperationError::Internal)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| OperationError::Internal)?;
    let mut selected: Option<(usize, String)> = None;
    for row in rows {
        let (id, raw) = row.map_err(|_| OperationError::Internal)?;
        let record: Value = serde_json::from_str(&raw).map_err(|_| {
            OperationError::Create(CreateError::InvalidStoredObject {
                object: "runtime_projects",
                id: id.clone(),
            })
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
            return Err(OperationError::Create(CreateError::InvalidStoredObject {
                object: "runtime_projects",
                id: id.clone(),
            }));
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

fn provider(context: &Context<'_>, account: &str) -> &'static str {
    context
        .accounts
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

fn refresh_account(context: &Context<'_>, account: &str) -> Result<(), OperationError> {
    let Some(driver) = &context.driver else {
        return Err(OperationError::Unavailable("account-driver/unavailable"));
    };
    match driver.inspect(account) {
        Ok(AccountStatus::Unavailable) => {
            let account = context.accounts.as_ref().and_then(|policy| {
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
            Err(OperationError::Invalid(message))
        }
        Ok(_) => Ok(()),
        Err(AccountDriverError::UnknownAccount) => Err(OperationError::Invalid("Unknown account")),
        Err(_) => Err(OperationError::Internal),
    }
}

fn project_directory_default() -> Result<String, OperationError> {
    let path = std::env::var_os("CODEX_CANVAS_CWD")
        .map(PathBuf::from)
        .map_or_else(std::env::current_dir, Ok)
        .map_err(|_| OperationError::Internal)?;
    project_directory(&path.to_string_lossy())
}
fn project_directory(value: &str) -> Result<String, OperationError> {
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
    let path = expand_home(value);
    let path = path
        .canonicalize()
        .map_err(|_| OperationError::Invalid("Select an existing project directory"))?;
    if !path.is_dir() {
        return Err(OperationError::Invalid(
            "Select an existing project directory",
        ));
    }
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

fn with_sync_delta(
    store: &RuntimeStore,
    after: i64,
    mut result: Value,
) -> Result<Value, OperationError> {
    let connection = store.read().map_err(|_| OperationError::Internal)?;
    let rows =
        SyncDocumentsRepository::entities_after(&connection, after, MAX_MUTATION_SYNC_ENTITIES + 1)
            .map_err(|_| OperationError::Internal)?;
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
            .map_err(|_| OperationError::Internal)?;
        if rows.len() <= MAX_MUTATION_SYNC_ENTITIES as usize && floor <= after {
            result["_syncEntitiesAfter"] = json!(after);
        }
    }
    Ok(result)
}

fn projected_response(store: &RuntimeStore, result: &Value) -> Result<Value, OperationError> {
    let connection = store.read()?;
    let raw: String = connection
        .query_row(
            "SELECT payload FROM sync_entities WHERE collection='agent' AND id=?1",
            [result["id"].as_str().ok_or(OperationError::Internal)?],
            |row| row.get(0),
        )
        .map_err(StoreError::from)?;
    let envelope: Value = serde_json::from_str(&raw).map_err(StoreError::from)?;
    envelope
        .get("value")
        .cloned()
        .ok_or(OperationError::Internal)
}
