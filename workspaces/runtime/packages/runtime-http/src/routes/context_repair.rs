//! Explicit context repair reconciles an idle Codex history before safely forking it.
use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, post},
};
use crate::{error::ApiError, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use studio_runtime_services::context_repair::{self, RepairError};
pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/context-repair",
        OperationStatus::Implemented,
        repair,
    )])
}
async fn repair(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::IdRequest>,
) -> Result<Json<crate::generated::ContextRepairResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let driver = state.account_driver.clone();
    let broker = state.account_session.clone();
    let id = body.id;
    let result = match context_repair::repair_idle(std::sync::Arc::clone(&store), &id).await {
        Ok(result) => Ok(result),
        Err(RepairError::RewriteUnavailable) => {
            let prepare_store = store.clone();
            let prepare_id = id.clone();
            let home = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-http/routes/context_repair",
                move || {
                    let driver = driver.ok_or(RepairError::AccountSession)?;
                    let connection = prepare_store.read()?;
                    let agent = studio_runtime_store::AgentRepository::get(
                        &connection,
                        &studio_runtime_store::AgentId::from(prepare_id),
                    )?
                    .ok_or(RepairError::MissingAgent)?;
                    let account_key = agent
                        .extra
                        .get("accountKey")
                        .and_then(Value::as_str)
                        .unwrap_or("default");
                    let snapshot = driver.snapshot().map_err(|_| RepairError::AccountSession)?;
                    let account = snapshot
                        .accounts
                        .iter()
                        .find(|row| row.get("id").and_then(Value::as_str) == Some(account_key))
                        .ok_or(RepairError::AccountSession)?;
                    let home = account
                        .get("home")
                        .and_then(Value::as_str)
                        .map(PathBuf::from)
                        .ok_or(RepairError::AccountSession)?;
                    Ok::<_, RepairError>(home)
                },
            )
            .await
            .map_err(|_| ApiError::internal())?
            .map_err(|error| match error {
                RepairError::MissingAgent => {
                    ApiError::new(StatusCode::BAD_REQUEST, "Unknown agent")
                }
                RepairError::AccountSession => {
                    ApiError::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
                }
                _ => ApiError::internal(),
            })?;
            let broker = broker.ok_or_else(|| {
                ApiError::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    RepairError::AccountSession.to_string(),
                )
            })?;
            context_repair::repair_idle_with_broker(
                std::sync::Arc::clone(&store),
                &id,
                &home,
                broker.as_ref(),
                Instant::now() + Duration::from_secs(60),
            )
            .await
        }
        Err(error) => Err(error),
    }
    .map_err(|error| match error {
        RepairError::MissingAgent => ApiError::new(StatusCode::BAD_REQUEST, "Unknown agent"),
        RepairError::ReceiptHold => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Context repair awaits its exact native receipt",
        ),
        RepairError::RewriteUnavailable => ApiError::new(StatusCode::CONFLICT, error.to_string()),
        RepairError::MissingSchema => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
        }
        RepairError::ProviderUnknown | RepairError::ProviderRejected => {
            ApiError::new(StatusCode::CONFLICT, error.to_string())
        }
        RepairError::AccountSession => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, error.to_string())
        }
        _ => ApiError::internal(),
    })?;
    Ok(Json(super::generated_json::<
        crate::generated::ContextRepairResponse,
    >(result)?))
}
inventory::submit! { super::manifest::RouteModuleRegistration { name: "context_repair", routes: Some(routes), dependencies: &[] } }
