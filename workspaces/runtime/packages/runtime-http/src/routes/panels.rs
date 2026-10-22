//! Profile operations.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use studio_runtime_services::panels::{self, ProfileField};
use studio_runtime_store::{ProfileRepository, ProfileRole, ReasoningEffort, StoreError};

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, get, post};
use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/profiles", OperationStatus::Implemented, profiles),
        post(
            "/api/profiles",
            OperationStatus::Implemented,
            update_profiles,
        ),
    ])
}

async fn profiles(
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::ProfileList>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let records = store
        .read_async("runtime-http/routes/panels", |connection| {
            ProfileRepository::list(connection).map_err(profile_read_error)
        })
        .await?;
    let profiles = json!({"profiles": records});
    Ok(Json(
        super::generated_json::<crate::generated::ProfileList>(profiles)?,
    ))
}

async fn update_profiles(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::ProfileBody>,
) -> Result<Json<crate::generated::ProfileMutation>, ApiError> {
    let action = body.action.unwrap_or_default();
    let requested_id = body.id.flatten().unwrap_or_default();
    let store = state.store.ok_or_else(ApiError::internal)?;
    let id = if requested_id.is_empty() {
        let id_store = store.clone();
        id_store
            .read_async("runtime-http/routes/panels", move |connection| {
                ProfileRepository::new_id(connection).map_err(|_| ApiError::internal())
            })
            .await?
    } else {
        requested_id
    };
    let result = if action == crate::generated::ProfileBodyAction::Delete {
        let deleted_id = id.clone();
        store
            .transaction_with(move |transaction| ProfileRepository::delete(transaction, &id))
            .await
            .map_err(|_| ApiError::internal())?;
        json!({"deleted": deleted_id})
    } else {
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
        let mut profile = panels::make_profile(
            id,
            &name,
            role,
            &model,
            effort,
            body.instructions.as_deref().unwrap_or_default(),
        )
        .map_err(profile_field_error)?;
        profile.capabilities = studio_runtime_schema::worker_capabilities::WorkerCapabilities {
            allowed_tools: body.allowed_tools.flatten(),
            disallowed_tools: body.disallowed_tools.flatten(),
            mcp_servers: body.mcp_servers.flatten(),
            skills: body.skills.flatten(),
            max_turns: body
                .max_turns
                .flatten()
                .map(u32::try_from)
                .transpose()
                .map_err(|_| validation_error("maxTurns"))?,
        };
        profile
            .capabilities
            .validate()
            .map_err(|message| ApiError::new(StatusCode::BAD_REQUEST, message))?;
        let response = serde_json::to_value(&profile).map_err(|_| ApiError::internal())?;
        store
            .transaction_with(move |transaction| ProfileRepository::save(transaction, &profile))
            .await
            .map_err(profile_read_error)?;
        response
    };
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
        StoreError::InvalidStoredObject { table, id } => {
            let object = table.strip_prefix("runtime_").unwrap_or(table);
            ApiError::invalid_stored(object, &id)
        }
        StoreError::SchemaMismatch(message)
            if message.starts_with("Invalid stored object profiles/") =>
        {
            let id = message
                .strip_prefix("Invalid stored object profiles/")
                .unwrap_or(&message);
            ApiError::invalid_stored("profiles", id)
        }
        StoreError::Sqlite(error)
            if error
                .to_string()
                .contains("no such table: runtime_profiles") =>
        {
            ApiError::invalid_stored("runtime_profiles", "<schema>")
        }
        _ => ApiError::internal(),
    }
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "panels",
        routes: Some(routes),
        dependencies: &[],
    }
}
