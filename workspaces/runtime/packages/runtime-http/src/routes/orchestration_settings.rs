//! Server-wide orchestration settings that the scheduler and the account
//! workers read on every use.

use axum::{Json, extract::State};
use serde::{Deserialize, Serialize};
use studio_runtime_schema::{idle_compaction, shared_values::*, shell_policy};
use studio_runtime_store::{
    CHILD_RESULT_COALESCE_SETTING, IDLE_SESSION_UNLOAD_SETTING, OrchestrationSetting,
    OrchestrationSettingsRepository,
};

use super::{
    extractors::ApiJson,
    manifest::{OperationStatus, RouteGroup, get, post},
};
use crate::{error::ApiError, router::HttpState};

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

struct Bounded<const MIN: u64, const MAX: u64>(u64);
impl<'de, const MIN: u64, const MAX: u64> Deserialize<'de> for Bounded<MIN, MAX> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = u64::deserialize(d)?;
        if (MIN..=MAX).contains(&value) {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom("value outside bounds"))
        }
    }
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Quiet {
    Disabled(Bounded<0, 0>),
    Enabled(Bounded<{ shell_policy::QUIET_MIN_MS }, { MAX_SAFE_INTEGER as u64 }>),
}
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    d: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Idle {
    mode: idle_compaction::Mode,
    min_tokens: Bounded<{ IDLE_COMPACTION_MIN_TOKENS }, { MAX_SAFE_INTEGER as u64 }>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Shell {
    quiet_threshold_ms: Quiet,
    pressure_cleanup: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SettingsInput {
    child_result_coalesce_seconds:
        Bounded<{ CHILD_RESULT_COALESCE_MIN_SECONDS }, { CHILD_RESULT_COALESCE_MAX_SECONDS }>,
    idle_session_unload_minutes:
        Bounded<{ IDLE_SESSION_UNLOAD_MIN_MINUTES }, { IDLE_SESSION_UNLOAD_MAX_MINUTES }>,
    #[serde(default, deserialize_with = "present")]
    idle_compaction: Option<Idle>,
    #[serde(default, deserialize_with = "present")]
    shell_policy: Option<Shell>,
}
#[derive(Serialize)]
struct SettingsResponse {
    settings: SettingsData,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SettingsData {
    child_result_coalesce_seconds: u64,
    idle_session_unload_minutes: u64,
    idle_compaction: idle_compaction::Settings,
    shell_policy: shell_policy::Settings,
}
fn response(
    values: [u64; 2],
    idle: idle_compaction::Settings,
    shells: shell_policy::Settings,
) -> Json<SettingsResponse> {
    Json(SettingsResponse {
        settings: SettingsData {
            child_result_coalesce_seconds: values[0],
            idle_session_unload_minutes: values[1],
            idle_compaction: idle,
            shell_policy: shells,
        },
    })
}

async fn read_settings(State(state): State<HttpState>) -> Result<Json<SettingsResponse>, ApiError> {
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
    Ok(response(values, idle, shells))
}

async fn write_settings(
    State(state): State<HttpState>,
    ApiJson(settings): ApiJson<SettingsInput>,
) -> Result<Json<SettingsResponse>, ApiError> {
    let shells = settings.shell_policy.map(|value| shell_policy::Settings {
        quiet_threshold_ms: match value.quiet_threshold_ms {
            Quiet::Disabled(value) => value.0,
            Quiet::Enabled(value) => value.0,
        },
        pressure_cleanup: value.pressure_cleanup,
    });
    let idle = settings
        .idle_compaction
        .map(|value| idle_compaction::Settings {
            mode: value.mode,
            min_tokens: value.min_tokens.0,
        });
    let values = [
        settings.child_result_coalesce_seconds.0,
        settings.idle_session_unload_minutes.0,
    ];
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
    Ok(response(values, idle, shells))
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "orchestration_settings", routes: Some(routes), dependencies: &[],
    }
}

#[cfg(test)]
mod tests {
    use super::SettingsInput;
    use serde_json::json;

    #[test]
    fn settings_input_matches_openapi_and_shell_policy_bounds() {
        let mut document = crate::schema::openapi_document().clone();
        document["$ref"] = json!("#/components/schemas/OrchestrationSettings");
        let validator = jsonschema::validator_for(&document).expect("settings schema");
        for (quiet, accepted) in [(0, true), (1, false), (9999, false), (10000, true)] {
            let value = json!({"childResultCoalesceSeconds":10,"idleSessionUnloadMinutes":15,
                "shellPolicy":{"quietThresholdMs":quiet,"pressureCleanup":true}});
            assert_eq!(validator.is_valid(&value), accepted, "schema: {value}");
            assert_eq!(
                serde_json::from_value::<SettingsInput>(value.clone()).is_ok(),
                accepted,
                "input: {value}"
            );
        }
    }
}
