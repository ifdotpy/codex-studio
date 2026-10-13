//! Account-scoped provider model catalogs.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::router::HttpState;
use crate::routes::extractors::QueryParams;
use crate::routes::manifest::{Dependency, OperationStatus, RouteGroup, get};
use studio_runtime_services::account_policy::AccountsSnapshot;
use studio_runtime_services::model_catalog::{CatalogRead, ModelCatalogService};

const PENDING_MESSAGE: &str = "Model catalog is pending; no workers were created. The existing metadata request remains available for recovery";
const WORKER_PENDING_MESSAGE: &str =
    "Model catalog is pending; no worker model catalog is available yet";
const UNAVAILABLE_MESSAGE: &str =
    "Model catalog unavailable; no workers were created: provider request failed";
const ACCOUNT_UNAVAILABLE_MESSAGE: &str = "Account is unavailable";
const CLAUDE_CATALOG_DEPENDENCY: &[Dependency] =
    &[Dependency::new("provider-service/claude-model-catalog")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![get(
        "/api/models",
        OperationStatus::partial(CLAUDE_CATALOG_DEPENDENCY),
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
    let snapshot = tokio::task::spawn_blocking(move || account_policy.accounts())
        .await
        .map_err(|_| ApiError::internal())?;
    catalog.retain_accounts(&snapshot.accounts);
    let result = if workers {
        worker_catalog(&catalog, &snapshot, &account_key, retry).await
    } else {
        account_catalog(&catalog, &snapshot, &account_key, retry).await
    };
    let (status, value) = match result {
        Ok(value) => (StatusCode::OK, value),
        Err((status, value)) => (status, value),
    };
    let response = serde_json::from_value(value).map_err(|_| ApiError::internal())?;
    Ok((status, Json(response)))
}

async fn account_catalog(
    catalog: &ModelCatalogService,
    snapshot: &AccountsSnapshot,
    account_key: &str,
    retry: bool,
) -> Result<Value, (StatusCode, Value)> {
    let account = account_context(snapshot, account_key)?;
    if account.provider == "claude" {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            json!({
                "error": "Not implemented: requires provider-service/claude-model-catalog"
            }),
        ));
    }
    match catalog
        .read(&account.key, &account.home, &account.provider, retry)
        .await
    {
        CatalogRead::Ready(value) => Ok(value),
        CatalogRead::Pending => Err((
            StatusCode::BAD_REQUEST,
            json!({"error": PENDING_MESSAGE, "catalogPending": true}),
        )),
        CatalogRead::Unavailable => Err((
            StatusCode::BAD_REQUEST,
            json!({"error": UNAVAILABLE_MESSAGE}),
        )),
    }
}

struct AccountContext {
    key: String,
    home: String,
    provider: String,
}

fn account_context(
    snapshot: &AccountsSnapshot,
    account_key: &str,
) -> Result<AccountContext, (StatusCode, Value)> {
    let Some(account) = snapshot
        .accounts
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(account_key))
    else {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": "Unknown Codex account"}),
        ));
    };
    if matches!(
        account.get("status").and_then(Value::as_str),
        Some("changed" | "error")
    ) {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": ACCOUNT_UNAVAILABLE_MESSAGE}),
        ));
    }
    Ok(AccountContext {
        key: account_key.to_owned(),
        home: account
            .get("home")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        provider: account
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("codex")
            .to_owned(),
    })
}

async fn worker_catalog(
    catalog: &ModelCatalogService,
    snapshot: &AccountsSnapshot,
    parent_account: &str,
    retry: bool,
) -> Result<Value, (StatusCode, Value)> {
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
        let Some(source) = snapshot
            .accounts
            .iter()
            .find(|row| row.get("id").and_then(Value::as_str) == Some(key.as_str()))
        else {
            continue;
        };
        if source.get("disconnected") == Some(&Value::Bool(true))
            || source.get("deleted") == Some(&Value::Bool(true))
            || source
                .get("duplicateOf")
                .is_some_and(|value| !value.is_null())
            || (key != parent_account
                && source.get("status").and_then(Value::as_str) != Some("ready")
                && source.get("canAttemptNativeProof") != Some(&Value::Bool(true)))
        {
            continue;
        }
        let account = match account_context(snapshot, &key) {
            Ok(account) => account,
            Err((_, error)) => {
                unavailable.push(json!({"accountKey": key, "error": error["error"]}));
                continue;
            }
        };
        match catalog
            .read(&account.key, &account.home, &account.provider, retry)
            .await
        {
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
                            row.insert(
                                "provider".to_owned(),
                                Value::String(account.provider.clone()),
                            );
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
            return Err((
                StatusCode::BAD_REQUEST,
                json!({"error": WORKER_PENDING_MESSAGE, "catalogPending": true}),
            ));
        }
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": "No worker model catalog is available"}),
        ));
    }
    let mut response = json!({"data": models, "unavailableAccounts": unavailable});
    if pending {
        response["catalogPending"] = Value::Bool(true);
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::{account_catalog, account_context};
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use std::collections::HashSet;
    use studio_runtime_services::account_policy::AccountsSnapshot;
    use studio_runtime_services::model_catalog::ModelCatalogService;

    fn snapshot(account: Value) -> AccountsSnapshot {
        AccountsSnapshot {
            accounts: vec![account],
            archived_accounts: Vec::new(),
            default_account_key: json!("default"),
            logins: Vec::new(),
            known_account_keys: HashSet::new(),
        }
    }

    #[tokio::test]
    async fn changed_and_error_accounts_are_refused_before_catalog_read() {
        for status in ["changed", "error"] {
            let snapshot = snapshot(json!({"id":"default", "home":"/missing", "status":status}));
            let error =
                account_catalog(&ModelCatalogService::new(None), &snapshot, "default", true)
                    .await
                    .expect_err("changed account must be refused");
            assert_eq!(error.0, StatusCode::BAD_REQUEST);
            assert_eq!(error.1, json!({"error":"Account is unavailable"}));
        }
    }

    #[tokio::test]
    async fn direct_claude_catalog_names_the_missing_provider_service() {
        let snapshot =
            snapshot(json!({"id":"claude-account", "provider":"claude", "status":"ready"}));
        let error = account_catalog(
            &ModelCatalogService::new(None),
            &snapshot,
            "claude-account",
            false,
        )
        .await
        .expect_err("Claude requires its provider service");
        assert_eq!(error.0, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(
            error.1,
            json!({"error":"Not implemented: requires provider-service/claude-model-catalog"})
        );
    }

    #[test]
    fn account_home_and_provider_are_projected_together() {
        let snapshot = snapshot(
            json!({"id":"profile", "home":"/profile", "provider":"codex", "status":"ready"}),
        );
        let context = account_context(&snapshot, "profile").expect("profile exists");
        assert_eq!(context.key, "profile");
        assert_eq!(context.home, "/profile");
        assert_eq!(context.provider, "codex");
    }

    #[test]
    fn route_manifest_declares_claude_dependency_as_partial() {
        let operation = crate::routes::manifest::operation_manifest()
            .expect("route manifest validates")
            .into_iter()
            .find(|operation| operation.key() == ("GET".to_owned(), "/api/models".to_owned()))
            .expect("models operation is registered");
        assert_eq!(
            serde_json::to_value(operation).expect("operation serializes"),
            json!({
                "method":"GET",
                "path":"/api/models",
                "status":"partial",
                "dependencies":["provider-service/claude-model-catalog"]
            })
        );
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "models",
        routes: Some(routes),
        dependencies: &["provider-service/claude-model-catalog"],
    }
}
