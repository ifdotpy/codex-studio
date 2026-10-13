use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use studio_runtime_services::work_policy;
use studio_runtime_services::workspace_files::WorkspaceContext;
use studio_runtime_services::workspace_views::{
    CursorKind, GitError, git_changes as service_git_changes, parse_task_cursor,
    reported_change_files, task_feed_response,
};
use studio_runtime_store::{
    Connection, StoredRecord, TranscriptRepository, WorkspaceViewsRepository,
};

use crate::error::ApiError;
use crate::router::HttpState;
use crate::validation::Registry;

use super::extractors::QueryParams;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get};

const CHANGES_DEPENDENCIES: [Dependency; 1] = [Dependency::new("workspace-state/image-execution")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/changes",
            OperationStatus::partial(&CHANGES_DEPENDENCIES),
            changes,
        ),
        get("/api/task", OperationStatus::Implemented, task_detail),
        get("/api/workspace", OperationStatus::Implemented, workspace),
        get(
            "/api/workspace/tasks",
            OperationStatus::Implemented,
            workspace_tasks,
        ),
    ])
}

async fn changes(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let agent = query.nonempty_str("agent").map(str::to_owned);
    let scope = query.nonempty_str("scope").map(str::to_owned);
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || {
        let mut connection = store.read().map_err(|_| ApiError::internal())?;
        let transaction = connection.transaction().map_err(|_| ApiError::internal())?;
        let actor_id = agent.as_deref().ok_or_else(invalid_request)?;
        let actor = WorkspaceViewsRepository::agent(&transaction, actor_id)
            .map_err(|_| invalid_stored("runtime_agents", actor_id))?
            .ok_or_else(invalid_request)?;
        let actor = checked_stored(
            &state.validators,
            "AgentRecordResponse",
            "runtime_agents",
            actor_id,
            actor,
        )?;
        if deleted(&actor) {
            return Err(invalid_request());
        }
        let result = if scope.as_deref() == Some("chat") {
            reported_changes(&transaction, actor_id)?
        } else {
            let context = WorkspaceContext::from_agent(&actor, actor_id)
                .map_err(|_| invalid_stored("runtime_agents", actor_id))?;
            if context.execution_backed {
                return Err(dependency_error(
                    "GET",
                    "/api/changes",
                    &CHANGES_DEPENDENCIES,
                ));
            }
            git_changes(&context.cwd)?
        };
        transaction.commit().map_err(|_| ApiError::internal())?;
        Ok::<Value, ApiError>(result)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(super::generated_json::<
        crate::generated::ChangesResponse,
    >(value)?))
}

async fn workspace(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let agent = query.nonempty_str("agent").map(str::to_owned);
    let view = query.nonempty_str("view").unwrap_or("full").to_owned();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let value = tokio::task::spawn_blocking(move || {
        let mut connection = store.read().map_err(|_| ApiError::internal())?;
        let transaction = connection.transaction().map_err(|_| ApiError::internal())?;
        let value = workspace_view(&transaction, &validators, agent.as_deref(), &view)?;
        transaction.commit().map_err(|_| ApiError::internal())?;
        Ok::<Value, ApiError>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(super::generated_json::<
        crate::generated::WorkspaceView,
    >(value)?))
}

fn git_changes(cwd: &str) -> Result<Value, ApiError> {
    service_git_changes(cwd).map_err(|error| match error {
        GitError::TimedOut | GitError::Other | GitError::OutputTooLarge | GitError::Failed(_) => {
            ApiError::internal()
        }
    })
}

fn reported_changes(connection: &Connection, agent: &str) -> Result<Value, ApiError> {
    let prefix = format!("{agent}:turn/diff/updated:");
    let items = TranscriptRepository::items_for_agent_including_restored(connection, agent)
        .map_err(|_| invalid_stored("runtime_items", agent))?;
    let Some(item) = items.into_iter().find(|item| {
        item.id == format!("{agent}:turn/diff/updated") || item.id.starts_with(&prefix)
    }) else {
        return Ok(empty_reported_changes());
    };
    if truthy(item.record.get("afterRestore")) {
        return Ok(empty_reported_changes());
    }
    let text = TranscriptRepository::full_text(connection, &item)
        .map_err(|_| invalid_stored("runtime_items", &item.id))?;
    if item.record.get("truncated").and_then(Value::as_bool) == Some(true) && text.is_none() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "The complete reported changes are unavailable",
        ));
    }
    let raw = text.or_else(|| {
        item.record
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let payload: Value = serde_json::from_str(raw.as_deref().unwrap_or_default())
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "The recorded changes are invalid"))?;
    let diff = payload.get("diff").and_then(Value::as_str).ok_or_else(|| {
        ApiError::new(StatusCode::BAD_REQUEST, "The recorded changes are invalid")
    })?;
    let files = reported_change_files(diff);
    let truncated = diff.chars().count() > 300_000;
    let diff = diff.chars().take(300_000).collect::<String>();
    Ok(json!({
        "scope":"chat", "git":true, "files":files, "diff":diff, "patch":diff,
        "truncated":truncated, "turnId":payload.get("turnId"), "reportedAt":item.record.get("at"),
    }))
}

