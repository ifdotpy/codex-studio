use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use studio_runtime_services::workspace_views::{CursorKind, parse_task_cursor, task_feed_response};
use studio_runtime_store::{StoreError, WorkspaceViewsRepository};

use crate::error::ApiError;
use crate::router::HttpState;

use super::extractors::QueryParams;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get};

const SNAPSHOT_DEPENDENCIES: [Dependency; 3] = [
    Dependency::new("workspace-state/snapshot"),
    Dependency::new("work-policy/views"),
    Dependency::new("coordination-policy/inbox"),
];
const CHANGES_DEPENDENCIES: [Dependency; 1] = [Dependency::new("workspace-state/git-changes")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/changes",
            OperationStatus::partial(&CHANGES_DEPENDENCIES),
            changes,
        ),
        get("/api/task", OperationStatus::Implemented, task_detail),
        get(
            "/api/workspace",
            OperationStatus::partial(&SNAPSHOT_DEPENDENCIES),
            workspace,
        ),
        get(
            "/api/workspace/tasks",
            OperationStatus::Implemented,
            workspace_tasks,
        ),
    ])
}

async fn changes() -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "GET",
        "/api/changes",
        &CHANGES_DEPENDENCIES,
    ))
}

async fn workspace() -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "GET",
        "/api/workspace",
        &SNAPSHOT_DEPENDENCIES,
    ))
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
    let task = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let task = WorkspaceViewsRepository::task(&connection, &id)
            .map_err(map_store_error)?
            .ok_or_else(invalid_request)?;
        let agent_id = task["agent"]
            .as_str()
            .ok_or_else(|| invalid_stored("runtime_tasks", &id))?;
        let agent = WorkspaceViewsRepository::agent(&connection, agent_id)
            .map_err(map_store_error)?
            .ok_or_else(|| invalid_stored("runtime_agents", agent_id))?;
        let agent = agent.value();
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
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        WorkspaceViewsRepository::validate_task_records(&connection).map_err(map_store_error)?;

        let agents = WorkspaceViewsRepository::agent_ids(&connection)
            .map_err(map_store_error)?
            .into_iter()
            .map(|id| {
                let record = WorkspaceViewsRepository::agent(&connection, &id)
                    .map_err(map_store_error)?
                    .ok_or_else(|| invalid_stored("runtime_agents", &id))?;
                let record = record.value();
                Ok((id, record))
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        let root = agent
            .as_deref()
            .map(|agent_id| {
                let record = agents
                    .iter()
                    .find(|(id, _)| id == agent_id)
                    .map(|(_, record)| record)
                    .ok_or_else(invalid_request)?;
                if deleted(record) {
                    return Err(invalid_request());
                }
                Ok(record["rootId"]
                    .as_str()
                    .ok_or_else(|| invalid_stored("runtime_agents", agent_id))?
                    .to_owned())
            })
            .transpose()?;
        let ids = agents
            .into_iter()
            .filter(|(_, record)| {
                !deleted(record) && root.as_ref().is_none_or(|root| record["rootId"] == *root)
            })
            .map(|(id, _)| id)
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
        .map_err(map_store_error)?;
        let has_more = page.len() > limit;
        let tasks = page
            .into_iter()
            .take(limit)
            .map(|row| {
                let record = WorkspaceViewsRepository::task(&connection, &row.id)
                    .map_err(map_store_error)?
                    .ok_or_else(|| invalid_stored("runtime_tasks", &row.id))?;
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

fn map_store_error(error: StoreError) -> ApiError {
    match error {
        StoreError::InvalidStoredObject { table, id } => invalid_stored(table, &id),
        _ => ApiError::internal(),
    }
}

fn deleted(agent: &Value) -> bool {
    agent
        .get("deletedAt")
        .and_then(Value::as_f64)
        .is_some_and(|value| value != 0.0)
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
