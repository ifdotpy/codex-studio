//! Local cost snapshots and persisted usage controls.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};
use studio_runtime_services::usage::{self, UsageError};
use studio_runtime_store::{UsageRepositoryError, cancel_capacity_retry, disable_usage_resume};

use crate::error::ApiError;
use crate::router::HttpState;

const RESET_REDEMPTION: [Dependency; 1] =
    [Dependency::new("account-driver/reset-credit-redemption")];
const COSTS_DEPENDENCIES: [Dependency; 1] = [Dependency::new("costs/local-scanner")];
const CAPACITY_DEPENDENCIES: [Dependency; 3] = [
    Dependency::new("event-driver/queue-mutations"),
    Dependency::new("store-bootstrap/capacity-retry-schema"),
    Dependency::new("turn-driver/capacity-retry"),
];
const USAGE_RESUME_DEPENDENCIES: [Dependency; 2] = [
    Dependency::new("store-bootstrap/usage-resume-schema"),
    Dependency::new("turn-driver/usage-resume"),
];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/costs",
            OperationStatus::partial(&COSTS_DEPENDENCIES),
            costs,
        ),
        post(
            "/api/limits/reset",
            OperationStatus::partial(&RESET_REDEMPTION),
            reset_limits,
        ),
        post(
            "/api/capacity-retry",
            OperationStatus::partial(&CAPACITY_DEPENDENCIES),
            capacity_retry,
        ),
        post(
            "/api/usage-resume",
            OperationStatus::partial(&USAGE_RESUME_DEPENDENCIES),
            usage_resume,
        ),
    ])
}

async fn costs(
    State(state): State<HttpState>,
    params: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let account_key = params
        .typed_value("account_key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("default")
        .to_owned();
    let account_policy = state
        .account_policy
        .clone()
        .ok_or_else(ApiError::internal)?;
    let account = account_policy
        .accounts()
        .accounts
        .into_iter()
        .find(|account| account.get("id").and_then(Value::as_str) == Some(&account_key))
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown Codex account"))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs_f64();
    let state_dir = std::path::PathBuf::from(state.state_dir);
    let result = tokio::task::spawn_blocking(move || {
        usage::account_cost_snapshot(&state_dir, &account_key, &account, now)
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(cost_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::AccountCostResponse,
    >(result)?))
}

async fn reset_limits(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "POST",
        "/api/limits/reset",
        &RESET_REDEMPTION,
    ))
}

async fn capacity_retry(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::CapacityRetryRequest>,
) -> Result<Json<Value>, ApiError> {
    if body.action == crate::generated::RetryAction::Retry {
        return Err(dependency_error(
            "POST",
            "/api/capacity-retry",
            &CAPACITY_DEPENDENCIES,
        ));
    }
    let agent_id = body.id;
    let retry_id = body.retry_id;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let now = current_time()?;
    let result = tokio::task::spawn_blocking(move || {
        store.transaction_with(|transaction| -> Result<Value, UsageRepositoryError> {
            let value = cancel_capacity_retry(transaction, &agent_id, &retry_id, now)?;
            let response = serde_json::from_value::<crate::generated::RetryResponse>(value)
                .map_err(|_| UsageRepositoryError::InvalidStoredObject {
                    table: "runtime_capacity_retries",
                    id: retry_id.clone(),
                })?;
            serde_json::to_value(response).map_err(|_| UsageRepositoryError::InvalidStoredObject {
                table: "runtime_capacity_retries",
                id: retry_id.clone(),
            })
        })
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(|error| usage_error(error, "/api/capacity-retry"))?;
    Ok(Json(result))
}

async fn usage_resume(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::UsageResumeRequest>,
) -> Result<Json<Option<Value>>, ApiError> {
    if body.enabled {
        return Err(dependency_error(
            "POST",
            "/api/usage-resume",
            &USAGE_RESUME_DEPENDENCIES,
        ));
    }
    let agent_id = body.id;
    let resume_id = body.resume_id;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let now = current_time()?;
    let result = tokio::task::spawn_blocking(move || {
        store.transaction_with(
            |transaction| -> Result<Option<Value>, UsageRepositoryError> {
                let value = disable_usage_resume(transaction, &agent_id, &resume_id, now)?;
                if value.is_null() {
                    return Ok(None);
                }
                let response =
                    serde_json::from_value::<crate::generated::UsageResumeResponse>(value)
                        .map_err(|_| UsageRepositoryError::InvalidStoredObject {
                            table: "runtime_usage_resumes",
                            id: resume_id.clone(),
                        })?;
                serde_json::to_value(response).map(Some).map_err(|_| {
                    UsageRepositoryError::InvalidStoredObject {
                        table: "runtime_usage_resumes",
                        id: resume_id.clone(),
                    }
                })
            },
        )
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(|error| usage_error(error, "/api/usage-resume"))?;
    Ok(Json(result))
}

fn current_time() -> Result<f64, ApiError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs_f64())
}

fn cost_error(error: UsageError) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
}

fn usage_error(error: UsageRepositoryError, path: &'static str) -> ApiError {
    match error {
        UsageRepositoryError::MissingFamily => {
            let dependencies: &[Dependency] = if path == "/api/capacity-retry" {
                &CAPACITY_DEPENDENCIES
            } else {
                &USAGE_RESUME_DEPENDENCIES
            };
            dependency_error("POST", path, dependencies)
        }
        UsageRepositoryError::EventDriverRequired => {
            dependency_error("POST", path, &CAPACITY_DEPENDENCIES)
        }
        UsageRepositoryError::UnknownAgent => ApiError::new(StatusCode::NOT_FOUND, "Unknown agent"),
        UsageRepositoryError::UnknownContinuation => {
            let message = if path == "/api/capacity-retry" {
                "Unknown capacity retry."
            } else {
                "Unknown automatic resume"
            };
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        UsageRepositoryError::InvalidStoredObject { table, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {table}/{id}"),
        ),
        UsageRepositoryError::Sqlite(_) | UsageRepositoryError::Store(_) => ApiError::internal(),
    }
}

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}

#[cfg(test)]
mod tests {
    use super::current_time;

    #[test]
    fn runtime_clock_is_a_finite_epoch_timestamp() {
        let now = current_time().expect("system epoch clock");
        assert!(now.is_finite() && now > 0.0);
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "usage",
        routes: Some(routes),
        dependencies: &[
            "account-driver/reset-credit-redemption",
            "costs/local-scanner",
            "event-driver/queue-mutations",
            "store-bootstrap/capacity-retry-schema",
            "store-bootstrap/usage-resume-schema",
            "turn-driver/capacity-retry",
            "turn-driver/usage-resume",
        ],
    }
}
