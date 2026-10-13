//! HTTP adapters for analytics and signature-checked session-cost snapshots.

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Instant;
use studio_runtime_services::analytics::{self, AnalyticsOptions};
use studio_runtime_services::budget_runtime;
use studio_runtime_services::costs::CostsError;

use super::extractors::QueryParams;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get};
use crate::error::ApiError;
use crate::router::HttpState;

const SESSION_COST_DEPENDENCIES: [Dependency; 3] = [
    Dependency::new("store-bootstrap/analytics-usage-schema"),
    Dependency::new("costs/pricing-refresh"),
    Dependency::new("costs/claude-local-usage"),
];

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

enum ReadBody {
    Json(Value),
    Export(Vec<u8>),
}

async fn analytics_read(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Response, ApiError> {
    let options: AnalyticsOptions = serde_json::from_value(Value::Object(query.into_typed()))
        .map_err(|_| ApiError::internal())?;
    let export = options.export;
    let timing = options.timing.as_deref() == Some("1") && !export;
    let started = Instant::now();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        let value = analytics::read(&store, &options)?;
        if export {
            Ok(ReadBody::Export(serde_json::to_vec(&value)?))
        } else {
            Ok(ReadBody::Json(value))
        }
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(map_analytics_error)?;
    let mut response = match result {
        ReadBody::Json(value) => {
            Json(super::generated_json::<crate::generated::AnalyticsResponse>(value)?)
                .into_response()
        }
        ReadBody::Export(body) => {
            let mut response = Response::new(Body::from(body));
            for (name, value) in [
                (header::CONTENT_TYPE, "application/json; charset=utf-8"),
                (
                    header::CONTENT_DISPOSITION,
                    "attachment; filename=\"codex-studio-analytics.json\"",
                ),
                (header::CACHE_CONTROL, "no-store"),
                (HeaderName::from_static("x-content-type-options"), "nosniff"),
            ] {
                response
                    .headers_mut()
                    .insert(name, HeaderValue::from_static(value));
            }
            response
        }
    };
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
    let account_providers = state
        .account_policy
        .as_ref()
        .map(|policy| {
            policy
                .accounts()
                .accounts
                .into_iter()
                .filter_map(|account| {
                    Some((
                        account.get("id")?.as_str()?.to_owned(),
                        account.get("provider")?.as_str()?.to_owned(),
                    ))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let value = tokio::task::spawn_blocking(move || {
        budget_runtime::session_cost(&store, &agent, &account_providers)
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(map_costs_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::SessionCostResponse,
    >(value)?))
}

fn cost_dependency(dependency: &'static str) -> ApiError {
    debug_assert!(
        SESSION_COST_DEPENDENCIES
            .iter()
            .any(|candidate| candidate.name() == dependency)
    );
    dependency_unavailable("GET", "/api/session-cost", &SESSION_COST_DEPENDENCIES)
        .unwrap_or_else(|_| ApiError::internal())
}

fn map_costs_error(error: CostsError) -> ApiError {
    match error {
        CostsError::Dependency(dependency) => cost_dependency(dependency),
        CostsError::Analytics(error) => map_analytics_error(error),
        CostsError::Usage(studio_runtime_store::CostUsageRepositoryError::MissingFamily) => {
            dependency_unavailable("GET", "/api/session-cost", &SESSION_COST_DEPENDENCIES)
                .unwrap_or_else(|_| ApiError::internal())
        }
        CostsError::Usage(studio_runtime_store::CostUsageRepositoryError::InvalidStoredRow(id)) => {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid stored object analytics_usage/{id}"),
            )
        }
        CostsError::Usage(
            studio_runtime_store::CostUsageRepositoryError::Sqlite(_)
            | studio_runtime_store::CostUsageRepositoryError::RecordLimit,
        )
        | CostsError::Grouping(_)
        | CostsError::Repository(_)
        | CostsError::Sqlite(_)
        | CostsError::Computation => ApiError::internal(),
        CostsError::InvalidPricing => cost_dependency("costs/pricing-refresh"),
    }
}

fn map_analytics_error(error: analytics::AnalyticsError) -> ApiError {
    match error {
        analytics::AnalyticsError::RefreshRequired => {
            cost_dependency("store-bootstrap/analytics-usage-schema")
        }
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
        analytics::AnalyticsError::InvalidStoredRow { table, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {table}/{id}"),
        ),
        analytics::AnalyticsError::Read(_)
        | analytics::AnalyticsError::Sqlite(_)
        | analytics::AnalyticsError::Serialization(_) => ApiError::internal(),
    }
}

fn valid_agent_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "insights",
        routes: Some(routes),
        dependencies: &[
        "store-bootstrap/analytics-usage-schema",
        "costs/claude-local-usage",
        "costs/pricing-refresh",
        ],
    }
}
