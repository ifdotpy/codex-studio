//! HTTP adapter for work mutations.

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, post};
use super::work_items;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::Value;
use studio_runtime_services::work_write::{
    self, AcceptWork, ClaimWork, CreateWork, SubmitWork, WorkMutation, WorkWrite, WorkWriteError,
};

use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![post(
        "/api/work",
        OperationStatus::Implemented,
        work_action,
    )])
}

pub(crate) async fn work_action(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let agent = body
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or(&state.workspace_id)
        .to_owned();
    let action = body.get("action").and_then(Value::as_str).unwrap_or("list");
    if action == "list" {
        return work_items::work_for_agent(state, agent).await.map(Json);
    }
    let request_id = body
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    let signature = work_items::work_request_signature(&agent, &body)?;
    let command = match action {
        "create" => WorkWrite::Create(CreateWork {
            actor_id: agent.clone(),
            title: required_text(&body, "title")?,
            description: optional_text(&body, "description")?.unwrap_or_default(),
            owner: body.get("owner").and_then(Value::as_str).map(str::to_owned),
            status: optional_text(&body, "status")?,
            dependencies: string_list(&body, "dependencies")?,
        }),
        "update" => WorkWrite::Mutate(WorkMutation::Update {
            task_id: required_text(&body, "task_id")?,
            version: version(&body)?,
            title: optional_text(&body, "title")?,
            description: optional_text(&body, "description")?,
            owner: body
                .get("owner")
                .map(|value| value.as_str().map(str::to_owned)),
            status: optional_text(&body, "status")?,
            dependencies: body
                .get("dependencies")
                .map(|_| string_list(&body, "dependencies"))
                .transpose()?,
        }),
        "cancel" => WorkWrite::Mutate(WorkMutation::Cancel {
            task_id: required_text(&body, "task_id")?,
            version: version(&body)?,
            reason: required_text(&body, "reason")?,
        }),
        "reject" => WorkWrite::Mutate(WorkMutation::Reject {
            task_id: required_text(&body, "task_id")?,
            version: version(&body)?,
            reason: required_text(&body, "result")?,
        }),
        "claim" => WorkWrite::Claim(ClaimWork {
            task_id: required_text(&body, "task_id")?,
            version: version(&body)?,
            claimant_id: body
                .get("owner")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| agent.clone()),
        }),
        "submit" => WorkWrite::Submit(SubmitWork {
            task_id: required_text(&body, "task_id")?,
            version: version(&body)?,
            result: required_text(&body, "result")?,
            checks: required_text(&body, "checks")?,
            revision: required_text(&body, "revision")?,
            files: string_list(&body, "files")?,
            state_dir: state.state_dir.clone().into(),
        }),
        "accept" => WorkWrite::Accept(AcceptWork {
            task_id: required_text(&body, "task_id")?,
            version: version(&body)?,
            reason: required_text(&body, "result")?,
        }),
        _ => return Err(invalid_request()),
    };
    commit(state, agent, request_id, signature, command)
        .await
        .map(Json)
}

async fn commit(
    state: HttpState,
    agent: String,
    request_id: Option<String>,
    signature: String,
    command: WorkWrite,
) -> Result<Value, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let wake = state.event_wake.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = store
            .transaction_with_error(|tx| {
                work_write::write(tx, &agent, request_id.as_deref(), &signature, command)
            })
            .map_err(write_error)?;
        if result.event_inserted {
            wake.notify_one();
        }
        if let Some(artifact) = &result.artifact {
            work_write::write_result_artifact(artifact).map_err(|_| ApiError::internal())?;
        }
        Ok::<_, ApiError>(result)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(result.record)
}

fn write_error(error: WorkWriteError) -> ApiError {
    match error {
        WorkWriteError::Event(error) => work_items::write_error(error),
        WorkWriteError::Store(_) => ApiError::internal(),
        WorkWriteError::NotImplemented(dependency) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {dependency}"),
        ),
    }
}

fn required_text(body: &Value, field: &str) -> Result<String, ApiError> {
    body.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(invalid_request)
}

fn optional_text(body: &Value, field: &str) -> Result<Option<String>, ApiError> {
    body.get(field)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(invalid_request)
        })
        .transpose()
}

fn string_list(body: &Value, field: &str) -> Result<Vec<String>, ApiError> {
    match body.get(field) {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(invalid_request)
            })
            .collect(),
        Some(_) => Err(invalid_request()),
    }
}

fn version(body: &Value) -> Result<Option<i64>, ApiError> {
    let Some(value) = body.get("version") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let number = value.as_number().ok_or_else(invalid_request)?;
    if let Some(value) = number.as_i64() {
        return Ok(Some(value));
    }
    let value = number.as_f64().ok_or_else(invalid_request)?;
    if value.is_finite()
        && value.fract() == 0.0
        && value >= i64::MIN as f64
        && value < 9_223_372_036_854_775_808.0
    {
        Ok(Some(value as i64))
    } else {
        Err(invalid_request())
    }
}

fn invalid_request() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "Invalid request")
}

#[cfg(test)]
mod tests {
    use super::version;
    use serde_json::json;

    #[test]
    fn integral_decimal_version_keeps_its_stale_value() {
        assert!(matches!(version(&json!({"version": 0.0})), Ok(Some(0))));
        assert!(version(&json!({"version": 1.5})).is_err());
    }
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "work_write",
        routes: Some(routes),
        dependencies: &["workspace-state/agent-archive", "workspace-state/submission-files"],
    }
}