fn empty_reported_changes() -> Value {
    json!({
        "scope":"chat", "git":true, "files":[], "diff":"", "patch":"",
        "truncated":false, "turnId":null, "reportedAt":null,
    })
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Null) | None => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

fn workspace_view(
    connection: &Connection,
    validators: &Registry,
    agent_id: Option<&str>,
    view: &str,
) -> Result<Value, ApiError> {
    let stored_agents = WorkspaceViewsRepository::agents(connection)
        .map_err(|_| invalid_stored("runtime_agents", "unknown"))?;
    let agents = parse_records(
        validators,
        "AgentRecordResponse",
        "runtime_agents",
        stored_agents,
    )?;
    let root = studio_runtime_services::workspace_views::agent_root(&agents, agent_id).map_err(
        |error| match error {
            studio_runtime_services::workspace_views::AgentScopeError::MissingAgent => {
                invalid_request()
            }
            studio_runtime_services::workspace_views::AgentScopeError::MissingRoot => {
                invalid_stored("runtime_agents", agent_id.unwrap_or("unknown"))
            }
        },
    )?;
    let all_agents = agents.clone();
    let agents = studio_runtime_services::workspace_views::agents_in_root(&agents, root.as_deref())
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let ids = agents
        .iter()
        .map(|agent| {
            agent["id"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid_stored("runtime_agents", "unknown"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    if view == "annotations" {
        let annotations = parse_records(
            validators,
            "AnnotationReceipt",
            "runtime_annotations",
            WorkspaceViewsRepository::records_for_agents(
                connection,
                "runtime_annotations",
                "agent",
                &ids,
            )
            .map_err(|_| invalid_stored("runtime_annotations", "unknown"))?,
        )?;
        return Ok(
            studio_runtime_services::workspace_views::workspace_view_response(
                view,
                studio_runtime_services::workspace_views::WorkspaceViewData {
                    annotations,
                    ..Default::default()
                },
            ),
        );
    }

    let all_work = parse_records(
        validators,
        "WorkItem",
        "runtime_work",
        WorkspaceViewsRepository::records(connection, "runtime_work")
            .map_err(|_| invalid_stored("runtime_work", "unknown"))?,
    )?;
    let statuses = work_policy::work_statuses(&all_work);
    let root_view = root.as_deref().unwrap_or("");
    let work = all_work
        .iter()
        .filter(|item| {
            ids.contains(&item["rootId"].as_str().unwrap_or_default().to_owned())
                && (root.is_none() || item["rootId"] == root_view)
        })
        .map(|item| work_policy::work_item_view(item, &statuses))
        .collect::<Vec<_>>();
    if view == "work" {
        return Ok(
            studio_runtime_services::workspace_views::workspace_view_response(
                view,
                studio_runtime_services::workspace_views::WorkspaceViewData {
                    work,
                    ..Default::default()
                },
            ),
        );
    }

    let requests = parse_records(
        validators,
        "RequestRecordResponse",
        "runtime_requests",
        WorkspaceViewsRepository::records_for_agents(connection, "runtime_requests", "agent", &ids)
            .map_err(|_| invalid_stored("runtime_requests", "unknown"))?,
    )?;
    let complaints = if root.is_some() {
        parse_records(
            validators,
            "ComplaintRecordResponse",
            "runtime_complaints",
            WorkspaceViewsRepository::records_for_agents(
                connection,
                "runtime_complaints",
                "leadId",
                &ids,
            )
            .map_err(|_| invalid_stored("runtime_complaints", "unknown"))?,
        )?
    } else {
        parse_records(
            validators,
            "ComplaintRecordResponse",
            "runtime_complaints",
            WorkspaceViewsRepository::records(connection, "runtime_complaints")
                .map_err(|_| invalid_stored("runtime_complaints", "unknown"))?,
        )?
    };
    let rules = parse_records(
        validators,
        "RuleRecordResponse",
        "runtime_rules",
        WorkspaceViewsRepository::records_for_agents(connection, "runtime_rules", "agent", &ids)
            .map_err(|_| invalid_stored("runtime_rules", "unknown"))?,
    )?;
    let tasks = parse_records(
        validators,
        "TaskDetailResponse",
        "runtime_tasks",
        WorkspaceViewsRepository::snapshot_tasks(connection, root.as_deref())
            .map_err(|_| invalid_stored("runtime_tasks", "unknown"))?,
    )?
    .into_iter()
    .map(|task| {
        let id = task["id"].as_str().unwrap_or("unknown");
        let projection = validators
            .project_entity("TaskEntityDto", &task)
            .ok_or_else(|| invalid_stored("runtime_tasks", id))?;
        if !validators.validate_stored("TaskEntityDto", &projection) {
            return Err(invalid_stored("runtime_tasks", id));
        }
        Ok(projection)
    })
    .collect::<Result<Vec<_>, ApiError>>()?;
    let monitors = parse_records(
        validators,
        "MonitorRecordResponse",
        "runtime_monitors",
        WorkspaceViewsRepository::snapshot_monitors(connection, root.as_deref())
            .map_err(|_| invalid_stored("runtime_monitors", "unknown"))?,
    )?
    .into_iter()
    .map(|monitor| {
        let id = monitor["id"].as_str().unwrap_or("unknown");
        let projection = validators
            .project_entity("MonitorEntityDto", &monitor)
            .ok_or_else(|| invalid_stored("runtime_monitors", id))?;
        if !validators.validate_stored("MonitorEntityDto", &projection) {
            return Err(invalid_stored("runtime_monitors", id));
        }
        Ok(projection)
    })
    .collect::<Result<Vec<_>, ApiError>>()?
    .into_iter()
    .filter(|monitor| {
        monitor["agent"]
            .as_str()
            .is_some_and(|agent| ids.iter().any(|id| id == agent))
    })
    .collect::<Vec<_>>();
    let inbox = studio_runtime_services::workspace_views::workspace_inbox(
        studio_runtime_services::workspace_views::WorkspaceInboxData {
            agents: &agents,
            all_agents: &all_agents,
            requests: &requests,
            complaints: &complaints,
            work: &work,
            statuses: &statuses,
            monitors: &monitors,
            rules: &rules,
        },
    );
    if view == "inbox" {
        return Ok(
            studio_runtime_services::workspace_views::workspace_view_response(
                view,
                studio_runtime_services::workspace_views::WorkspaceViewData {
                    inbox,
                    ..Default::default()
                },
            ),
        );
    }

    let annotations = parse_records(
        validators,
        "AnnotationReceipt",
        "runtime_annotations",
        WorkspaceViewsRepository::records_for_agents(
            connection,
            "runtime_annotations",
            "agent",
            &ids,
        )
        .map_err(|_| invalid_stored("runtime_annotations", "unknown"))?,
    )?;
    let checkpoint_ids = agent_id
        .map(|id| vec![id.to_owned()])
        .unwrap_or_else(|| ids.clone());
    let checkpoint_records = parse_records(
        validators,
        "CheckpointSummary",
        "runtime_checkpoints",
        WorkspaceViewsRepository::records_for_agents(
            connection,
            "runtime_checkpoints",
            "agent",
            &checkpoint_ids,
        )
        .map_err(|_| invalid_stored("runtime_checkpoints", "unknown"))?,
    )?;
    let agent_records = all_agents
        .iter()
        .map(|agent| {
            (
                agent["id"].as_str().unwrap_or_default().to_owned(),
                agent.as_object().cloned().unwrap_or_default(),
            )
        })
        .collect();
    let checkpoint_records = checkpoint_records
        .into_iter()
        .map(|record| {
            (
                record["id"].as_str().unwrap_or_default().to_owned(),
                record.as_object().cloned().unwrap_or_default(),
            )
        })
        .collect();
    let checkpoints = studio_runtime_services::questions_reads::checkpoint_list(
        agent_id,
        agent_records,
        checkpoint_records,
    )
    .map_err(|_| invalid_stored("runtime_checkpoints", "unknown"))?["checkpoints"]
        .as_array()
        .cloned()
        .ok_or_else(|| invalid_stored("runtime_checkpoints", "unknown"))?;
    let plans = parse_records(
        validators,
        "PlanView",
        "runtime_plans",
        WorkspaceViewsRepository::records_for_agents(connection, "runtime_plans", "id", &ids)
            .map_err(|_| invalid_stored("runtime_plans", "unknown"))?,
    )?
    .into_iter()
    .filter(|plan| root.as_ref().is_none_or(|root| plan["rootId"] == *root))
    .collect::<Vec<_>>();
    Ok(
        studio_runtime_services::workspace_views::workspace_view_response(
            view,
            studio_runtime_services::workspace_views::WorkspaceViewData {
                work,
                annotations,
                checkpoints,
                plans,
                rules,
                inbox,
                tasks,
                monitors,
            },
        ),
    )
}

fn parse_records(
    validators: &Registry,
    schema: &str,
    table: &str,
    records: Vec<StoredRecord>,
) -> Result<Vec<Value>, ApiError> {
    records
        .into_iter()
        .map(|row| {
            let raw = row.record.ok_or_else(|| invalid_stored(table, &row.id))?;
            let value: Value =
                serde_json::from_str(&raw).map_err(|_| invalid_stored(table, &row.id))?;
            checked_stored(validators, schema, table, &row.id, value)
        })
        .collect()
}

async fn task_detail(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let id = query
        .typed_value("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let (store, validators) = (
        state.store.clone().ok_or_else(ApiError::internal)?,
        state.validators.clone(),
    );
    let task = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let task = WorkspaceViewsRepository::task(&connection, &id)
            .map_err(|_| invalid_stored("runtime_tasks", &id))?
            .ok_or_else(invalid_request)?;
        let task = checked_stored(
            &validators,
            "TaskDetailResponse",
            "runtime_tasks",
            &id,
            task,
        )?;
        let agent_id = task["agent"]
            .as_str()
            .ok_or_else(|| invalid_stored("runtime_tasks", &id))?;
        let agent = WorkspaceViewsRepository::agent(&connection, agent_id)
            .map_err(|_| invalid_stored("runtime_agents", agent_id))?
            .ok_or_else(|| invalid_stored("runtime_agents", agent_id))?;
        let agent = checked_stored(
            &validators,
            "AgentRecordResponse",
            "runtime_agents",
            agent_id,
            agent,
        )?;
        if deleted(&agent) {
            return Err(invalid_request());
        }
        Ok(task)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(task))
}

async fn workspace_tasks(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let agent = query.nonempty_str("agent").map(str::to_owned);
    let limit = query
        .typed_value("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 100) as usize;
    let cursor = parse_task_cursor(query.nonempty_str("cursor"), CursorKind::Current)
        .map_err(|_| invalid_cursor("cursor"))?;
    let before = parse_task_cursor(query.nonempty_str("before"), CursorKind::Before)
        .map_err(|_| invalid_cursor("before"))?;
    let (store, validators) = (
        state.store.clone().ok_or_else(ApiError::internal)?,
        state.validators.clone(),
    );
    let result = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        if let Some(id) = WorkspaceViewsRepository::first_invalid_task_identity(&connection)
            .map_err(|_| invalid_stored("runtime_tasks", "unknown"))?
        {
            return Err(invalid_stored("runtime_tasks", &id));
        }

        let agents = WorkspaceViewsRepository::agent_ids(&connection)
            .map_err(|_| invalid_stored("runtime_agents", "unknown"))?
            .into_iter()
            .map(|id| {
                let record = WorkspaceViewsRepository::agent(&connection, &id)
                    .map_err(|_| invalid_stored("runtime_agents", &id))?
                    .ok_or_else(|| invalid_stored("runtime_agents", &id))?;
                checked_stored(
                    &validators,
                    "AgentRecordResponse",
                    "runtime_agents",
                    &id,
                    record,
                )
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        let _root = studio_runtime_services::workspace_views::agent_root(&agents, agent.as_deref())
            .map_err(|error| match error {
                studio_runtime_services::workspace_views::AgentScopeError::MissingAgent => {
                    invalid_request()
                }
                studio_runtime_services::workspace_views::AgentScopeError::MissingRoot => {
                    invalid_stored("runtime_agents", agent.as_deref().unwrap_or("unknown"))
                }
            })?;
        let ids =
            studio_runtime_services::workspace_views::agents_in_root(&agents, _root.as_deref())
                .into_iter()
                .filter_map(|record| record["id"].as_str().map(str::to_owned))
                .collect::<Vec<_>>();
        let page = WorkspaceViewsRepository::task_page(
            &connection,
            &ids,
            cursor
                .as_ref()
                .filter(|_| before.is_none())
                .map(|value| (value.time, value.id.as_str())),
            before.as_ref().map(|value| (value.time, value.id.as_str())),
            limit,
        )
        .map_err(|_| invalid_stored("runtime_tasks", "unknown"))?;
        let has_more = page.len() > limit;
        let tasks = page
            .into_iter()
            .take(limit)
            .map(|row| {
                let record = WorkspaceViewsRepository::task(&connection, &row.id)
                    .map_err(|_| invalid_stored("runtime_tasks", &row.id))?
                    .ok_or_else(|| invalid_stored("runtime_tasks", &row.id))?;
                let record = checked_stored(
                    &validators,
                    "TaskDetailResponse",
                    "runtime_tasks",
                    &row.id,
                    record,
                )?;
                let mut summary = record;
                if let Some(record) = summary.as_object_mut() {
                    record.remove("tail");
                    record.remove("arguments");
                    record.remove("error");
                }
                Ok(summary)
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        Ok(task_feed_response(
            tasks,
            has_more,
            cursor.as_ref(),
            before.as_ref(),
        ))
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

fn checked_stored(
    validators: &Registry,
    schema: &str,
    table: &str,
    id: &str,
    value: Value,
) -> Result<Value, ApiError> {
    if !validators.valid_stored_record(schema, id, &value) {
        Err(invalid_stored(table, id))
    } else {
        Ok(value)
    }
}

fn deleted(agent: &Value) -> bool {
    studio_runtime_services::workspace_views::deleted_agent(agent)
}

fn invalid_request() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "Invalid request")
}

fn invalid_stored(object: &str, id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object {object} row {id}"),
    )
}

fn invalid_cursor(parameter: &str) -> ApiError {
    ApiError::validation(vec![json!("query"), json!(parameter)])
}

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "workspace_views",
        routes: Some(routes),
        dependencies: &["coordination-policy/inbox", "work-policy/views", "workspace-state/git-changes", "workspace-state/snapshot"],
    }
}
