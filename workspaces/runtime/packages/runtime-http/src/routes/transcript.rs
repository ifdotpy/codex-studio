//! HTTP adapters for persisted transcript item reads.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use studio_runtime_services::transcript::{self, TranscriptError};

use super::extractors::QueryParams;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get};
use crate::error::ApiError;
use crate::router::HttpState;

const PAGE_DEPENDENCIES: [Dependency; 3] = [
    Dependency::new("transcript/event-driver"),
    Dependency::new("transcript/payload-store"),
    Dependency::new("transcript/workspace-state"),
];
const CANVAS_TRANSCRIPT_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("transcript/event-driver")];
const SEARCH_DEPENDENCIES: [Dependency; 1] = [Dependency::new("transcript/search-index")];
const SEARCH_ITEM_DEPENDENCIES: [Dependency; 1] = [Dependency::new("transcript/workspace-state")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/transcript/page",
            OperationStatus::partial(&PAGE_DEPENDENCIES),
            transcript_page,
        ),
        get(
            "/api/transcript/item",
            OperationStatus::Implemented,
            transcript_item,
        ),
        get(
            "/api/transcript/search",
            OperationStatus::Implemented,
            transcript_search,
        ),
        get(
            "/api/search/item",
            OperationStatus::partial(&SEARCH_ITEM_DEPENDENCIES),
            search_item,
        ),
        get(
            "/api/search",
            OperationStatus::partial(&SEARCH_DEPENDENCIES),
            search,
        ),
        get(
            "/api/transcript",
            OperationStatus::partial(&CANVAS_TRANSCRIPT_DEPENDENCIES),
            canvas_transcript,
        ),
    ])
}

async fn transcript_page() -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "GET",
        "/api/transcript/page",
        &PAGE_DEPENDENCIES,
    ))
}

async fn transcript_item(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let agent = query
        .typed_value("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let identity = query
        .typed_value("message_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || transcript::item(&store, &agent, &identity))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(map_transcript_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::TranscriptItemResponse,
    >(value)?))
}

async fn transcript_search(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let agent = query
        .typed_value("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let search = query
        .typed_value("q")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let limit = query
        .typed_value("limit")
        .and_then(Value::as_i64)
        .unwrap_or(100);
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value =
        tokio::task::spawn_blocking(move || transcript::search(&store, &agent, &search, limit))
            .await
            .map_err(|_| ApiError::internal())?
            .map_err(map_transcript_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::TranscriptSearchResponse,
    >(value)?))
}

async fn search_item() -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "GET",
        "/api/search/item",
        &SEARCH_ITEM_DEPENDENCIES,
    ))
}

async fn search() -> Result<Json<Value>, ApiError> {
    Err(dependency_error("GET", "/api/search", &SEARCH_DEPENDENCIES))
}

async fn canvas_transcript() -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "GET",
        "/api/transcript",
        &CANVAS_TRANSCRIPT_DEPENDENCIES,
    ))
}

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}

fn map_transcript_error(error: TranscriptError) -> ApiError {
    match error {
        TranscriptError::UnknownMessage => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Unknown message in this conversation",
        ),
        TranscriptError::DeletedConversation => {
            ApiError::new(StatusCode::BAD_REQUEST, "This conversation was deleted")
        }
        TranscriptError::MissingContent => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Full transcript content is unavailable",
        ),
        TranscriptError::InvalidStoredObject(object) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}"),
        ),
        TranscriptError::Store => ApiError::internal(),
    }
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "transcript",
        routes: Some(routes),
        dependencies: &["transcript/event-driver", "transcript/payload-store", "transcript/search-index", "transcript/workspace-state"],
    }
}
