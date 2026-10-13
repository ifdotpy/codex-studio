//! Read-only account and persisted usage snapshots.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::json;

use crate::error::ApiError;
use crate::router::HttpState;
use crate::routes::extractors::QueryParams;
use crate::routes::manifest::{
    Dependency, OperationStatus, RouteGroup, dependency_unavailable, get as register_get,
};

static LIMITS_DEPENDENCIES: [Dependency; 1] =
    [Dependency::new("provider-session/account-rate-limits")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        register_get("/api/accounts", OperationStatus::Implemented, accounts),
        register_get(
            "/api/limits",
            OperationStatus::partial(&LIMITS_DEPENDENCIES),
            limits,
        ),
    ])
}

async fn accounts(
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::AccountsResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let account_policy = state
        .account_policy
        .clone()
        .ok_or_else(ApiError::internal)?;
    let snapshot = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ())?;
        let waiting = studio_runtime_store::account_state::authentication_recovery_account_keys(
            &connection,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| ())?
                .as_secs_f64(),
        )
        .map_err(|_| ())?;
        let mut snapshot = account_policy.accounts();
        studio_runtime_services::account_policy::apply_authentication_recovery(
            &mut snapshot,
            &waiting,
        );
        Ok::<_, ()>(snapshot)
    })
    .await
    .ok()
    .and_then(Result::ok);
    let snapshot = snapshot.ok_or_else(ApiError::internal)?;
    let response = serde_json::from_value(json!({
        "accounts": snapshot.accounts,
        "archivedAccounts": snapshot.archived_accounts,
        "defaultAccountKey": snapshot.default_account_key,
        "logins": snapshot.logins,
        "supportsDisconnect": true,
        "supportsDelete": true,
    }))
    .map_err(|_| ApiError::internal())?;
    Ok(Json(response))
}

async fn limits(
    State(state): State<HttpState>,
    params: QueryParams,
) -> Result<Json<crate::generated::UsageLimitsResponse>, ApiError> {
    let account_key = params
        .typed_value("account_key")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("default");
    let cached = params
        .typed_value("cached")
        .and_then(serde_json::Value::as_str)
        == Some("1");
    let account_policy = state
        .account_policy
        .as_ref()
        .ok_or_else(ApiError::internal)?;
    let snapshot = account_policy.limits(account_key);
    if cached
        && (snapshot.get("data").is_some_and(|data| !data.is_null())
            || snapshot
                .get("error")
                .is_some_and(|error| !error.is_null() && error != ""))
    {
        let response = serde_json::from_value::<crate::generated::UsageLimitsResponse>(snapshot)
            .map_err(|_| ApiError::internal())?;
        return Ok(Json(response));
    }
    // Python returns an existing snapshot while scheduling a background
    // refresh before checking whether the account is currently registered.
    if snapshot.get("data").is_some_and(|data| !data.is_null()) {
        return Err(dependency_error()?);
    }
    if !account_policy.has_account(account_key) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Unknown Codex account",
        ));
    }
    Err(dependency_error()?)
}

fn dependency_error() -> Result<ApiError, ApiError> {
    dependency_unavailable("GET", "/api/limits", &LIMITS_DEPENDENCIES)
        .map_err(|_| ApiError::internal())
}
