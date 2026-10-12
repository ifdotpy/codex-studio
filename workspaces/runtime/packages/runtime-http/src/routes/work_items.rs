//! HTTP handlers for persisted work-item, plan and queue reads.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use studio_runtime_services::work_policy::{self, WorkPolicyError};
use studio_runtime_store::{AgentId, AgentRepository, WorkItemsRepository};

use crate::error::ApiError;
use crate::router::HttpState;

const WORK_DEPENDENCY: [Dependency; 1] = [Dependency::new("event-driver/work-items")];
const QUEUE_DEPENDENCY: [Dependency; 1] = [Dependency::new("event-driver/queue-mutations")];
const PLAN_DEPENDENCY: [Dependency; 1] = [Dependency::new("event-driver/plan-updates")];
const ANNOTATION_DEPENDENCY: [Dependency; 1] = [Dependency::new("event-driver/annotations")];
const ORGANIZATION_DEPENDENCY: [Dependency; 1] =
    [Dependency::new("workspace-state/organization-effects")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/work", OperationStatus::Implemented, work),
        post(
            "/api/work",
            OperationStatus::partial(&WORK_DEPENDENCY),
            work_action,
        ),
        get("/api/queue", OperationStatus::Implemented, queue),
        post(
            "/api/queue",
            OperationStatus::partial(&QUEUE_DEPENDENCY),
            queue_action,
        ),
        get("/api/plan", OperationStatus::Implemented, plan),
        post(
            "/api/plan",
            OperationStatus::partial(&PLAN_DEPENDENCY),
            save_plan,
        ),
        post(
            "/api/annotation",
            OperationStatus::partial(&ANNOTATION_DEPENDENCY),
            annotation,
        ),
        post(
            "/api/organization",
            OperationStatus::partial(&ORGANIZATION_DEPENDENCY),
            organization,
        ),
    ])
}

pub(crate) async fn work(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let agent_id = query_string(&query, "agent")?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let agent = load_agent(
            AgentRepository::get(&connection, &AgentId::from(agent_id.as_str()))
                .map_err(|_| invalid_agent(&agent_id))?,
            &agent_id,
        )?;
        let records = WorkItemsRepository::work(&connection).map_err(|_| ApiError::internal())?;
        let result = work_policy::work_list(&agent.root_id.0, &records).map_err(stored_error)?;
        let result = validate_work_items(result)?;
        generated_json::<crate::generated::WorkList>(result).map_err(|_| {
            stored_error(WorkPolicyError::InvalidStoredObject {
                object: "runtime_work",
                id: agent_id,
            })
        })
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

pub(crate) async fn queue(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let agent_id = query_string(&query, "agent")?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let agent = load_agent(
            AgentRepository::get(&connection, &AgentId::from(agent_id.as_str()))
                .map_err(|_| invalid_agent(&agent_id))?,
            &agent_id,
        )?;
        let events = WorkItemsRepository::queue(&connection, &agent_id, agent.epoch)
            .map_err(|_| ApiError::internal())?;
        let asset_ids = work_policy::queue_asset_ids(&events).map_err(stored_error)?;
        let assets = WorkItemsRepository::assets(&connection, &asset_ids)
            .map_err(|_| ApiError::internal())?;
        let items = work_policy::queue_items(&events, &assets).map_err(stored_error)?;
        let items = validate_queue_assets(items)?;
        let revision = agent
            .extra
            .get("queueMutationRevision")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let digest = queue_revision(agent.epoch, revision, &items)?;
        generated_json::<crate::generated::QueueView>(json!({
            "items": items,
            "revision": digest,
            "capabilities": {"reorder": true, "receipts": true},
        }))
        .map_err(|_| {
            stored_error(WorkPolicyError::InvalidStoredObject {
                object: "runtime_events",
                id: agent_id,
            })
        })
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

pub(crate) async fn plan(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let agent_id = query_string(&query, "agent")?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let agent = load_agent(
            AgentRepository::get(&connection, &AgentId::from(agent_id.as_str()))
                .map_err(|_| invalid_agent(&agent_id))?,
            &agent_id,
        )?;
        let stored =
            WorkItemsRepository::plan(&connection, &agent_id).map_err(|_| ApiError::internal())?;
        let result = work_policy::plan_view(&agent_id, &agent.root_id.0, stored.as_deref())
            .map_err(stored_error)?;
        generated_json::<crate::generated::PlanView>(result).map_err(|_| {
            stored_error(WorkPolicyError::InvalidStoredObject {
                object: "runtime_plans",
                id: agent_id,
            })
        })
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

pub(crate) async fn work_action(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(dependency_error("POST", "/api/work", &WORK_DEPENDENCY))
}

pub(crate) async fn queue_action(ApiJson(body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    validate_queue_action(&body)?;
    Err(dependency_error("POST", "/api/queue", &QUEUE_DEPENDENCY))
}

pub(crate) async fn save_plan(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(dependency_error("POST", "/api/plan", &PLAN_DEPENDENCY))
}

pub(crate) async fn annotation(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "POST",
        "/api/annotation",
        &ANNOTATION_DEPENDENCY,
    ))
}

pub(crate) async fn organization(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "POST",
        "/api/organization",
        &ORGANIZATION_DEPENDENCY,
    ))
}

fn query_string(query: &QueryParams, name: &str) -> Result<String, ApiError> {
    query
        .typed_value(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request"))
}

fn load_agent(
    agent: Option<studio_runtime_store::AgentRecord>,
    id: &str,
) -> Result<studio_runtime_store::AgentRecord, ApiError> {
    let agent = agent.ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request"))?;
    if agent.id.0 != id {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object runtime_agents/{id}"),
        ));
    }
    Ok(agent)
}

