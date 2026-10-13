//! Synchronous orchestration of persisted agent operations.
use super::*;
use crate::account_driver::{AccountDriver, AccountDriverError, AccountStatus};
use crate::account_policy::AccountPolicyState;
use crate::event_write::{python_json, python_strip, truthy};
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
    pub yolo: Option<bool>,
    pub concurrency: Option<i64>,
    pub max_agents: Option<i64>,
    pub token_budget: Option<i64>,
    pub profile: Option<String>,
    pub parent: Option<String>,
    pub previous: Option<String>,
    pub reuse_empty: Option<bool>,
    pub catalog: bool,
    pub worker_catalog: bool,
    pub project_binding: bool,
    pub intent: Value,
}

pub struct Configuration {
    pub id: String,
    pub concurrency: Option<i64>,
    pub max_agents: Option<i64>,
    pub token_budget: Option<Option<i64>>,
    pub mode_receipt: bool,
}

pub struct Conversation {
    pub id: String,
    pub expected_account: Option<String>,
    pub excluded: Option<Vec<String>>,
    pub settings: bool,
    pub non_defaults: bool,
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
        request.intent["prompt"] = json!(python_strip(&request.prompt));
        request.intent["name"] = json!(python_strip(request.name.as_deref().unwrap_or("Lead")));
        if let Some(cwd) = &requested_cwd {
            request.intent["cwd"] = json!(cwd);
        }
    }
    let signature = lead_signature(&request.intent, workspace, requested_cwd.as_deref());
    let read = context.store.read()?;
    let validate = |schema: &str, value: &Value| (context.validate)(schema, value);
    let intent = request.intent.clone();
    let replay = |connection: &Connection| {
        if draft {
            replay_lead(connection, &id, &signature, validate)
        } else {
            replay_agent(connection, &id, &intent, validate)
        }
    };
    let existing = replay(&read)?;
    drop(read);
    if existing.is_some() {
        return context.store.transaction_with_error(|tx| {
            let current = replay(tx)?.ok_or(CreateError::Conflict)?;
            Ok(sync_projection(tx, &current)?)
        });
    }
    let read = context.store.read()?;
    let previous = previous_lead(&read, request.previous.as_deref(), &validate)?;
    let profile = request
        .profile
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| {
            load_record(&read, "runtime_profiles", "ProfileRecord", id, &validate)?
                .ok_or(OperationError::Invalid("Unknown worker profile"))
        })
        .transpose()?;
    if let Some(profile) = &profile {
        if request.role.is_none() {
            request.role = profile
                .get("role")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        request.catalog |= ["model", "effort"].iter().any(|field| {
            !request
                .intent
                .as_object()
                .is_some_and(|body| body.contains_key(*field))
                && profile.get(*field).is_some_and(|value| !value.is_null())
        });
    }
    drop(read);
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
    let cwd = requested_cwd.map_or_else(
        || {
            if let Some(prior) = &previous {
                project_directory(
                    prior
                        .get("hostProjectPath")
                        .filter(|value| !value.is_null())
                        .or_else(|| prior.get("cwd"))
                        .and_then(Value::as_str)
                        .ok_or(OperationError::Internal)?,
                )
            } else {
                project_directory_default()
            }
        },
        Ok,
    )?;
    let account = account_key(context, request.account.as_deref(), &cwd)?;
    let yolo = request.yolo.unwrap_or_else(|| {
        previous
            .as_ref()
            .and_then(|record| record.get("yoloMode"))
            .and_then(Value::as_bool)
            .unwrap_or(true)
    });
    let concurrency = request.concurrency.unwrap_or(DEFAULT_CONCURRENCY);
    let requested_max_agents = request.max_agents.unwrap_or(DEFAULT_MAX_AGENTS);
    let role = request.role.as_deref().unwrap_or("orchestrator");
    let max_agents = if request.max_agents.is_some() || role != "orchestrator" {
        requested_max_agents
    } else {
        requested_max_agents.max(concurrency + 2)
    };
    let prompt = python_strip(&request.prompt);
    let created = crate::event_write::unix_time().map_err(|_| OperationError::Internal)?;
    let mut record = build_agent_record(
        &id,
        if draft {
            "New chat"
        } else {
            python_strip(request.name.as_deref().unwrap_or("Lead"))
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
        draft.then_some(signature.clone()),
    );
    if !draft && role == "orchestrator" {
        record["maxAgentsExplicit"] = json!(request.max_agents.is_some());
    }
    mode_fields(&mut record);
    record["profileId"] = json!(request.profile);
    if let Some(profile) = profile {
        record["profileInstructions"] = profile["instructions"].clone();
    }
    if !draft {
        record["creationIntent"] = intent.clone();
    }
    mutate(context, |tx| {
        if let Some(existing) = replay(tx)? {
            return Ok(sync_projection(tx, &existing)?);
        }
        if let Some(prior) = previous_lead(tx, request.previous.as_deref(), &validate)?
            && request.reuse_empty.unwrap_or(true)
            && !truthy(prior.get("layrReady"))
            && prior
                .get("workspaceMode")
                .and_then(Value::as_str)
                .unwrap_or("worktree")
                == workspace
            && prior
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("codex")
                == provider(context, &account)
            && crate::sync_entity_context::agent_view_context(tx, &prior, None)?.empty_lead
        {
            return Err(OperationError::Unavailable(
                "agent-management/empty-chat-reuse",
            ));
        }
        refresh_account(context, &account)?;
        Ok(super::create_agent(
            tx, &id, prompt, record, created, validate,
        )?)
    })
}

pub fn configure(context: &Context<'_>, request: Configuration) -> Result<Value, OperationError> {
    if request.concurrency.is_some() {
        if request.max_agents.is_some() || request.token_budget.is_some() {
            return Err(OperationError::Invalid(
                "Configure only team storage and token limits",
            ));
        }
        return Err(OperationError::Unavailable(
            "agent-management/agent-mode-settings",
        ));
    }
    if request.mode_receipt {
        return Err(OperationError::Invalid(
            "Configure only team storage and token limits",
        ));
    }
    let max_agents = request.max_agents;
    mutate(context, |tx| {
        let result = configure_limits(
            tx,
            &request.id,
            max_agents,
            request.token_budget,
            |schema, value| (context.validate)(schema, value),
        )?;
        if !(context.validate)("ConfigureResponse", &result) {
            return Err(MutationError::InvalidStoredObject {
                table: "runtime_agents",
                id: request.id.clone(),
            }
            .into());
        }
        Ok(result)
    })
}
pub fn conversation(context: &Context<'_>, request: Conversation) -> Result<Value, OperationError> {
    if request.settings {
        return Err(OperationError::Unavailable(
            "agent-management/conversation-settings",
        ));
    }
    mutate(context, |tx| {
        Ok(update_exclusions(
            tx,
            &request.id,
            request.expected_account.as_deref(),
            request.excluded.as_deref(),
            request.non_defaults,
            |schema, value| (context.validate)(schema, value),
        )?)
    })
}
pub fn rename_operation(
    context: &Context<'_>,
    id: &str,
    name: Option<&str>,
    request_id: Option<&str>,
) -> Result<Value, OperationError> {
    let name = name.map(python_strip);
    if name.is_some_and(str::is_empty) {
        return Err(OperationError::Invalid(
            "A name must have 1 to 80 characters",
        ));
    }
    let signature = rename_signature(id, name);
    mutate(context, |tx| {
        Ok(rename(
            tx,
            id,
            name,
            request_id,
            &signature,
            |schema, value| (context.validate)(schema, value),
        )?)
    })
}
fn rename_signature(id: &str, name: Option<&str>) -> String {
    let body = python_json(&json!({"chat": id, "name": name, "operation": "rename"}));
    Sha256::digest(body.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn previous_lead(
    connection: &Connection,
    id: Option<&str>,
    validate: &impl Fn(&str, &Value) -> bool,
) -> Result<Option<Value>, OperationError> {
    id.filter(|id| !id.is_empty())
        .map(|id| {
            let record =
                load_agent_value(connection, id, validate)?.ok_or(MutationError::UnknownAgent)?;
            if !truthy(record.get("isLead")) {
                return Err(OperationError::Invalid("Select a lead conversation"));
            }
            if truthy(record.get("deletedAt")) {
                return Err(MutationError::Deleted.into());
            }
            Ok(record)
        })
        .transpose()
}

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
    let connection = context.store.read()?;
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
        let invalid = || MutationError::InvalidStoredObject {
            table: "runtime_projects",
            id: id.clone(),
        };
        let record: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
        if !record.is_object() || record.get("id").and_then(Value::as_str) != Some(id.as_str()) {
            return Err(invalid().into());
        }
        let project_path = record
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        let project_account = match record.get("accountKey") {
            None | Some(Value::Null) => None,
            Some(Value::String(account)) => Some(account.as_str()),
            Some(_) => return Err(invalid().into()),
        };
        let path = PathBuf::from(project_path);
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
    let mut settings = body.clone();
    if let Some(fields) = settings.as_object_mut() {
        fields.remove("id");
        fields.entry("model").or_insert(Value::Null);
        fields.entry("previous").or_insert(Value::Null);
        fields
            .entry("workspaceMode")
            .or_insert(json!(workspace_mode));
    }
    if let Some(cwd) = cwd {
        settings["cwd"] = json!(cwd);
    }
    python_json(&settings)
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
        record["maxAgentsExplicit"] = json!(false);
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

fn mutate(
    context: &Context<'_>,
    operation: impl FnOnce(&Transaction<'_>) -> Result<Value, OperationError>,
) -> Result<Value, OperationError> {
    context.store.transaction_with_error(|tx| {
        let before = SyncDocumentsRepository::entity_sequence(tx)?;
        let result = operation(tx)?;
        Ok(crate::sync_documents::with_entity_delta(
            tx, before, result,
        )?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn snapshot(store: &RuntimeStore) -> Vec<Vec<Vec<rusqlite::types::Value>>> {
        let connection = store.read().expect("snapshot connection");
        let mut tables = connection
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .expect("tables");
        tables
            .query_map([], |row| row.get::<_, String>(0))
            .expect("table rows")
            .map(|name| {
                let mut rows = connection
                    .prepare(&format!("SELECT * FROM \"{}\"", name.expect("table")))
                    .expect("read table");
                let columns = rows.column_count();
                rows.query_map([], |row| (0..columns).map(|index| row.get(index)).collect())
                    .expect("rows")
                    .collect::<Result<Vec<_>, _>>()
                    .expect("snapshot")
            })
            .collect()
    }

    #[test]
    fn creation_rechecks_replay_and_previous_eligibility_after_controlled_interleaving() {
        let root = std::env::temp_dir().join(format!("creation-recheck-{}", new_id().expect("id")));
        std::fs::create_dir_all(&root).expect("temporary state");
        let schema: Value = serde_json::from_str(include_str!(
            "../../../../apps/server/tests/fixtures/runtime-state-schema.json"
        ))
        .expect("schema");
        for name in ["canvas.sqlite3", "analytics.sqlite3"] {
            let connection = Connection::open(root.join(name)).expect("database");
            connection
                .execute_batch("PRAGMA journal_mode=WAL;")
                .expect("WAL");
            let mut entries = schema[name]["sqlite_master"]
                .as_array()
                .expect("schema entries")
                .clone();
            entries.sort_by_key(|entry| match entry["type"].as_str() {
                Some("table") => 0,
                Some("index") => 1,
                Some("view") => 2,
                _ => 3,
            });
            for entry in entries {
                let exists: bool = connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
                        [entry["name"].as_str().expect("object name")],
                        |row| row.get(0),
                    )
                    .expect("schema object lookup");
                if let Some(sql) = entry["sql"].as_str().filter(|_| !exists) {
                    connection.execute_batch(sql).expect("schema statement");
                }
            }
        }
        let store = RuntimeStore::open(Some(&root)).expect("runtime store");
        for scenario in ["agent", "lead", "previous"] {
            let intent = if scenario == "agent" {
                json!({"cwd":root,"name":"Lead","prompt":"Task"})
            } else {
                json!({"cwd":root})
            };
            let mut record = json!({"id":scenario,"isLead":true,"cwd":root,"status":"idle"});
            record["creationIntent"] = intent.clone();
            record["quickCreate"] = json!(true);
            record["quickCreateRequest"] =
                json!(lead_signature(&intent, "worktree", root.to_str()));
            store
                .transaction(|tx| {
                    tx.execute(
                        "INSERT INTO runtime_agents VALUES(?1,?2)",
                        rusqlite::params![scenario, record.to_string()],
                    )?;
                    Ok(())
                })
                .expect("seed existing agent");
            let interleaved = Cell::new(false);
            let after_interleaving = RefCell::new(None);
            let validate = |schema: &str, _: &Value| {
                if schema == "PersistedAgentRecord" && !interleaved.replace(true) {
                    let mut changed = record.clone();
                    match scenario {
                        "agent" => changed["creationIntent"]["prompt"] = json!("Another request"),
                        "lead" => changed["quickCreateRequest"] = json!("{}"),
                        _ => changed["deletedAt"] = json!(1),
                    }
                    store
                        .transaction(|tx| {
                            tx.execute(
                                "UPDATE runtime_agents SET record=?1 WHERE id=?2",
                                rusqlite::params![changed.to_string(), scenario],
                            )?;
                            Ok(())
                        })
                        .expect("concurrent change after initial read");
                    *after_interleaving.borrow_mut() = Some(snapshot(&store));
                }
                true
            };
            let context = Context {
                store: &store,
                accounts: None,
                driver: None,
                validate: &validate,
            };
            let request = Creation {
                id: Some(
                    if scenario == "previous" {
                        "new-agent"
                    } else {
                        scenario
                    }
                    .to_owned(),
                ),
                cwd: root.to_str().map(str::to_owned),
                account: Some("default".to_owned()),
                prompt: if scenario == "agent" { "Task" } else { "" }.to_owned(),
                previous: (scenario == "previous").then(|| scenario.to_owned()),
                reuse_empty: Some(false),
                intent,
                ..Creation::default()
            };
            let result = if scenario == "agent" {
                agent(&context, request)
            } else {
                lead(&context, request)
            };
            assert!(
                matches!(
                    result,
                    Err(OperationError::Create(
                        MutationError::Conflict | MutationError::Deleted
                    ))
                ),
                "{scenario}: {result:?}"
            );
            assert!(interleaved.get());
            assert_eq!(
                Some(snapshot(&store)),
                *after_interleaving.borrow(),
                "refusal writes nothing after {scenario} interleaving"
            );
        }
        drop(store);
        std::fs::remove_dir_all(root).expect("remove temporary state");
    }
}
