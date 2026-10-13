//! File metadata and bounded content reads for local workspaces.

use super::extractors::ApiJson;
use super::extractors::QueryParams;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use base64::Engine;
use serde_json::{Value, json};
use studio_runtime_services::payload_store::{self, PayloadStoreError};
use studio_runtime_services::workspace_files::{self, WorkspaceContext, WorkspaceFilesError};
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore, WorkspaceFilesRepository};

use crate::error::ApiError;
use crate::router::HttpState;

const IMAGE_EXECUTION_DEPENDENCY: &str = "workspace-state/image-execution";
const EXECUTION_DEPENDENCIES: [Dependency; 1] = [Dependency::new(IMAGE_EXECUTION_DEPENDENCY)];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post("/api/assets", OperationStatus::Implemented, upload_asset),
        get(
            "/api/file-info",
            OperationStatus::partial(&EXECUTION_DEPENDENCIES),
            file_info,
        ),
        get(
            "/api/file",
            OperationStatus::partial(&EXECUTION_DEPENDENCIES),
            file_content,
        ),
    ])
}

async fn upload_asset(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let agent = body
        .get("agent")
        .and_then(Value::as_str)
        .ok_or_else(ApiError::internal)?
        .to_owned();
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(ApiError::internal)?
        .to_owned();
    let id = body.get("id").and_then(Value::as_str).map(str::to_owned);
    let encoded = body
        .get("base64")
        .or_else(|| body.get("data"))
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid file data"))?
        .to_owned();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let asset = tokio::task::spawn_blocking(move || {
        payload_store::upload(&store, &agent, &name, id.as_deref(), &encoded)
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(map_payload_store_error)?;
    super::generated_json::<crate::generated::AssetRecord>(asset).map(Json)
}

pub(crate) async fn file_info(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let agent_id = string_query(&query, "agent").map(str::to_owned);
    let path = string_query(&query, "path").map(str::to_owned);
    let asset_id = string_query(&query, "asset")
        .filter(|asset| !asset.is_empty())
        .map(str::to_owned);
    let result = tokio::task::spawn_blocking(move || {
        read_info(
            &store,
            agent_id.as_deref(),
            path.as_deref(),
            asset_id.as_deref(),
        )
    })
    .await
    .map_err(|_| ApiError::internal())??;
    super::generated_json::<crate::generated::FileInfo>(json!({
        "path": result.path,
        "name": result.name,
        "mime": result.mime,
        "size": result.size,
    }))
    .map(Json)
}

pub(crate) async fn file_content(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let agent_id = string_query(&query, "agent").map(str::to_owned);
    let path = string_query(&query, "path").map(str::to_owned);
    let asset_id = string_query(&query, "asset")
        .filter(|asset| !asset.is_empty())
        .map(str::to_owned);
    let result = tokio::task::spawn_blocking(move || {
        read_content(
            &store,
            agent_id.as_deref(),
            path.as_deref(),
            asset_id.as_deref(),
        )
    })
    .await
    .map_err(|_| ApiError::internal())??;
    super::generated_json::<crate::generated::FileContent>(json!({
        "name": result.name,
        "mime": result.mime,
        "base64": base64_encode(&result.bytes),
    }))
    .map(Json)
}

fn read_info(
    store: &RuntimeStore,
    agent_id: Option<&str>,
    path: Option<&str>,
    asset_id: Option<&str>,
) -> Result<workspace_files::FileInfo, ApiError> {
    if let Some(asset_id) = asset_id {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let asset = WorkspaceFilesRepository::asset(&connection, asset_id)
            .map_err(|_| ApiError::internal())?
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown attachment"))?;
        let (asset_value, owner_id) = validate_asset(&asset.id, &asset.record)?;
        drop(connection);
        validate_agent(store, &owner_id)?;
        let path = asset_value
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_stored("assets", &asset.id))?;
        let mut info = workspace_files::file_info(path, None).map_err(file_error)?;
        info.mime = asset_value
            .get("mime")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_stored("assets", &asset.id))?
            .to_owned();
        return Ok(info);
    }
    let context = workspace_context(store, agent_id)?;
    ensure_local_workspace(&context, "GET", "/api/file-info")?;
    let path = path.ok_or_else(|| file_error(WorkspaceFilesError::MissingPath))?;
    workspace_files::file_info(path, Some(&context.cwd)).map_err(file_error)
}

fn read_content(
    store: &RuntimeStore,
    agent_id: Option<&str>,
    path: Option<&str>,
    asset_id: Option<&str>,
) -> Result<workspace_files::FileContent, ApiError> {
    if let Some(asset_id) = asset_id {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let asset = WorkspaceFilesRepository::asset(&connection, asset_id)
            .map_err(|_| ApiError::internal())?
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown attachment"))?;
        let (asset_value, owner_id) = validate_asset(&asset.id, &asset.record)?;
        drop(connection);
        validate_agent(store, &owner_id)?;
        let path = asset_value
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_stored("assets", &asset.id))?;
        let mut content = workspace_files::file_content(path, None).map_err(file_error)?;
        content.mime = asset_value
            .get("mime")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_stored("assets", &asset.id))?
            .to_owned();
        return Ok(content);
    }
    let context = workspace_context(store, agent_id)?;
    ensure_local_workspace(&context, "GET", "/api/file")?;
    let path = path.ok_or_else(|| file_error(WorkspaceFilesError::MissingPath))?;
    workspace_files::file_content(path, Some(&context.cwd)).map_err(file_error)
}

