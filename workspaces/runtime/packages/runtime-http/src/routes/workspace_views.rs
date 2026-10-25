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
    AgentRecord, CheckpointRecord, Connection, StoreError, StoredRecord, TranscriptRepository,
    WorkspaceViewsRepository,
};

use crate::error::ApiError;
use crate::router::HttpState;
use crate::validation::Registry;

use super::extractors::QueryParams;
use super::manifest::{OperationStatus, RouteGroup, get};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/changes", OperationStatus::Implemented, changes),
        get("/api/task", OperationStatus::Implemented, task_detail),
        get(
            "/api/task/output",
            OperationStatus::Implemented,
            task_output,
        ),
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
) -> Result<Json<crate::generated::ChangesResponse>, ApiError> {
    let agent = query.nonempty_str("agent").map(str::to_owned);
    let scope = query.nonempty_str("scope").map(str::to_owned);
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    if scope.as_deref().is_some_and(|scope| scope != "chat") {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Unknown changes scope",
        ));
    }
    let actor_id = agent.ok_or_else(invalid_request)?;
    let chat = scope.is_some();
    // Reported changes come from the transcript; Git changes run after the read.
    let read = store
        .read_async("runtime-http/routes/workspace_views", move |transaction| {
            let actor = WorkspaceViewsRepository::agent(transaction, &actor_id)?
                .ok_or_else(invalid_request)?;
            let actor = actor.value().clone();
            if deleted(&actor) {
                return Err(invalid_request());
            }
            if studio_runtime_services::legacy_image_workspace::is_legacy(&actor) && !chat {
                let reason = "This legacy workspace is finished";
                return Ok(Ok(
                    json!({"files":[],"patch":"","git":false,"error":reason}),
                ));
            }
            if chat {
                return reported_changes(transaction, &actor_id).map(Ok);
            }
            let context = WorkspaceContext::from_agent(&actor, &actor_id)
                .map_err(|_| invalid_stored("runtime_agents", &actor_id))?;
            Ok(Err(context.cwd))
        })
        .await?;
    let value = match read {
        Ok(value) => value,
        Err(cwd) => studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/workspace_views",
            move || git_changes(&cwd),
        )
        .await
        .map_err(|_| ApiError::internal())??,
    };
    Ok(Json(super::generated_json::<
        crate::generated::ChangesResponse,
    >(value)?))
}

