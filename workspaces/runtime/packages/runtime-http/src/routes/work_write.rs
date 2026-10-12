//! HTTP adapter for work mutations.

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_services::work_write::{self, CreateWork, WorkMutation, WorkWriteError};
use studio_runtime_store::{AgentId, AgentRepository, WorkItemsRepository};

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
    let agent_id = body
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or(&state.workspace_id)
        .to_owned();
    let action = body.get("action").and_then(Value::as_str).unwrap_or("list");
    match action {
        "create" => {
            if body
                .get("owner")
                .and_then(Value::as_str)
                .is_some_and(|owner| !owner.is_empty())
            {
                return Err(unavailable("worker-driver/work-assignment"));
            }
            let title = body
                .get("title")
                .and_then(Value::as_str)
                .ok_or_else(invalid_request)?
                .to_owned();
            let description = match body.get("description") {
                None => "",
                Some(value) => value.as_str().ok_or_else(invalid_request)?,
            }
            .to_owned();
            let signature = signature(&agent_id, &body)?;
            let request_id = body
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            let store = state.store.clone().ok_or_else(ApiError::internal)?;
            let validators = state.validators.clone();
            let output = tokio::task::spawn_blocking(move || {
                store
                    .transaction_with_error(|transaction| {
                        work_write::create(
                            transaction,
                            CreateWork {
                                agent_id: agent_id.clone(),
                                actor_id: agent_id,
                                request_id,
                                title,
                                description,
                                signature,
                            },
                            |record| validators.validate_stored("WorkItem", record),
                        )
                    })
                    .map_err(write_error)
            })
            .await
            .map_err(|_| ApiError::internal())??;
            Ok(Json(output.record))
        }
        "list" => {
            let store = state.store.clone().ok_or_else(ApiError::internal)?;
            let validators = state.validators.clone();
            let result = tokio::task::spawn_blocking(move || {
                let connection = store.read().map_err(|_| ApiError::internal())?;
                let agent = AgentRepository::get(&connection, &AgentId::from(agent_id.as_str()))
                    .map_err(|_| ApiError::internal())?
                    .ok_or_else(invalid_request)?;
                let mut records = Vec::new();
                for row in
                    WorkItemsRepository::work(&connection).map_err(|_| ApiError::internal())?
                {
                    let value: Value = serde_json::from_str(
                        row.record
                            .as_deref()
                            .ok_or_else(|| invalid_stored(&row.id))?,
                    )
                    .map_err(|_| invalid_stored(&row.id))?;
                    if value.get("id").and_then(Value::as_str) != Some(row.id.as_str())
                        || !validators.validate_stored("WorkItem", &value)
                    {
                        return Err(invalid_stored(&row.id));
                    }
                    records.push(value);
                }
                Ok(studio_runtime_services::work_policy::work_list(
                    &agent.root_id.0,
                    &records,
                ))
            })
            .await
            .map_err(|_| ApiError::internal())??;
            Ok(Json(result))
        }
        "claim" => Err(unavailable("worker-driver/work-assignment")),
        "submit" => Err(unavailable("workspace-state/submission-files")),
        "accept" => Err(unavailable("workspace-state/agent-archive")),
        "reject" | "cancel" | "update" => {
            if action == "update"
                && ["owner", "status", "dependencies"]
                    .iter()
                    .any(|field| body.get(*field).is_some())
            {
                return Err(unavailable("worker-driver/work-assignment"));
            }
            let task_id = body
                .get("task_id")
                .and_then(Value::as_str)
                .ok_or_else(invalid_request)?
                .to_owned();
            let version = body.get("version").and_then(Value::as_i64);
            let mutation = match action {
                "update" => WorkMutation::Update {
                    task_id,
                    version,
                    title: optional_text_field(&body, "title")?,
                    description: optional_text_field(&body, "description")?,
                },
                "reject" => WorkMutation::Reject {
                    task_id,
                    version,
                    reason: body
                        .get("result")
                        .and_then(Value::as_str)
                        .ok_or_else(invalid_request)?
                        .to_owned(),
                },
                _ => WorkMutation::Cancel {
                    task_id,
                    version,
                    reason: body
                        .get("reason")
                        .and_then(Value::as_str)
                        .ok_or_else(invalid_request)?
                        .to_owned(),
                },
            };
            let signature = signature(&agent_id, &body)?;
            let request_id = body
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            let store = state.store.clone().ok_or_else(ApiError::internal)?;
            let validators = state.validators.clone();
            let wake = state.event_wake.clone();
            let output = tokio::task::spawn_blocking(move || {
                let result = store
                    .transaction_with_error(|transaction| {
                        work_write::mutate(
                            transaction,
                            &agent_id,
                            request_id.as_deref(),
                            &signature,
                            mutation,
                            |record| validators.validate_stored("WorkItem", record),
                        )
                    })
                    .map_err(write_error)?;
                if result.event_inserted {
                    wake.notify_one();
                }
                Ok(result.record)
            })
            .await
            .map_err(|_| ApiError::internal())??;
            Ok(Json(output))
        }
        _ => Err(invalid_request()),
    }
}

fn signature(agent_id: &str, body: &Value) -> Result<String, ApiError> {
    let value = json!({"agent": agent_id, "actor": null, "body": body});
    let digest = Sha256::digest(python_json(&value).into_bytes());
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn python_json(value: &Value) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => python_quote(value),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_json)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => {
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort();
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|key| format!("{}: {}", python_quote(key), python_json(&values[key])))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

fn python_quote(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{8}' => output.push_str("\\b"),
            '\u{c}' => output.push_str("\\f"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character < ' ' => {
                output.push_str(&format!("\\u{:04x}", character as u32));
            }
            character if character as u32 > 0x7f => {
                let scalar = character as u32;
                if scalar <= 0xffff {
                    output.push_str(&format!("\\u{scalar:04x}"));
                } else {
                    let scalar = scalar - 0x10000;
                    output.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (scalar >> 10),
                        0xdc00 + (scalar & 0x3ff)
                    ));
                }
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}

fn optional_text_field(body: &Value, name: &str) -> Result<Option<String>, ApiError> {
    body.get(name)
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(invalid_request)
        })
        .transpose()
}

fn unavailable(dependency: &'static str) -> ApiError {
    ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        format!("Not implemented: requires {dependency}"),
    )
}

fn invalid_request() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "Invalid request")
}

fn invalid_stored(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object runtime_work/{id}"),
    )
}

fn write_error(error: WorkWriteError) -> ApiError {
    match error {
        WorkWriteError::InvalidRequest(message) | WorkWriteError::Conflict(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        WorkWriteError::InvalidStoredObject { object, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}/{id}"),
        ),
        WorkWriteError::EventWrite(error) => match error {
            studio_runtime_services::event_write::EventWriteError::InvalidRequest(message)
            | studio_runtime_services::event_write::EventWriteError::Conflict(message) => {
                ApiError::new(StatusCode::BAD_REQUEST, message)
            }
            studio_runtime_services::event_write::EventWriteError::InvalidStoredObject {
                object,
                id,
            } => ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid stored object {object}/{id}"),
            ),
            _ => ApiError::internal(),
        },
        WorkWriteError::Store(_) | WorkWriteError::Clock => ApiError::internal(),
    }
}
