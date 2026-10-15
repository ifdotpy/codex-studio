//! Read-only account and persisted usage snapshots.

use axum::Json;
use axum::extract::State;

use crate::error::ApiError;
use crate::router::HttpState;
use crate::routes::extractors::QueryParams;
use crate::routes::manifest::{OperationStatus, RouteGroup, get as register_get};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        register_get("/api/accounts", OperationStatus::Implemented, accounts),
        register_get("/api/limits", OperationStatus::Implemented, limits),
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
    let account_driver = state.account_driver.clone();
    let snapshot = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/catalog",
        move || {
            let connection = store.read().map_err(|_| ())?;
            let waiting =
                studio_runtime_store::account_state::authentication_recovery_account_keys(
                    &connection,
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_err(|_| ())?
                        .as_secs_f64(),
                )
                .map_err(|_| ())?;
            let mut snapshot = match account_driver {
                Some(driver) => driver.listing_snapshot().map_err(|_| ())?,
                None => account_policy.accounts(),
            };
            studio_runtime_services::account_policy::apply_authentication_recovery(
                &mut snapshot,
                &waiting,
            );
            Ok::<_, ()>(snapshot)
        },
    )
    .await
    .ok()
    .and_then(Result::ok);
    let snapshot = snapshot.ok_or_else(ApiError::internal)?;
    super::accounts_response(snapshot)
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
    if let Some(port) = state.account_session.clone() {
        let account = account_key.to_owned();
        let response = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/catalog",
            move || {
                studio_runtime_services::account_session_runtime::limits(
                    port.as_ref(),
                    &account,
                    cached,
                )
            },
        )
        .await
        .map_err(|_| ApiError::internal())?;
        let response = match response {
            Ok(response) => response,
            Err(
                studio_runtime_services::account_session_runtime::AccountSessionError::Refused(
                    "Unknown Codex account",
                ),
            ) => {
                return Err(super::account_login::session_error(
                    studio_runtime_services::account_session_runtime::AccountSessionError::Refused(
                        "Unknown Codex account",
                    ),
                ));
            }
            Err(error) => limit_error_snapshot(&state, &account_key, &error.to_string())?,
        };
        return serde_json::from_value(response)
            .map(Json)
            .map_err(|_| ApiError::internal());
    }
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
    if snapshot.get("data").is_some_and(|data| !data.is_null()) {
        return serde_json::from_value(snapshot)
            .map(Json)
            .map_err(|_| ApiError::internal());
    }
    let snapshot = limit_error_snapshot(&state, account_key, "Account limits could not be read.")?;
    serde_json::from_value(snapshot)
        .map(Json)
        .map_err(|_| ApiError::internal())
}

fn limit_error_snapshot(
    state: &HttpState,
    account_key: &str,
    message: &str,
) -> Result<serde_json::Value, ApiError> {
    let account_policy = state
        .account_policy
        .as_ref()
        .ok_or_else(ApiError::internal)?;
    let mut snapshot = account_policy.limits(account_key);
    let object = snapshot.as_object_mut().ok_or_else(ApiError::internal)?;
    object.insert(
        "error".to_owned(),
        serde_json::Value::String(message.to_owned()),
    );
    object.insert(
        "checkedAt".to_owned(),
        serde_json::Value::from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| ApiError::internal())?
                .as_secs_f64(),
        ),
    );
    Ok(snapshot)
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "catalog",
        routes: Some(routes),
        dependencies: &[],
    }
}
