//! Transfer control and receipt replay. Provider-dependent starts refuse before writes.
use super::{
    extractors::ApiJson,
    manifest::{Dependency, OperationStatus, RouteGroup, post},
};
use crate::{error::ApiError, generated, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde::Deserialize;
use serde_json::Value;
use studio_runtime_services::{
    account_transfer::{Action, TransferError},
    account_transfer_driver,
};

const TRANSFER: &[Dependency] = &[
    Dependency::new("account-transfer/provider-history-executor"),
    Dependency::new("account-transfer/destination-settings"),
    Dependency::new("store-bootstrap/account-transfer-schema"),
];
const SELECTION: &[Dependency] = &[
    Dependency::new("account-driver/unavailable"),
    Dependency::new("account-transfer/usage-resume-cancellation"),
];
pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/agents/account",
            OperationStatus::partial(SELECTION),
            selection,
        ),
        post(
            "/api/agents/account-transfer",
            OperationStatus::partial(TRANSFER),
            transfer,
        ),
    ])
}
async fn selection(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::AccountSelectionRequest>,
) -> Result<Json<Value>, ApiError> {
    tokio::task::spawn_blocking(move || {
        let store = state.store.as_deref().ok_or_else(ApiError::internal)?;
        let accounts = || {
            if let Some(driver) = &state.account_driver {
                driver.snapshot().map_err(|_| TransferError::Stored {
                    object: "account registry",
                    pointer: "/".into(),
                })
            } else if let Some(policy) = &state.account_policy {
                Ok(policy.accounts())
            } else {
                Err(TransferError::Requires("account-driver/unavailable"))
            }
        };
        let value = studio_runtime_services::account_selection::select(
            store,
            &accounts,
            &body.id,
            &body.account_key,
            body.cwd.as_ref().and_then(Option::as_deref),
        )
        .map_err(error)?;
        state.event_wake.notify_one();
        super::generated_json::<generated::AgentResponse>(value).map(Json)
    })
    .await
    .map_err(|_| ApiError::internal())?
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
) -> Result<Json<Value>, ApiError> {
    tokio::task::spawn_blocking(move || {
        let store = state.store.as_deref().ok_or_else(|| {
            error(TransferError::Requires(
                "account-transfer/provider-history-executor",
            ))
        })?;
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
            account_transfer_driver::action(store, &body.request_id, action, now)
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
            let registered = if let Some(driver) = &state.account_driver {
                driver
                    .snapshot()
                    .map_err(|_| ApiError::internal())?
                    .known_account_keys
                    .contains(target)
            } else if let Some(policy) = &state.account_policy {
                policy.has_account(target)
            } else {
                return Err(error(TransferError::Requires(
                    "account-transfer/provider-history-executor",
                )));
            };
            if !registered {
                return Err(error(TransferError::Refused("Unknown account")));
            }
            account_transfer_driver::request(
                store,
                &body.request_id,
                lead,
                target,
                body.scope.as_deref().unwrap_or("team"),
            )
        }
        .map_err(error)?;
        state.event_wake.notify_one();
        super::generated_json::<generated::TransferResponse>(result).map(Json)
    })
    .await
    .map_err(|_| ApiError::internal())?
}
fn error(error: TransferError) -> ApiError {
    let status = match error {
        TransferError::Requires(_) => StatusCode::NOT_IMPLEMENTED,
        TransferError::Stored { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        TransferError::Store(_) => return ApiError::internal(),
        TransferError::Refused(_) => StatusCode::BAD_REQUEST,
    };
    ApiError::new(status, error.to_string())
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "account_transfer", routes: Some(routes), dependencies: &[
            "account-transfer/provider-history-executor", "account-transfer/destination-settings",
            "account-driver/unavailable", "account-transfer/usage-resume-cancellation", "store-bootstrap/account-transfer-schema",
        ],
    }
}
