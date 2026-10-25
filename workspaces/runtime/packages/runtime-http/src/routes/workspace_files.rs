//! File metadata and bounded content reads for local workspaces.

use super::extractors::ApiJson;
use super::extractors::QueryParams;
use super::manifest::{OperationStatus, RouteGroup, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use base64::Engine;
use serde_json::{Value, json};
use studio_runtime_schema::shared_values::USER_ATTACHMENT_LIMIT_LABEL;
use studio_runtime_services::payload_store::{self, PayloadStoreError};
use studio_runtime_services::workspace_files::{self, WorkspaceContext, WorkspaceFilesError};
use studio_runtime_store::{
    AgentId, AgentRepository, RuntimeStore, StoreError, WorkspaceFilesRepository,
};

use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post("/api/assets", OperationStatus::Implemented, upload_asset),
        get("/api/file-info", OperationStatus::Implemented, file_info),
        get("/api/file", OperationStatus::Implemented, file_content),
    ])
}

async fn upload_asset(
    State(state): State<HttpState>,
    ApiJson(mut body): ApiJson<Value>,
) -> Result<Json<crate::generated::AssetRecord>, ApiError> {
    let agent = take_string(&mut body, "agent").ok_or_else(ApiError::internal)?;
    let name = take_string(&mut body, "name").ok_or_else(ApiError::internal)?;
    let id = take_string(&mut body, "id");
    let encoded = take_string(&mut body, "base64")
        .or_else(|| take_string(&mut body, "data"))
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid file data"))?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let asset = payload_store::upload(&store, &agent, &name, id.as_deref(), &encoded)
        .await
        .map_err(map_payload_store_error)?;
    super::generated_json::<crate::generated::AssetRecord>(asset).map(Json)
}

pub(crate) async fn file_info(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::FileInfo>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let agent_id = string_query(&query, "agent").map(str::to_owned);
    let path = string_query(&query, "path").map(str::to_owned);
    let asset_id = string_query(&query, "asset")
        .filter(|asset| !asset.is_empty())
        .map(str::to_owned);
    let target = file_target(&store, agent_id, path, asset_id).await?;
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/workspace_files",
        move || read_info(target),
    )
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
) -> Result<Json<crate::generated::FileContent>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let agent_id = string_query(&query, "agent").map(str::to_owned);
    let path = string_query(&query, "path").map(str::to_owned);
    let asset_id = string_query(&query, "asset")
        .filter(|asset| !asset.is_empty())
        .map(str::to_owned);
    let target = file_target(&store, agent_id, path, asset_id).await?;
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/workspace_files",
        move || read_content(target),
    )
    .await
    .map_err(|_| ApiError::internal())??;
    super::generated_json::<crate::generated::FileContent>(json!({
        "name": result.name,
        "mime": result.mime,
        "base64": base64_encode(&result.bytes),
    }))
    .map(Json)
}

/// A file to read once the store lookups are done.
enum FileTarget {
    Asset {
        path: String,
        mime: String,
    },
    Workspace {
        context: WorkspaceContext,
        path: String,
    },
}

/// Resolve an attachment or a workspace path with one store read.
async fn file_target(
    store: &RuntimeStore,
    agent_id: Option<String>,
    path: Option<String>,
    asset_id: Option<String>,
) -> Result<FileTarget, ApiError> {
    if let Some(asset_id) = asset_id {
        return store
            .read_async("runtime-http/routes/workspace_files", move |connection| {
                let asset = WorkspaceFilesRepository::asset(connection, &asset_id)
                    .map_err(map_store_error)?
                    .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown attachment"))?;
                let asset_value = asset.value();
                let field = |name: &str| {
                    asset_value
                        .get(name)
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .ok_or_else(|| invalid_stored("assets", asset.id().as_ref()))
                };
                let owner_id = field("agent")?;
                validate_agent(connection, &owner_id)?;
                Ok(FileTarget::Asset {
                    path: field("path")?,
                    mime: field("mime")?,
                })
            })
            .await;
    }
    let agent_id = agent_id
        .filter(|agent| !agent.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Supply an agent ID"))?;
    let context = store
        .read_async("runtime-http/routes/workspace_files", move |connection| {
            validate_agent(connection, &agent_id)
        })
        .await?;
    let path = path.ok_or_else(|| file_error(WorkspaceFilesError::MissingPath))?;
    Ok(FileTarget::Workspace { context, path })
}

fn read_info(target: FileTarget) -> Result<workspace_files::FileInfo, ApiError> {
    match target {
        FileTarget::Asset { path, mime } => {
            let mut info = workspace_files::file_info(&path, None).map_err(file_error)?;
            info.mime = mime;
            Ok(info)
        }
        FileTarget::Workspace { context, path } => {
            workspace_files::file_info(&path, Some(&context.cwd)).map_err(file_error)
        }
    }
}

fn read_content(target: FileTarget) -> Result<workspace_files::FileContent, ApiError> {
    match target {
        FileTarget::Asset { path, mime } => {
            let mut content = workspace_files::file_content(&path, None).map_err(file_error)?;
            content.mime = mime;
            Ok(content)
        }
        FileTarget::Workspace { context, path } => {
            workspace_files::file_content(&path, Some(&context.cwd)).map_err(file_error)
        }
    }
}

fn validate_agent(
    connection: &studio_runtime_store::Connection,
    agent_id: &str,
) -> Result<WorkspaceContext, ApiError> {
    let agent = AgentRepository::get(connection, &AgentId::from(agent_id))
        .map_err(map_store_error)?
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent"))?;
    let record = agent.value();
    if record
        .get("deletedAt")
        .is_some_and(|value| studio_runtime_store::truthy(Some(value)))
    {
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

fn string_query<'a>(query: &'a QueryParams, name: &str) -> Option<&'a str> {
    query.typed_value(name).and_then(Value::as_str)
}

fn invalid_stored(object: &str, id: &str) -> ApiError {
    ApiError::invalid_stored(object, id)
}

fn map_store_error(error: StoreError) -> ApiError {
    match error {
        StoreError::InvalidStoredObject { table, id } => invalid_stored(table, &id),
        _ => ApiError::internal(),
    }
}

fn file_error(error: WorkspaceFilesError) -> ApiError {
    match error {
        WorkspaceFilesError::InvalidAgent(id) => invalid_stored("agents", &id),
        WorkspaceFilesError::MissingFile => {
            ApiError::new(StatusCode::BAD_REQUEST, "This file does not exist")
        }
        error @ WorkspaceFilesError::PreviewLimit { .. } => {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
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
            format!("Files must contain 1 byte to {USER_ATTACHMENT_LIMIT_LABEL}"),
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
        PayloadStoreError::InvalidStoredAsset { id } => {
            ApiError::invalid_stored("runtime_assets", &id)
        }
        PayloadStoreError::Store(studio_runtime_store::StoreError::InvalidStoredObject {
            table: "runtime_assets",
            id,
        }) => ApiError::invalid_stored("runtime_assets", &id),
        PayloadStoreError::Io(_)
        | PayloadStoreError::Store(_)
        | PayloadStoreError::Json(_)
        | PayloadStoreError::Clock
        | PayloadStoreError::Entropy => ApiError::internal(),
    }
}

fn take_string(body: &mut Value, key: &str) -> Option<String> {
    match body.as_object_mut()?.remove(key)? {
        Value::String(value) => Some(value),
        _ => None,
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "workspace_files",
        routes: Some(routes),
        dependencies: &[],
    }
}
