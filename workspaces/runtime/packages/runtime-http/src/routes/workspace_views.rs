use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use studio_runtime_services::workspace_views::{CursorKind, parse_task_cursor, task_feed_response};
use studio_runtime_store::WorkspaceViewsRepository;

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

async fn workspace(query: QueryParams) -> Result<Json<Value>, ApiError> {
    // The shared boundary has already validated the complete query schema.
    let _ = query;
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
            .map_err(|_| invalid_stored("runtime_tasks"))?
            .ok_or_else(invalid_request)?;
        let agent_id = task
            .get("agent")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_stored("runtime_tasks"))?;
        let agent = WorkspaceViewsRepository::agent(&connection, agent_id)
            .map_err(|_| invalid_stored("runtime_agents"))?
            .ok_or_else(|| invalid_stored("runtime_agents"))?;
        if agent
            .get("deletedAt")
            .is_some_and(|value| !value.is_null() && !is_false(value))
        {
            return Err(invalid_request());
        }
        generated_json::<crate::generated::TaskDetailResponse>(task)
            .map_err(|_| invalid_stored("runtime_tasks"))
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(task))
}

async fn workspace_tasks(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let agent = query
        .typed_value("agent")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let limit = query
        .typed_value("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 100) as usize;
    let cursor = parse_task_cursor(
        query.typed_value("cursor").and_then(Value::as_str),
        CursorKind::Current,
    )
    .map_err(|_| invalid_cursor("cursor"))?;
    let before = parse_task_cursor(
        query.typed_value("before").and_then(Value::as_str),
        CursorKind::Before,
    )
    .map_err(|_| invalid_cursor("before"))?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let root = if let Some(agent_id) = agent.as_deref() {
            let record = WorkspaceViewsRepository::agent(&connection, agent_id)
                .map_err(|_| invalid_stored("runtime_agents"))?
                .ok_or_else(invalid_request)?;
            if record
                .get("deletedAt")
                .is_some_and(|value| !value.is_null() && !is_false(value))
            {
                return Err(invalid_request());
            }
            Some(
                record
                    .get("rootId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| invalid_stored("runtime_agents"))?
                    .to_owned(),
            )
        } else {
            None
        };
        let ids = WorkspaceViewsRepository::live_agent_ids(&connection, root.as_deref())
            .map_err(|_| invalid_stored("runtime_agents"))?;
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
        .map_err(|_| invalid_stored("runtime_tasks"))?;
        let rows_returned = page.len();
        let selected = page
            .iter()
            .take(limit)
            .map(|row| row.record.clone())
            .collect::<Vec<_>>();
        let summaries = selected
            .into_iter()
            .map(|record| {
                let mut detail = generated_json::<crate::generated::TaskDetailResponse>(record)
                    .map_err(|_| invalid_stored("runtime_tasks"))?;
                if let Value::Object(object) = &mut detail {
                    object.remove("tail");
                    object.remove("arguments");
                    object.remove("error");
                }
                generated_json::<crate::generated::WorkspaceTaskRecord>(detail)
                    .map_err(|_| invalid_stored("runtime_tasks"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let sort_value = page.get(limit.saturating_sub(1)).map(|row| row.sort_value);
        Ok(task_feed_response(
            summaries,
            rows_returned,
            cursor.as_ref().filter(|_| before.is_none()),
            before.as_ref(),
            limit,
            sort_value,
        ))
    })
    .await
    .map_err(|_| ApiError::internal())??;
    generated_json::<crate::generated::WorkspaceTaskFeed>(result).map(Json)
}

fn generated_json<T>(value: Value) -> Result<Value, ApiError>
where
    T: DeserializeOwned + Serialize,
{
    let dto = serde_json::from_value::<T>(value).map_err(|_| ApiError::internal())?;
    serde_json::to_value(dto).map_err(|_| ApiError::internal())
}

fn is_false(value: &Value) -> bool {
    match value {
        Value::Bool(value) => !value,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::String(value) => value.is_empty(),
        Value::Array(values) => values.is_empty(),
        Value::Object(values) => values.is_empty(),
        Value::Null => false,
    }
}

fn invalid_request() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "Invalid request")
}

fn invalid_stored(object: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object {object}"),
    )
}

fn invalid_cursor(parameter: &str) -> ApiError {
    ApiError::validation(vec![json!("query"), json!(parameter)])
}

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}
