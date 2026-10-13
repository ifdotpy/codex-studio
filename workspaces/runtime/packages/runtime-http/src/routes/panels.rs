//! Profile operations and progress-panel endpoints.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use studio_runtime_services::panels::{self, ProfileField};
use studio_runtime_services::progress_panel::{self, ProgressPanelError};
use studio_runtime_store::{
    AgentId, AgentRepository, ProfileRepository, ProfileRole, ReasoningEffort, RuntimeStore,
    StoreError,
};

use super::extractors::{ApiJson, QueryParams};
#[cfg(windows)]
use super::manifest::{Dependency, dependency_error};
use super::manifest::{OperationStatus, RouteGroup, get, post};
use crate::error::ApiError;
use crate::router::HttpState;

#[cfg(windows)]
const WINDOWS_FILE_IDENTITY: &[Dependency] =
    &[Dependency::new("resource-policy/windows-file-identity")];

#[cfg(windows)]
fn panel_status() -> OperationStatus {
    OperationStatus::partial(WINDOWS_FILE_IDENTITY)
}

#[cfg(not(windows))]
fn panel_status() -> OperationStatus {
    OperationStatus::Implemented
}

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/panel", panel_status(), panel),
        post("/api/panel/layout", panel_status(), save_panel_layout),
        get("/api/profiles", OperationStatus::Implemented, profiles),
        post(
            "/api/profiles",
            OperationStatus::Implemented,
            update_profiles,
        ),
    ])
}

async fn panel(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    #[cfg(windows)]
    {
        let _ = (state, query);
        return Err(dependency_error("GET", "/api/panel", WINDOWS_FILE_IDENTITY));
    }
    #[cfg(not(windows))]
    {
        let agent = query
            .typed_value("agent")
            .and_then(Value::as_str)
            .ok_or_else(|| validation_error("agent"))?
            .to_owned();
        let store = state.store.ok_or_else(ApiError::internal)?;
        let root = store.root().to_owned();
        let result = tokio::task::spawn_blocking(move || {
            checked_agent(&store, &agent)?;
            progress_panel::read_progress(&root, &agent).map_err(panel_read_error)
        })
        .await
        .map_err(|_| ApiError::internal())??;
        Ok(Json(super::generated_json::<
            crate::generated::ProgressPanel,
        >(result)?))
    }
}

async fn save_panel_layout(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    #[cfg(windows)]
    {
        let _ = (state, body);
        return Err(dependency_error(
            "POST",
            "/api/panel/layout",
            WINDOWS_FILE_IDENTITY,
        ));
    }
    #[cfg(not(windows))]
    {
        let agent = body
            .get("agent")
            .and_then(Value::as_str)
            .ok_or_else(|| validation_error("agent"))?
            .to_owned();
        let store = state.store.ok_or_else(ApiError::internal)?;
        let root = store.root().to_owned();
        let result = tokio::task::spawn_blocking(move || {
            checked_agent(&store, &agent)?;
            progress_panel::record_layout(&root, &body).map_err(layout_error)
        })
        .await
        .map_err(|_| ApiError::internal())??;
        Ok(Json(super::generated_json::<
            crate::generated::PanelLayoutResult,
        >(result)?))
    }
}

fn checked_agent(store: &RuntimeStore, id: &str) -> Result<(), ApiError> {
    let connection = store.read().map_err(|_| ApiError::internal())?;
    let id = AgentId::from(id);
    let agent = AgentRepository::get(&connection, &id)
        .map_err(|_| ApiError::internal())?
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown managed agent"))?;
    if agent.extra.get("deletedAt").is_some_and(json_truthy) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "This agent was deleted",
        ));
    }
    Ok(())
}

fn panel_read_error(error: ProgressPanelError) -> ApiError {
    match error {
        ProgressPanelError::InvalidAgent => field_validation("query", "agent"),
        _ => ApiError::internal(),
    }
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn layout_error(error: ProgressPanelError) -> ApiError {
    let message = error.to_string();
    match error {
        ProgressPanelError::InvalidAgent => field_validation("body", "agent"),
        ProgressPanelError::InvalidLayout("fields") => {
            ApiError::validation(vec![Value::String("body".to_owned())])
        }
        ProgressPanelError::InvalidLayout(field) => field_validation("body", field),
        ProgressPanelError::StaleRevision
        | ProgressPanelError::NewerMeasurement
        | ProgressPanelError::TooManyClients
        | ProgressPanelError::Busy
        | ProgressPanelError::ChangedWhileSaving => ApiError::new(StatusCode::CONFLICT, message),
        _ => ApiError::internal(),
    }
}

fn field_validation(section: &str, field: &str) -> ApiError {
    ApiError::validation(vec![
        Value::String(section.to_owned()),
        Value::String(field.to_owned()),
    ])
}

async fn profiles(State(state): State<HttpState>) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let profiles = tokio::task::spawn_blocking(move || -> Result<Value, ApiError> {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let records = ProfileRepository::list(&connection).map_err(profile_read_error)?;
        Ok(json!({"profiles": records}))
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(
        super::generated_json::<crate::generated::ProfileList>(profiles)?,
    ))
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
    Ok(Json(super::generated_json::<
        crate::generated::ProfileMutation,
    >(result)?))
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
