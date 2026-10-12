//! Read the durable rule list.

use super::manifest::{OperationStatus, RouteGroup, get};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use studio_runtime_services::rules::{self, RulesError};
use studio_runtime_store::{RulesRepository, StoreError};

use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![get("/api/rules", OperationStatus::Implemented, list)])
}

pub(crate) async fn list(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let records = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let (rules, agents) = RulesRepository::list(&connection).map_err(store_error)?;
        let rules = rules
            .into_iter()
            .map(|mut rule| {
                let dto =
                    serde_json::from_str::<crate::generated::RuleRecordResponse>(&rule.record)
                        .map_err(|_| invalid_stored_rule(&rule.id))?;
                rule.record = serde_json::to_string(&dto).map_err(|_| ApiError::internal())?;
                Ok(rule)
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        rules::visible_rules(rules, agents).map_err(rules_error)
    })
    .await
    .map_err(|_| ApiError::internal())??;

    let output = records
        .into_iter()
        .map(|rule| serde_json::from_str::<Value>(&rule.record).map_err(|_| ApiError::internal()))
        .collect::<Result<Vec<_>, _>>()?;
    let response = json!({"rules": output});
    Ok(Json(generated_json::<crate::generated::RuleList>(
        response,
    )?))
}

fn rules_error(error: RulesError) -> ApiError {
    match error {
        RulesError::InvalidStoredObject { object, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}/{id}"),
        ),
    }
}

fn store_error(error: StoreError) -> ApiError {
    match error {
        StoreError::InvalidStoredObject { table, id } => {
            let object = match table {
                "runtime_agents" => "agents",
                "runtime_rules" => "rules",
                other => other,
            };
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid stored object {object}/{id}"),
            )
        }
        _ => ApiError::internal(),
    }
}

fn invalid_stored_rule(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object rules/{id}"),
    )
}

fn generated_json<T>(value: Value) -> Result<Value, ApiError>
where
    T: DeserializeOwned + Serialize,
{
    let dto = serde_json::from_value::<T>(value).map_err(|_| ApiError::internal())?;
    serde_json::to_value(dto).map_err(|_| ApiError::internal())
}