async fn workspace(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::WorkspaceView>, ApiError> {
    let agent = query.nonempty_str("agent").map(str::to_owned);
    let view = query.nonempty_str("view").unwrap_or("full").to_owned();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let value = store
        .read_async("runtime-http/routes/workspace_views", move |transaction| {
            workspace_view(transaction, &validators, agent.as_deref(), &view)
        })
        .await?;
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
        .map_err(map_transcript_error)?;
    let Some(item) = items.into_iter().find(|item| {
        item.id == format!("{agent}:turn/diff/updated") || item.id.starts_with(&prefix)
    }) else {
        return Ok(empty_reported_changes());
    };
    if studio_runtime_services::event_write::truthy(item.record.get("afterRestore")) {
        return Ok(empty_reported_changes());
    }
    let text = TranscriptRepository::full_text(connection, &item).map_err(map_transcript_error)?;
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

fn workspace_view(
    connection: &Connection,
    validators: &Registry,
    agent_id: Option<&str>,
    view: &str,
) -> Result<Value, ApiError> {
    let stored_agents = WorkspaceViewsRepository::agents(connection).map_err(ApiError::stored)?;
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
            .map_err(ApiError::stored)?,
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
        WorkspaceViewsRepository::records(connection, "runtime_work").map_err(ApiError::stored)?,
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
            .map_err(ApiError::stored)?,
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
            .map_err(ApiError::stored)?,
        )?
    } else {
        parse_records(
            validators,
            "ComplaintRecordResponse",
            "runtime_complaints",
            WorkspaceViewsRepository::records(connection, "runtime_complaints")
                .map_err(ApiError::stored)?,
        )?
    };
    let rules = parse_open_records(
        "runtime_rules",
        WorkspaceViewsRepository::records_for_agents(connection, "runtime_rules", "agent", &ids)
            .map_err(ApiError::stored)?,
    )?;
    let tasks = project_task_records(
        validators,
        WorkspaceViewsRepository::snapshot_tasks(connection, root.as_deref())
            .map_err(ApiError::stored)?,
    )?;
    let monitors = WorkspaceViewsRepository::snapshot_monitors(connection, root.as_deref())
        .map_err(ApiError::stored)?
        .into_iter()
        .map(|row| {
            let id = row.id;
            let raw = row
                .record
                .ok_or_else(|| invalid_stored("runtime_monitors", &id))?;
            let monitor: Value =
                serde_json::from_str(&raw).map_err(|_| invalid_stored("runtime_monitors", &id))?;
            if monitor.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                return Err(invalid_stored("runtime_monitors", &id));
            }
            let projection = validators
                .project_entity("monitor", &monitor)
                .ok_or_else(|| invalid_stored("runtime_monitors", &id))?;
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
        .map_err(ApiError::stored)?,
    )?;
    let checkpoint_ids = agent_id
        .map(|id| vec![id.to_owned()])
        .unwrap_or_else(|| ids.clone());
    let checkpoint_records = parse_open_records(
        "runtime_checkpoints",
        WorkspaceViewsRepository::records_for_agents(
            connection,
            "runtime_checkpoints",
            "agent",
            &checkpoint_ids,
        )
        .map_err(ApiError::stored)?,
    )?;
    let agent_records = all_agents
        .into_iter()
        .map(|agent| {
            let id = agent
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_stored("runtime_agents", "unknown"))?
                .to_owned();
            AgentRecord::from_value(agent).map_err(|_| invalid_stored("runtime_agents", &id))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    let checkpoint_records = checkpoint_records
        .into_iter()
        .map(|record| {
            let id = record
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid_stored("runtime_checkpoints", "unknown"))?
                .to_owned();
            CheckpointRecord::from_value(record)
                .map_err(|_| invalid_stored("runtime_checkpoints", &id))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
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
            .map_err(ApiError::stored)?,
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
    _validators: &Registry,
    _schema: &str,
    table: &str,
    records: Vec<StoredRecord>,
) -> Result<Vec<Value>, ApiError> {
    records
        .into_iter()
        .map(|row| {
            let raw = row.record.ok_or_else(|| invalid_stored(table, &row.id))?;
            let value: Value =
                serde_json::from_str(&raw).map_err(|_| invalid_stored(table, &row.id))?;
            if !value.is_object()
                || value.get("id").and_then(Value::as_str) != Some(row.id.as_str())
            {
                return Err(invalid_stored(table, &row.id));
            }
            Ok(value)
        })
        .collect()
}

fn parse_open_records(table: &str, records: Vec<StoredRecord>) -> Result<Vec<Value>, ApiError> {
    records
        .into_iter()
        .map(|row| {
            let raw = row.record.ok_or_else(|| invalid_stored(table, &row.id))?;
            let value: Value =
                serde_json::from_str(&raw).map_err(|_| invalid_stored(table, &row.id))?;
            if !value.is_object()
                || value.get("id").and_then(Value::as_str) != Some(row.id.as_str())
            {
                return Err(invalid_stored(table, &row.id));
            }
            Ok(value)
        })
        .collect()
}

fn project_task_records(
    validators: &Registry,
    records: Vec<StoredRecord>,
) -> Result<Vec<Value>, ApiError> {
    records
        .into_iter()
        .map(|row| {
            let id = row.id;
            let raw = row
                .record
                .ok_or_else(|| invalid_stored("runtime_tasks", &id))?;
            let task: Value =
                serde_json::from_str(&raw).map_err(|_| invalid_stored("runtime_tasks", &id))?;
            if task.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                return Err(invalid_stored("runtime_tasks", &id));
            }
            let projection = validators
                .project_entity("task", &task)
                .ok_or_else(|| invalid_stored("runtime_tasks", &id))?;
            Ok(projection)
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
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let task = store
        .read_async("runtime-http/routes/workspace_views", move |connection| {
            let task = WorkspaceViewsRepository::task(connection, &id)
                .map_err(map_store_error)?
                .ok_or_else(invalid_request)?;
            let agent_id = task["agent"]
                .as_str()
                .ok_or_else(|| invalid_stored("runtime_tasks", &id))?;
            let agent = WorkspaceViewsRepository::agent(connection, agent_id)
                .map_err(map_store_error)?
                .ok_or_else(|| invalid_stored("runtime_agents", agent_id))?;
            let agent = agent.value();
            if deleted(&agent) {
                return Err(invalid_request());
            }
            Ok(task)
        })
        .await?;
    Ok(Json(task))
}

async fn task_output(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let id = query
        .nonempty_str("id")
        .ok_or_else(invalid_request)?
        .to_owned();
    let owner = query
        .nonempty_str("agent")
        .ok_or_else(invalid_request)?
        .to_owned();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let (task, agent) = store
        .read_async("task-output/owner", move |connection| {
            let task = WorkspaceViewsRepository::task(connection, &id)?
                .filter(|task| task["agent"] == owner)
                .ok_or_else(invalid_request)?;
            let agent =
                WorkspaceViewsRepository::agent(connection, &owner)?.ok_or_else(invalid_request)?;
            Ok::<_, ApiError>((task, agent.value().clone()))
        })
        .await?;
    let owner = agent["id"].as_str().ok_or_else(invalid_request)?.to_owned();
    let value =
        studio_runtime_process::blocking_pool::spawn_blocking("task-output/read", move || {
            studio_runtime_services::task_output::read(&task, &agent, &owner)
        })
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|message| ApiError::new(StatusCode::BAD_REQUEST, message))?;
    Ok(Json(value))
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
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let result = store
        .read_async(
            "runtime-http/routes/workspace_views",
            move |connection| -> Result<Value, ApiError> {
                // Each served row is decoded below; rows outside the page are not read.
                let agents = parse_records(
                    &validators,
                    "AgentRecordResponse",
                    "runtime_agents",
                    WorkspaceViewsRepository::agents(connection).map_err(map_store_error)?,
                )?;
                let root =
                    studio_runtime_services::workspace_views::agent_root(&agents, agent.as_deref())
                        .map_err(|error| {
                            match error {
                        studio_runtime_services::workspace_views::AgentScopeError::MissingAgent => {
                            invalid_request()
                        }
                        studio_runtime_services::workspace_views::AgentScopeError::MissingRoot => {
                            invalid_stored("runtime_agents", agent.as_deref().unwrap_or("unknown"))
                        }
                        }
                        })?;
                let ids = studio_runtime_services::workspace_views::agents_in_root(
                    &agents,
                    root.as_deref(),
                )
                .into_iter()
                .filter_map(|record| record["id"].as_str().map(str::to_owned))
                .collect::<Vec<_>>();
                let page = WorkspaceViewsRepository::task_page(
                    connection,
                    &ids,
                    cursor
                        .as_ref()
                        .filter(|_| before.is_none())
                        .map(|value| (value.time, value.id.as_str())),
                    before.as_ref().map(|value| (value.time, value.id.as_str())),
                    limit,
                )
                .map_err(map_store_error)?;
                let has_more = page.len() > limit;
                let tasks = page
                    .into_iter()
                    .take(limit)
                    .map(|row| {
                        let record = row.decode(connection).map_err(map_store_error)?;
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
            },
        )
        .await?;
    Ok(Json(result))
}

fn map_store_error(error: StoreError) -> ApiError {
    ApiError::stored(error)
}

fn map_transcript_error(error: studio_runtime_store::TranscriptStoreError) -> ApiError {
    use studio_runtime_store::TranscriptStoreError;
    match error {
        TranscriptStoreError::InvalidStoredObject { table, id } => invalid_stored(table, &id),
        TranscriptStoreError::Store(error) => ApiError::stored(error),
        TranscriptStoreError::Sqlite(_) | TranscriptStoreError::ActiveTurn => ApiError::internal(),
    }
}

fn deleted(agent: &Value) -> bool {
    studio_runtime_services::workspace_views::deleted_agent(agent)
}

fn invalid_request() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "Invalid request")
}

fn invalid_stored(object: &str, id: &str) -> ApiError {
    ApiError::invalid_stored(object, id)
}

fn invalid_cursor(parameter: &str) -> ApiError {
    ApiError::validation(vec![json!("query"), json!(parameter)])
}

#[cfg(test)]
mod tests {
    use super::map_store_error;
    use crate::error::{InternalErrorCause, InternalErrorKind};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use studio_runtime_store::StoreError;

    #[tokio::test]
    async fn task_page_http_keeps_summary_and_ignores_invalid_lookahead() {
        use axum::body::{Body, to_bytes};
        use axum::extract::ConnectInfo;
        use axum::http::Request;
        use serde_json::{Value, json};
        use std::sync::Arc;
        use studio_runtime_store::{RuntimeStore, StoreError};
        use tower::ServiceExt;

        let root = tempfile::tempdir().expect("temporary runtime");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        store.transaction_with(|tx| {
            tx.execute("INSERT INTO runtime_agents VALUES('agent',?1)", [json!({"id":"agent","rootId":"agent","epoch":0,"status":"idle"}).to_string()])?;
            for (id, created) in [("a", 1), ("b", 2), ("c", 3)] {
                tx.execute("INSERT INTO runtime_tasks VALUES(?1,?2)", (id, json!({"id":if id == "a" { "wrong" } else { id },"agent":"agent","created":created,"tail":"large tail","arguments":{},"error":"detail"}).to_string()))?;
            }
            Ok::<_, StoreError>(())
        }).await.expect("seed");
        let state = crate::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            store.root().to_string_lossy().into(),
            49171,
            None,
            Some(store),
            None,
        );
        let app = crate::router(state).expect("router");
        let request = Request::builder()
            .uri("/api/workspace/tasks?agent=agent&limit=2")
            .header("host", "127.0.0.1:49171")
            .header("x-canvas-token", "fixture-token")
            .header(crate::API_SCHEMA_HASH_HEADER, crate::api_schema_hash())
            .extension(ConnectInfo(
                "127.0.0.1:12345"
                    .parse::<std::net::SocketAddr>()
                    .expect("address"),
            ))
            .body(Body::empty())
            .expect("request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body");
        let value: Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(value["tasks"][0]["id"], "c");
        assert_eq!(value["tasks"][1]["id"], "b");
        assert_eq!(value["hasMore"], true);
        assert_eq!(value["nextBefore"], json!({"created":2,"id":"b"}));
        assert_eq!(value["cursor"], json!({"updated":3.0,"id":"c"}));
        for task in value["tasks"].as_array().expect("tasks") {
            assert!(task.get("tail").is_none());
            assert!(task.get("arguments").is_none());
            assert!(task.get("error").is_none());
        }
    }

    #[test]
    fn workspace_store_mapping_keeps_database_failures_internal() {
        let malformed = map_store_error(StoreError::InvalidStoredObject {
            table: "runtime_agents",
            id: "agent-7".to_owned(),
        })
        .into_response();
        assert_eq!(malformed.status(), StatusCode::SERVICE_UNAVAILABLE);

        let database_failure =
            map_store_error(StoreError::Sqlite(rusqlite::Error::InvalidQuery)).into_response();
        assert_eq!(
            database_failure
                .extensions()
                .get::<InternalErrorCause>()
                .map(|cause| cause.0),
            Some(InternalErrorKind::StoreDatabase)
        );
    }

    #[test]
    fn a_result_from_before_result_files_keeps_the_workspace_view_available() {
        // Results submitted before result files existed keep their text inline.
        let legacy = serde_json::json!({
            "id": "result-1", "agent": "worker-1", "text": "done", "checks": "tests",
            "revision": "abc", "files": [], "created": 1788712208.47
        });
        let view = serde_json::json!({
            "work": [{
                "id": "task-1", "rootId": "lead-1", "title": "Task", "status": "accepted",
                "created": 1.0, "updated": 2.0, "version": 3, "results": [legacy]
            }],
            "annotations": [], "checkpoints": [], "plans": [], "rules": [], "inbox": [],
            "tasks": [], "tasksHistoryLimit": 100, "monitors": []
        });
        super::super::generated_json::<crate::generated::WorkspaceView>(view)
            .expect("a legacy result without resultFile is part of the view");
    }

    #[test]
    fn review_work_from_before_creator_tracking_keeps_the_workspace_view_available() {
        // Work written before createdBy existed reaches the inbox without a creator.
        let legacy = serde_json::json!({
            "id": "work-1", "rootId": "lead-1", "title": "Task", "description": "Old row",
            "status": "review", "created": 1.0, "updated": 2.0, "version": 3,
            "dependencies": [], "decisions": [], "results": [],
            "blockedBy": [], "displayStatus": "review"
        });
        let view = serde_json::json!({
            "work": [legacy.clone()], "annotations": [], "checkpoints": [], "plans": [],
            "rules": [], "tasks": [], "tasksHistoryLimit": 100, "monitors": [],
            "inbox": [{
                "id": "work-1", "kind": "work", "agent": "lead-1", "title": "Task",
                "text": "Result awaits acceptance", "work": legacy
            }]
        });
        let typed = super::super::generated_json::<crate::generated::WorkspaceView>(view)
            .expect("review work without createdBy is part of the view");
        let wire = serde_json::to_value(typed).expect("the view serializes");
        assert!(wire["inbox"][0]["work"].get("createdBy").is_none());
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "workspace_views",
        routes: Some(routes),
        dependencies: &[
            "coordination-policy/inbox",
            "work-policy/views",
            "workspace-state/git-changes",
            "workspace-state/snapshot",
        ],
    }
}
