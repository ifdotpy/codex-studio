//! Durable transfer requests, worker selection, and receipt replay.
use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, post},
};
use crate::{error::ApiError, generated, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::Value;
use studio_runtime_services::{
    account_transfer::{Action, TransferError},
    account_transfer_driver,
};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/agents/account",
            OperationStatus::Implemented,
            selection,
        ),
        post(
            "/api/agents/account-transfer",
            OperationStatus::Implemented,
            transfer,
        ),
    ])
}
async fn selection(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::AccountSelectionRequest>,
) -> Result<Json<generated::AgentResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let driver = state.account_driver.clone();
    let policy = state.account_policy.clone();
    let accounts = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/account_transfer",
        move || {
            if let Some(driver) = driver {
                driver.snapshot().map_err(|_| ())
            } else if let Some(policy) = policy {
                Ok(policy.accounts())
            } else {
                Err(())
            }
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(|_| {
        error(TransferError::Stored {
            object: "account registry",
            pointer: "/".into(),
        })
    })?;
    let accounts_value = accounts.clone();
    let accounts = move || Ok::<_, TransferError>(accounts_value.clone());
    let value = studio_runtime_services::account_selection::select(
        &store,
        &accounts,
        &body.id,
        &body.account_key,
        body.cwd.as_ref().and_then(Option::as_deref),
    )
    .await
    .map_err(error)?;
    state.event_wake.notify_one();
    state.continuation_wake.notify_one();
    super::generated_json::<generated::AgentResponse>(value).map(Json)
}
#[derive(Deserialize)]
struct Request {
    request_id: String,
    action: Option<String>,
    id: Option<String>,
    account_key: Option<String>,
    scope: Option<String>,
}
async fn transfer(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Request>,
) -> Result<Json<generated::TransferResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = if let Some(action) = body.action {
        let action = match action.as_str() {
            "cancel" => Action::Cancel,
            "retry" => Action::Retry,
            "finish_history" => Action::FinishHistory,
            _ => {
                return Err(ApiError::validation(vec![
                    Value::String("body".into()),
                    Value::String("action".into()),
                ]));
            }
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ApiError::internal())?
            .as_secs_f64();
        account_transfer_driver::action(&store, &body.request_id, action, now)
            .await
            .map_err(error)?
    } else {
        let lead = body.id.as_deref().ok_or_else(|| {
            ApiError::validation(vec![
                Value::String("body".into()),
                Value::String("id".into()),
            ])
        })?;
        let target = body.account_key.as_deref().ok_or_else(|| {
            ApiError::validation(vec![
                Value::String("body".into()),
                Value::String("account_key".into()),
            ])
        })?;
        let registered = if let Some(driver) = state.account_driver.clone() {
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/account_transfer",
                move || {
                    driver
                        .snapshot()
                        .map(|snapshot| snapshot.known_account_keys)
                },
            )
            .await
            .map_err(|_| ApiError::internal())?
            .map_err(|_| ApiError::internal())?
            .contains(target)
        } else if let Some(policy) = &state.account_policy {
            policy.has_account(target)
        } else {
            return Err(error(TransferError::Refused("Unknown account")));
        };
        if !registered {
            return Err(error(TransferError::Refused("Unknown account")));
        }
        if state.account_policy.as_ref().is_some_and(|policy| {
            policy.accounts().accounts.iter().any(|account| {
                account["id"].as_str() == Some(target)
                    && (account
                        .get("deleted")
                        .is_some_and(|value| studio_runtime_store::truthy(Some(value)))
                        || account
                            .get("disconnected")
                            .is_some_and(|value| studio_runtime_store::truthy(Some(value))))
            })
        }) {
            return Err(error(TransferError::Refused(
                "Reconnect this account before transferring a team to it",
            )));
        }
        let target_provider = state
            .model_catalog
            .account_profile(target)
            .map(|(_, provider)| provider)
            .or_else(|| {
                state.account_policy.as_ref().and_then(|policy| {
                    policy
                        .accounts()
                        .accounts
                        .into_iter()
                        .find(|account| account["id"].as_str() == Some(target))
                        .and_then(|account| {
                            account
                                .get("provider")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                        })
                })
            })
            .or_else(|| (target == "default").then(|| "codex".to_owned()))
            .ok_or_else(|| error(TransferError::Refused("Unknown account provider")))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ApiError::internal())?
            .as_secs_f64();
        account_transfer_driver::request(
            &store,
            &body.request_id,
            lead,
            target,
            body.scope.as_deref().unwrap_or("team"),
            &target_provider,
            now,
        )
        .await
        .map_err(error)?
    };
    state.event_wake.notify_one();
    state.continuation_wake.notify_one();
    super::generated_json::<generated::TransferResponse>(result).map(Json)
}
fn error(error: TransferError) -> ApiError {
    let status = match error {
        TransferError::Requires(_) => StatusCode::NOT_IMPLEMENTED,
        TransferError::Stored { object, pointer } => {
            return ApiError::invalid_stored(object, &pointer);
        }
        TransferError::Store(_) => return ApiError::internal(),
        TransferError::Refused(_) => StatusCode::BAD_REQUEST,
    };
    ApiError::new(status, error.to_string())
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "account_transfer", routes: Some(routes), dependencies: &[
            "account-transfer/usage-resume-cancellation",
            "account-transfer/provider-history-executor", "account-transfer/destination-settings",
            "store-bootstrap/account-transfer-schema",
        ],
    }
}