fn invalid_agent(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object runtime_agents/{id}"),
    )
}

fn validate_work_items(mut result: Value) -> Result<Value, ApiError> {
    let items = result
        .get_mut("items")
        .and_then(Value::as_array_mut)
        .ok_or_else(ApiError::internal)?;
    for item in items {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<record>")
            .to_owned();
        *item = generated_json::<crate::generated::WorkItem>(item.clone()).map_err(|_| {
            stored_error(WorkPolicyError::InvalidStoredObject {
                object: "runtime_work",
                id,
            })
        })?;
    }
    let items = result
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(ApiError::internal)?;
    result["tasks"] = Value::Array(items);
    Ok(result)
}

fn validate_queue_assets(mut items: Vec<Value>) -> Result<Vec<Value>, ApiError> {
    for item in &mut items {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<event>")
            .to_owned();
        let assets = item
            .get_mut("assets")
            .and_then(Value::as_array_mut)
            .ok_or_else(ApiError::internal)?;
        for asset in assets {
            let asset_id = asset
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("<asset>")
                .to_owned();
            *asset = generated_json::<crate::generated::TranscriptAsset>(asset.clone()).map_err(
                |_| {
                    stored_error(WorkPolicyError::InvalidStoredObject {
                        object: "runtime_assets",
                        id: asset_id,
                    })
                },
            )?;
        }
        let event = generated_json::<crate::generated::QueueItem>(item.clone()).map_err(|_| {
            stored_error(WorkPolicyError::InvalidStoredObject {
                object: "runtime_events",
                id,
            })
        })?;
        *item = event;
    }
    Ok(items)
}

fn queue_revision(epoch: i64, revision: i64, items: &[Value]) -> Result<String, ApiError> {
    let value = json!([epoch, revision, items]);
    let encoded = python_json(&value)?;
    Ok(Sha256::digest(encoded.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn python_json(value: &Value) -> Result<String, ApiError> {
    match value {
        Value::Null => Ok("null".to_owned()),
        Value::Bool(value) => Ok(value.to_string()),
        Value::Number(value) => Ok(value.to_string()),
        Value::String(value) => python_json_string(value),
        Value::Array(values) => Ok(format!(
            "[{}]",
            values
                .iter()
                .map(python_json)
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        )),
        Value::Object(values) => {
            let entries = values
                .iter()
                .map(|(key, value)| {
                    Ok(format!(
                        "{}: {}",
                        python_json_string(key)?,
                        python_json(value)?
                    ))
                })
                .collect::<Result<Vec<_>, ApiError>>()?;
            Ok(format!("{{{}}}", entries.join(", ")))
        }
    }
}

fn python_json_string(value: &str) -> Result<String, ApiError> {
    let mut encoded = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => encoded.push_str("\\\""),
            '\\' => encoded.push_str("\\\\"),
            '\u{8}' => encoded.push_str("\\b"),
            '\u{c}' => encoded.push_str("\\f"),
            '\n' => encoded.push_str("\\n"),
            '\r' => encoded.push_str("\\r"),
            '\t' => encoded.push_str("\\t"),
            character if character < ' ' => {
                write!(&mut encoded, "\\u{:04x}", character as u32)
                    .map_err(|_| ApiError::internal())?;
            }
            character if character.is_ascii() => encoded.push(character),
            character if (character as u32) <= 0xffff => {
                write!(&mut encoded, "\\u{:04x}", character as u32)
                    .map_err(|_| ApiError::internal())?;
            }
            character => {
                let scalar = character as u32 - 0x1_0000;
                let high = 0xd800 + (scalar >> 10);
                let low = 0xdc00 + (scalar & 0x3ff);
                write!(&mut encoded, "\\u{high:04x}\\u{low:04x}")
                    .map_err(|_| ApiError::internal())?;
            }
        }
    }
    encoded.push('"');
    Ok(encoded)
}

fn generated_json<T>(value: Value) -> Result<Value, ApiError>
where
    T: DeserializeOwned + Serialize,
{
    let dto = serde_json::from_value::<T>(value).map_err(|_| ApiError::internal())?;
    serde_json::to_value(dto).map_err(|_| ApiError::internal())
}

fn stored_error(error: WorkPolicyError) -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}

fn validate_queue_action(body: &Value) -> Result<(), ApiError> {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let has_id = ["id", "message_id"].iter().any(|field| {
        body.get(*field)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    });
    if matches!(action, "cancel" | "edit" | "first" | "send_now") && !has_id {
        return Err(ApiError::validation(vec![Value::String("body".to_owned())]));
    }
    if action == "send_now"
        && !body
            .get("request_id")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    {
        return Err(ApiError::validation(vec![Value::String("body".to_owned())]));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::queue_revision;
    use serde_json::json;

    #[test]
    fn queue_revision_uses_python_canonical_json_ordering() {
        let items = [json!({
            "id":"event-1", "kind":"user", "text":"hello", "status":"pending",
            "created":1.0, "assets":[], "delivery":"queue", "requestedDelivery":"queue",
            "error":null,
        })];
        assert_eq!(
            queue_revision(2, 0, &items).expect("queue revision hashes"),
            "1a89b77c4b5d8aa6865aea65f8a3dc013026b0652980402d6c0f2ffa3bd17d95"
        );
    }
}
