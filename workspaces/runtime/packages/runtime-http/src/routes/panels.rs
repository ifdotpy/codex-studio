//! Profile operations and progress-panel endpoints.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use studio_runtime_services::panels::{self, ProfileField};
use studio_runtime_store::{ProfileRepository, ProfileRole, ReasoningEffort, StoreError};

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use crate::error::ApiError;
use crate::router::HttpState;

const PANEL_READ_DEPENDENCY: [Dependency; 1] =
    [Dependency::new("resource-policy/read-progress-panel")];
const PANEL_LAYOUT_DEPENDENCY: [Dependency; 1] =
    [Dependency::new("resource-policy/write-progress-layout")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/panel",
            OperationStatus::partial(&PANEL_READ_DEPENDENCY),
            panel,
        ),
        post(
            "/api/panel/layout",
            OperationStatus::partial(&PANEL_LAYOUT_DEPENDENCY),
            save_panel_layout,
        ),
        get("/api/profiles", OperationStatus::Implemented, profiles),
        post(
            "/api/profiles",
            OperationStatus::Implemented,
            update_profiles,
        ),
    ])
}

async fn panel(query: QueryParams) -> Result<Json<Value>, ApiError> {
    let _ = query;
    Err(dependency_error(
        "GET",
        "/api/panel",
        &PANEL_READ_DEPENDENCY,
    ))
}

async fn save_panel_layout(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "POST",
        "/api/panel/layout",
        &PANEL_LAYOUT_DEPENDENCY,
    ))
}

async fn profiles(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let profiles = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let records = ProfileRepository::list(&connection).map_err(profile_read_error)?;
        Ok(json!({"profiles": records}))
    })
    .await
    .map_err(|_| ApiError::internal())??;
    generated_json::<crate::generated::ProfileList>(profiles)
}

async fn update_profiles(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::ProfileBody>,
) -> Result<Json<Value>, ApiError> {
    let action = body.action.unwrap_or_default();
    let requested_id = body.id.flatten().unwrap_or_default();
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        if !ProfileRepository::available(&connection).map_err(|_| ApiError::internal())? {
            return Err(schema_dependency_error());
        }
        let id = if requested_id.is_empty() {
            ProfileRepository::new_id(&connection).map_err(|_| ApiError::internal())?
        } else {
            requested_id
        };
        drop(connection);
        if action == crate::generated::ProfileBodyAction::Delete {
            store
                .transaction(|transaction| ProfileRepository::delete(transaction, &id))
                .map_err(|_| ApiError::internal())?;
            return Ok(json!({"deleted": id}));
        }

        let name = body
            .name
            .flatten()
            .ok_or_else(|| validation_error("name"))?;
        let model = body
            .model
            .flatten()
            .ok_or_else(|| validation_error("model"))?;
        let role = match body.role.unwrap_or_default() {
            crate::generated::ProfileBodyRole::Reviewer => ProfileRole::Reviewer,
            crate::generated::ProfileBodyRole::Implementer => ProfileRole::Implementer,
        };
        let effort = body.effort.flatten().map(|effort| match effort {
            crate::generated::ProfileBodyEffort::Low => ReasoningEffort::Low,
            crate::generated::ProfileBodyEffort::Medium => ReasoningEffort::Medium,
            crate::generated::ProfileBodyEffort::High => ReasoningEffort::High,
            crate::generated::ProfileBodyEffort::Xhigh => ReasoningEffort::Xhigh,
            crate::generated::ProfileBodyEffort::Max => ReasoningEffort::Max,
            crate::generated::ProfileBodyEffort::Ultra => ReasoningEffort::Ultra,
        });
        let profile = panels::make_profile(
            id,
            &name,
            role,
            &model,
            effort,
            body.instructions.as_deref().unwrap_or_default(),
        )
        .map_err(profile_field_error)?;
        store
            .transaction(|transaction| ProfileRepository::save(transaction, &profile))
            .map_err(profile_read_error)?;
        serde_json::to_value(profile).map_err(|_| ApiError::internal())
    })
    .await
    .map_err(|_| ApiError::internal())??;
    generated_json::<crate::generated::ProfileMutation>(result)
}

fn profile_field_error(field: ProfileField) -> ApiError {
    validation_error(match field {
        ProfileField::Name => "name",
        ProfileField::Model => "model",
    })
}

fn validation_error(field: &'static str) -> ApiError {
    ApiError::validation(vec![
        Value::String("body".to_owned()),
        Value::String(field.to_owned()),
    ])
}

fn profile_read_error(error: StoreError) -> ApiError {
    match error {
        StoreError::SchemaMismatch(message)
            if message.starts_with("Invalid stored object profiles/") =>
        {
            ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, message)
        }
        _ => ApiError::internal(),
    }
}

fn schema_dependency_error() -> ApiError {
    ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        "Not implemented: requires store-bootstrap/workspace-profiles-schema",
    )
}

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}

fn generated_json<T>(value: Value) -> Result<Json<Value>, ApiError>
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let typed: T = serde_json::from_value(value).map_err(|_| ApiError::internal())?;
    serde_json::to_value(typed)
        .map(Json)
        .map_err(|_| ApiError::internal())
}