fn workspace_context(
    store: &RuntimeStore,
    agent_id: Option<&str>,
) -> Result<WorkspaceContext, ApiError> {
    let agent_id = agent_id
        .filter(|agent| !agent.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Supply an agent ID"))?;
    validate_agent(store, agent_id)
}

fn validate_agent(store: &RuntimeStore, agent_id: &str) -> Result<WorkspaceContext, ApiError> {
    let connection = store.read().map_err(|_| ApiError::internal())?;
    let agent = AgentRepository::get(&connection, &AgentId::from(agent_id))
        .map_err(|_| ApiError::internal())?
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent"))?;
    let record = serde_json::to_value(agent).map_err(|_| ApiError::internal())?;
    let dto = serde_json::from_value::<crate::generated::AgentRecordResponse>(record)
        .map_err(|_| invalid_stored("agents", agent_id))?;
    let record = serde_json::to_value(dto).map_err(|_| ApiError::internal())?;
    if record.get("deletedAt").is_some_and(json_truthy) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "This agent was deleted",
        ));
    }
    WorkspaceContext::from_agent(&record, agent_id).map_err(|error| match error {
        WorkspaceFilesError::InvalidAgent(id) => invalid_stored("agents", &id),
        other => file_error(other),
    })
}

fn validate_asset(id: &str, record: &str) -> Result<(Value, String), ApiError> {
    let value = serde_json::from_str::<Value>(record).map_err(|_| invalid_stored("assets", id))?;
    let dto = serde_json::from_value::<crate::generated::AssetRecord>(value)
        .map_err(|_| invalid_stored("assets", id))?;
    let value = serde_json::to_value(dto).map_err(|_| ApiError::internal())?;
    if value.get("id").and_then(Value::as_str) != Some(id) {
        return Err(invalid_stored("assets", id));
    }
    let owner = value
        .get("agent")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_stored("assets", id))?
        .to_owned();
    Ok((value, owner))
}

fn ensure_local_workspace(
    context: &WorkspaceContext,
    method: &str,
    path: &str,
) -> Result<(), ApiError> {
    if context.execution_backed {
        Err(
            dependency_unavailable(method, path, &EXECUTION_DEPENDENCIES)
                .unwrap_or_else(|_| ApiError::internal()),
        )
    } else {
        Ok(())
    }
}

fn string_query<'a>(query: &'a QueryParams, name: &str) -> Option<&'a str> {
    query.typed_value(name).and_then(Value::as_str)
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
    }
}

fn invalid_stored(object: &str, id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object {object}/{id}"),
    )
}

fn file_error(error: WorkspaceFilesError) -> ApiError {
    match error {
        WorkspaceFilesError::InvalidAgent(id) => invalid_stored("agents", &id),
        WorkspaceFilesError::MissingFile => {
            ApiError::new(StatusCode::BAD_REQUEST, "This file does not exist")
        }
        WorkspaceFilesError::PreviewLimit => ApiError::new(
            StatusCode::BAD_REQUEST,
            "This file exceeds the 20 MiB preview limit",
        ),
        WorkspaceFilesError::MissingPath => ApiError::new(StatusCode::BAD_REQUEST, "Supply a path"),
        WorkspaceFilesError::InvalidPath => ApiError::new(StatusCode::BAD_REQUEST, "Invalid path"),
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn map_payload_store_error(error: PayloadStoreError) -> ApiError {
    match error {
        PayloadStoreError::InvalidData => {
            ApiError::new(StatusCode::BAD_REQUEST, "Invalid file data")
        }
        PayloadStoreError::InvalidFilename => {
            ApiError::new(StatusCode::BAD_REQUEST, "Invalid filename")
        }
        PayloadStoreError::ContentLimit => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Files must contain 1 byte to 20 MiB",
        ),
        PayloadStoreError::UploadConflict => ApiError::new(
            StatusCode::BAD_REQUEST,
            "This upload id has different content",
        ),
        PayloadStoreError::UnknownAgent => {
            ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent")
        }
        PayloadStoreError::DeletedAgent => {
            ApiError::new(StatusCode::BAD_REQUEST, "This agent was deleted")
        }
        PayloadStoreError::InvalidStoredAsset => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Invalid stored object assets",
        ),
        PayloadStoreError::Io(_)
        | PayloadStoreError::Store(_)
        | PayloadStoreError::Json(_)
        | PayloadStoreError::Clock
        | PayloadStoreError::Entropy => ApiError::internal(),
    }
}
