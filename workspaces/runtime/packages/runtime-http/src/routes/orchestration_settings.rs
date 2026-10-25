//! Server-wide orchestration settings that the scheduler and the account
//! workers read on every use.

use axum::{Json, extract::State};
use serde_json::{Map, Value};
use studio_runtime_store::{
    CHILD_RESULT_COALESCE_SETTING, IDLE_SESSION_UNLOAD_SETTING, OrchestrationSetting,
    OrchestrationSettingsRepository,
};

use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, get, post},
};
use crate::{error::ApiError, generated, router::HttpState};

/// Every field of the settings record, in response order.
const SETTINGS: [OrchestrationSetting; 2] =
    [CHILD_RESULT_COALESCE_SETTING, IDLE_SESSION_UNLOAD_SETTING];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/orchestration/settings",
            OperationStatus::Implemented,
            read_settings,
        ),
        post(
            "/api/orchestration/settings",
            OperationStatus::Implemented,
            write_settings,
        ),
    ])
}

fn response(
    values: [u64; 2],
    idle: studio_runtime_schema::idle_compaction::Settings,
    shells: studio_runtime_schema::shell_policy::Settings,
) -> Result<Json<generated::OrchestrationSettingsResponse>, ApiError> {
    let mut settings = SETTINGS
        .iter()
        .zip(values)
        .map(|(setting, value)| (setting.field.to_owned(), Value::from(value)))
        .collect::<Map<_, _>>();
    settings.insert(
        "idleCompaction".into(),
        serde_json::to_value(idle).map_err(|_| ApiError::internal())?,
    );
    settings.insert(
        "shellPolicy".into(),
        serde_json::to_value(shells).map_err(|_| ApiError::internal())?,
    );
    Ok(Json(super::generated_json(
        serde_json::json!({ "settings": settings }),
    )?))
}

async fn read_settings(
    State(state): State<HttpState>,
) -> Result<Json<generated::OrchestrationSettingsResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let (values, idle, shells) = store
        .read_async("runtime-http/routes/orchestration_settings", |connection| {
            let mut values = [0; 2];
            for (value, setting) in values.iter_mut().zip(SETTINGS) {
                *value = OrchestrationSettingsRepository::value(connection, setting)?;
            }
            Ok::<_, studio_runtime_store::StoreError>((
                values,
                OrchestrationSettingsRepository::idle_compaction(connection)?,
                OrchestrationSettingsRepository::shell_policy(connection)?,
            ))
        })
        .await
        .map_err(|_| ApiError::internal())?;
    response(values, idle, shells)
}

async fn write_settings(
    State(state): State<HttpState>,
    ApiJson(settings): ApiJson<generated::OrchestrationSettings>,
) -> Result<Json<generated::OrchestrationSettingsResponse>, ApiError> {
    let shells = settings
        .shell_policy
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| ApiError::internal())?
        .map(serde_json::from_value::<studio_runtime_schema::shell_policy::Settings>)
        .transpose()
        .map_err(|_| {
            ApiError::validation(vec![
                Value::String("body".into()),
                Value::String("shellPolicy".into()),
            ])
        })?;
    if shells.as_ref().is_some_and(|settings| !settings.valid()) {
        return Err(ApiError::validation(vec![
            Value::String("body".into()),
            Value::String("shellPolicy".into()),
            Value::String("quietThresholdMs".into()),
        ]));
    }
    let idle = settings
        .idle_compaction
        .as_ref()
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| ApiError::internal())?
        .map(serde_json::from_value::<studio_runtime_schema::idle_compaction::Settings>)
        .transpose()
        .map_err(|_| {
            ApiError::validation(vec![
                Value::String("body".into()),
                Value::String("idleCompaction".into()),
            ])
        })?;
    if idle.as_ref().is_some_and(|settings| !settings.valid()) {
        return Err(ApiError::validation(vec![
            Value::String("body".into()),
            Value::String("idleCompaction".into()),
            Value::String("minTokens".into()),
        ]));
    }
    let requested = [
        settings.child_result_coalesce_seconds,
        settings.idle_session_unload_minutes,
    ];
    let mut values = [0; 2];
    for ((value, requested), setting) in values.iter_mut().zip(requested).zip(SETTINGS) {
        *value = u64::try_from(requested)
            .ok()
            .filter(|value| setting.valid(*value))
            .ok_or_else(|| {
                ApiError::validation(vec![
                    Value::String("body".to_owned()),
                    Value::String(setting.field.to_owned()),
                ])
            })?;
    }
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let (idle, shells) = store
        .transaction(move |transaction| {
            for (value, setting) in values.into_iter().zip(SETTINGS) {
                OrchestrationSettingsRepository::set(transaction, setting, value)?;
            }
            if let Some(idle) = idle {
                OrchestrationSettingsRepository::set_idle_compaction(transaction, &idle)?;
            }
            if let Some(shells) = shells {
                OrchestrationSettingsRepository::set_shell_policy(transaction, &shells)?;
            }
            Ok((
                OrchestrationSettingsRepository::idle_compaction(transaction)?,
                OrchestrationSettingsRepository::shell_policy(transaction)?,
            ))
        })
        .await
        .map_err(|_| ApiError::internal())?;
    // Held results are re-evaluated against the new hold at once; account
    // workers read the unload delay on their next idle sweep.
    state.event_wake.notify_one();
    response(values, idle, shells)
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "orchestration_settings", routes: Some(routes), dependencies: &[],
    }
}
