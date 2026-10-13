//! HTTP adapters for analytics and signature-checked session-cost snapshots.

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::time::Instant;
use studio_runtime_services::analytics::{self, AnalyticsOptions};

use super::extractors::QueryParams;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get};
use crate::error::ApiError;
use crate::router::HttpState;

const SESSION_COST_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("usage-policy/session-cost-refresh")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/analytics",
            OperationStatus::Implemented,
            analytics_read,
        ),
        get(
            "/api/session-cost",
            OperationStatus::partial(&SESSION_COST_DEPENDENCIES),
            session_cost,
        ),
    ])
}

async fn analytics_read(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Response, ApiError> {
    let options = AnalyticsOptions {
        agent: string(&query, "agent"),
        scope: string(&query, "scope"),
        from: number(&query, "from"),
        to: number(&query, "to"),
        limit: integer(&query, "limit"),
        offset: integer(&query, "offset"),
        tool: string(&query, "tool"),
        export: string(&query, "export").as_deref() == Some("1"),
        view: string(&query, "view"),
        detail: string(&query, "detail"),
        item: string(&query, "item"),
        turn: string(&query, "turn"),
        thread: string(&query, "thread"),
        turns: string(&query, "turns"),
        timing: string(&query, "timing"),
    };
    let export = options.export;
    let timing = options.timing.as_deref() == Some("1") && !export;
    let started = Instant::now();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || analytics::read(&store, &options))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(map_analytics_error)?;
    if export {
        let body = serde_json::to_vec(&result).map_err(|_| ApiError::internal())?;
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = StatusCode::OK;
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        response.headers_mut().insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"codex-studio-analytics.json\""),
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response.headers_mut().insert(
            "x-content-type-options",
            HeaderValue::from_static("nosniff"),
        );
        return Ok(response);
    }
    let mut response =
        Json(super::generated_json::<crate::generated::AnalyticsResponse>(result)?).into_response();
    if timing {
        let value = format!(
            "analytics-total;dur={:.3}",
            started.elapsed().as_secs_f64() * 1000.0
        );
        if let Ok(value) = HeaderValue::from_str(&value) {
            response.headers_mut().insert("server-timing", value);
        }
    }
    Ok(response)
}

async fn session_cost(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let agent = query.nonempty_str("agent").unwrap_or_default().to_owned();
    if agent.is_empty() || !valid_agent_id(&agent) {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Select a chat"));
    }
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value =
        tokio::task::spawn_blocking(move || analytics::persisted_session_cost(&store, &agent))
            .await
            .map_err(|_| ApiError::internal())?
            .map_err(map_session_cost_error)?;
    let Some(value) = value else {
        return Err(dependency_error(
            "GET",
            "/api/session-cost",
            &SESSION_COST_DEPENDENCIES,
        ));
    };
    Ok(Json(super::generated_json::<
        crate::generated::SessionCostResponse,
    >(value)?))
}

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}

fn map_analytics_error(error: analytics::AnalyticsError) -> ApiError {
    match error {
        analytics::AnalyticsError::InvalidRequest(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        analytics::AnalyticsError::UnknownAgent => {
            ApiError::new(StatusCode::BAD_REQUEST, "Select an agent for analytics")
        }
        analytics::AnalyticsError::InvalidStoredObject(object) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}"),
        ),
        analytics::AnalyticsError::Read(_) | analytics::AnalyticsError::Sqlite(_) => {
            ApiError::internal()
        }
    }
}

fn map_session_cost_error(error: analytics::SessionCostError) -> ApiError {
    match error {
        analytics::SessionCostError::UnknownAgent => {
            ApiError::new(StatusCode::BAD_REQUEST, "Unknown chat")
        }
        analytics::SessionCostError::InvalidStored(object) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}"),
        ),
        analytics::SessionCostError::Json(_) => ApiError::internal(),
        analytics::SessionCostError::Read(_) | analytics::SessionCostError::Sqlite(_) => {
            ApiError::internal()
        }
    }
}

fn valid_agent_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
}

fn string(query: &QueryParams, name: &str) -> Option<String> {
    query
        .typed_value(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
}
fn number(query: &QueryParams, name: &str) -> Option<f64> {
    query.typed_value(name).and_then(Value::as_f64)
}
fn integer(query: &QueryParams, name: &str) -> Option<i64> {
    query.typed_value(name).and_then(Value::as_i64)
}
