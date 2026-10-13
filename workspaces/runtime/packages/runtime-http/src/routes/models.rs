//! Account-scoped provider model catalogs.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::router::HttpState;
use crate::routes::extractors::QueryParams;
use crate::routes::manifest::{OperationStatus, RouteGroup, get};
use studio_runtime_services::account_policy::AccountsSnapshot;
use studio_runtime_services::model_catalog::{CatalogRead, ModelCatalogService};

const PENDING_MESSAGE: &str = "Model catalog is pending; no workers were created. The existing metadata request remains available for recovery";
const WORKER_PENDING_MESSAGE: &str =
    "Model catalog is pending; no worker model catalog is available yet";
const UNAVAILABLE_MESSAGE: &str =
    "Model catalog unavailable; no workers were created: provider request failed";

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![get(
        "/api/models",
        OperationStatus::Implemented,
        models,
    )])
}

async fn models(
    State(state): State<HttpState>,
    params: QueryParams,
) -> Result<(StatusCode, Json<crate::generated::ModelCatalogResponse>), ApiError> {
    let account_key = params
        .nonempty_str("account_key")
        .unwrap_or("default")
        .to_owned();
    let workers = params.typed_value("workers").and_then(Value::as_str) == Some("1");
    let retry = params.typed_value("retry").and_then(Value::as_str) == Some("1");
    let account_policy = state
        .account_policy
        .clone()
        .ok_or_else(ApiError::internal)?;
    let catalog = state.model_catalog.clone();
    let result = tokio::task::spawn_blocking(move || {
        let snapshot = account_policy.accounts();
        if workers {
            worker_catalog(&catalog, &snapshot, &account_key, retry)
        } else {
            account_catalog(&catalog, &snapshot, &account_key, retry)
        }
    })
    .await
    .map_err(|_| ApiError::internal())?;
    let (status, value) = match result {
        Ok(value) => (StatusCode::OK, value),
        Err(value) => (StatusCode::BAD_REQUEST, value),
    };
    let response = serde_json::from_value(value).map_err(|_| ApiError::internal())?;
    Ok((status, Json(response)))
}

fn account_catalog(
    catalog: &ModelCatalogService,
    snapshot: &AccountsSnapshot,
    account_key: &str,
    retry: bool,
) -> Result<Value, Value> {
    let Some(account) = snapshot
        .accounts
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(account_key))
    else {
        return Err(json!({"error": "Unknown Codex account"}));
    };
    let home = account
        .get("home")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let provider = account
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("codex");
    match catalog.read(account_key, home, provider, retry) {
        CatalogRead::Ready(value) => Ok(value),
        CatalogRead::Pending => Err(json!({"error": PENDING_MESSAGE, "catalogPending": true})),
        CatalogRead::Unavailable => Err(json!({"error": UNAVAILABLE_MESSAGE})),
    }
}

fn worker_catalog(
    catalog: &ModelCatalogService,
    snapshot: &AccountsSnapshot,
    parent_account: &str,
    retry: bool,
) -> Result<Value, Value> {
    let default_account = snapshot.default_account_key.as_str().unwrap_or("default");
    let mut order = Vec::new();
    for key in [parent_account, default_account].into_iter().chain(
        snapshot
            .accounts
            .iter()
            .filter_map(|row| row.get("id").and_then(Value::as_str)),
    ) {
        if !order.iter().any(|existing| existing == key) {
            order.push(key.to_owned());
        }
    }

    let mut models = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut unavailable = Vec::new();
    let mut pending = false;
    for key in order {
        let Some(account) = snapshot
            .accounts
            .iter()
            .find(|row| row.get("id").and_then(Value::as_str) == Some(key.as_str()))
        else {
            continue;
        };
        if account.get("disconnected") == Some(&Value::Bool(true))
            || account.get("deleted") == Some(&Value::Bool(true))
            || account
                .get("duplicateOf")
                .is_some_and(|value| !value.is_null())
            || (key != parent_account
                && account.get("status").and_then(Value::as_str) != Some("ready")
                && account.get("canAttemptNativeProof") != Some(&Value::Bool(true)))
        {
            continue;
        }
        let home = account
            .get("home")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let provider = account
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("codex");
        match catalog.read(&key, home, provider, retry) {
            CatalogRead::Ready(value) => {
                if let Some(rows) = value.get("data").and_then(Value::as_array) {
                    for row in rows {
                        let Some(model) = row
                            .get("model")
                            .and_then(Value::as_str)
                            .filter(|model| !model.is_empty())
                        else {
                            continue;
                        };
                        if row.get("hidden") == Some(&Value::Bool(true))
                            || !seen.insert(model.to_owned())
                        {
                            continue;
                        }
                        let mut row = row.clone();
                        if let Some(row) = row.as_object_mut() {
                            row.insert("accountKey".to_owned(), Value::String(key.clone()));
                            row.insert("provider".to_owned(), Value::String(provider.to_owned()));
                        }
                        models.push(row);
                    }
                }
            }
            CatalogRead::Pending => {
                pending = true;
                unavailable.push(json!({
                    "accountKey": key,
                    "error": PENDING_MESSAGE,
                    "catalogPending": true,
                }));
            }
            CatalogRead::Unavailable => unavailable.push(json!({
                "accountKey": key,
                "error": UNAVAILABLE_MESSAGE,
            })),
        }
    }
    if models.is_empty() {
        if pending {
            return Err(json!({"error": WORKER_PENDING_MESSAGE, "catalogPending": true}));
        }
        return Err(json!({"error": "No worker model catalog is available"}));
    }
    let mut response = json!({"data": models, "unavailableAccounts": unavailable});
    if pending {
        response["catalogPending"] = Value::Bool(true);
    }
    Ok(response)
}
