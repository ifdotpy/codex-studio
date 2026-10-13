//! HTTP adapters for persisted transcript item reads.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use studio_runtime_services::transcript::{self, TranscriptError};
use studio_runtime_services::transcript_runtime::{self, PageRequest};

use super::extractors::QueryParams;
use super::manifest::{Dependency, OperationStatus, RouteGroup, get};
use crate::error::ApiError;
use crate::router::HttpState;

pub(super) const PAGE_DEPENDENCIES: &[Dependency] = &[
    Dependency::new("transcript/payload-store"),
    Dependency::new("transcript/receipt-page"),
    Dependency::new("transcript/reasoning-page"),
];
const CANVAS_DEPENDENCIES: &[Dependency] = &[
    Dependency::new("transcript/payload-store"),
    Dependency::new("transcript/receipt-page"),
    Dependency::new("transcript/reasoning-page"),
    Dependency::new("transcript/workspace-state"),
];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/transcript/page",
            OperationStatus::partial(PAGE_DEPENDENCIES),
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
            OperationStatus::Implemented,
            search_item,
        ),
        get("/api/search", OperationStatus::Implemented, search),
        get(
            "/api/transcript",
            OperationStatus::partial(CANVAS_DEPENDENCIES),
            canvas_transcript,
        ),
    ])
}

async fn transcript_page(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let request = PageRequest {
        before: query
            .typed_value("before")
            .and_then(Value::as_str)
            .map(str::to_owned),
        around: query
            .typed_value("around")
            .and_then(Value::as_str)
            .map(str::to_owned),
        after: query
            .typed_value("after")
            .and_then(Value::as_str)
            .map(str::to_owned),
        limit: query.typed_value("limit").and_then(Value::as_i64),
    };
    read_page(state, query, request, false).await
}

async fn read_page(
    state: HttpState,
    query: QueryParams,
    request: PageRequest,
    native_fallback: bool,
) -> Result<Json<Value>, ApiError> {
    let agent = query
        .typed_value("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || {
        match transcript_runtime::page(&store, &agent, &request) {
            Err(TranscriptError::UnknownMessage) if native_fallback => Err(
                TranscriptError::RequiresDependency("transcript/workspace-state"),
            ),
            result => result,
        }
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(map_transcript_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::TranscriptPageResponse,
    >(value)?))
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

async fn search_item(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let identity = query
        .typed_value("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || transcript::search_item(&store, &identity))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(map_transcript_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::SearchItemResponse,
    >(value)?))
}

async fn search(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let search = query
        .typed_value("q")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let limit = query
        .typed_value("limit")
        .and_then(Value::as_i64)
        .unwrap_or(50);
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value =
        tokio::task::spawn_blocking(move || transcript::search_work(&store, &search, None, limit))
            .await
            .map_err(|_| ApiError::internal())?
            .map_err(map_transcript_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::SearchResponse,
    >(value)?))
}

async fn canvas_transcript(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    read_page(state, query, PageRequest::default(), true).await
}

pub(crate) fn map_transcript_error(error: TranscriptError) -> ApiError {
    match error {
        TranscriptError::DeletedAssetOwner => {
            ApiError::new(StatusCode::BAD_REQUEST, "This agent was deleted")
        }
        TranscriptError::UnknownAssetOwner => {
            ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent")
        }
        TranscriptError::SnapshotChanged => ApiError::new(
            StatusCode::CONFLICT,
            "Search context changed; retry the read",
        ),
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
        TranscriptError::SearchResultUnavailable => {
            ApiError::new(StatusCode::BAD_REQUEST, "This search result is unavailable")
        }
        TranscriptError::ChatUnavailable => {
            ApiError::new(StatusCode::BAD_REQUEST, "This chat is unavailable")
        }
        TranscriptError::HistoryCursor => {
            ApiError::new(StatusCode::BAD_REQUEST, "Use one history cursor")
        }
        TranscriptError::InvalidStoredState { table, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {table}/{id}"),
        ),
        TranscriptError::InvalidSearch => {
            ApiError::new(StatusCode::BAD_REQUEST, "Supply a valid search query")
        }
        TranscriptError::RequiresDependency(dependency) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {dependency}"),
        ),
        TranscriptError::InvalidStoredObject { table, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {table}/{id}"),
        ),
        TranscriptError::Store => ApiError::internal(),
    }
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "transcript",
        routes: Some(routes),
        dependencies: &["transcript/payload-store", "transcript/receipt-page", "transcript/reasoning-page", "transcript/workspace-state"],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transcript_partial_manifest_accepts_every_remaining_refusal() {
        use super::super::manifest::dependency_unavailable;
        for path in ["/api/transcript/page", "/api/sync/pull"] {
            assert!(dependency_unavailable("GET", path, PAGE_DEPENDENCIES).is_ok());
        }
        assert!(dependency_unavailable("GET", "/api/transcript", CANVAS_DEPENDENCIES).is_ok());
    }
}
