//! HTTP handlers for persisted work-item, plan and queue reads.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use studio_runtime_services::event_write::{self, AnnotationWrite, EventWriteError, PlanWrite};
use studio_runtime_services::work_policy::{self, WorkPolicyError};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, Connection, StoredRecord, WorkItemsRepository,
};

use crate::error::ApiError;
use crate::router::HttpState;
use crate::validation::Registry;

const QUEUE_DEPENDENCY: [Dependency; 1] = [Dependency::new("event-driver/queue-mutations")];
const ORGANIZATION_DEPENDENCY: [Dependency; 1] =
    [Dependency::new("workspace-state/organization-effects")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/work", OperationStatus::Implemented, work),
        get("/api/queue", OperationStatus::Implemented, queue),
        post(
            "/api/queue",
            OperationStatus::partial(&QUEUE_DEPENDENCY),
            queue_action,
        ),
        get("/api/plan", OperationStatus::Implemented, plan),
        post("/api/plan", OperationStatus::Implemented, save_plan),
        post("/api/annotation", OperationStatus::Implemented, annotation),
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
    work_for_agent(state, agent_id).await.map(Json)
}

pub(crate) async fn work_for_agent(state: HttpState, agent_id: String) -> Result<Value, ApiError> {
    read_agent(state, agent_id, |connection, agent, validators| {
        let records = WorkItemsRepository::work(connection).map_err(|_| ApiError::internal())?;
        let records = stored_values("runtime_work", "WorkItem", records, validators, false)?;
        Ok(work_policy::work_list(&agent.root_id.0, &records))
    })
    .await
}

pub(crate) async fn queue(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let agent_id = query_string(&query, "agent")?;
    read_agent(state, agent_id, |connection, agent, validators| {
        let prepared = work_policy::prepare_queue(
            WorkItemsRepository::queue(connection, &agent.id.0, agent.epoch).map_err(|_| ApiError::internal())?,
        ).map_err(stored_error)?;
        let assets = WorkItemsRepository::assets(connection, &prepared.asset_ids).map_err(|_| ApiError::internal())?;
        let assets_present = assets.is_some();
        let assets = stored_values("runtime_assets", "TranscriptAsset", assets.unwrap_or_default(), validators, true)?;
        let owners = assets.iter().map(|asset| asset["agent"].as_str().expect("schema-validated asset owner")).collect::<BTreeSet<_>>();
        for owner in owners {
            let record = AgentRepository::get(connection, &AgentId::from(owner)).map_err(|_| invalid_stored("runtime_agents", owner))?;
            if !record.as_ref().is_some_and(is_live) {
                return Err(invalid_stored("runtime_agents", owner));
            }
        }
        let items = work_policy::queue_items(prepared, &assets, assets_present).map_err(stored_error)?;
        for item in &items {
            if !validators.validate_stored("QueueItem", item) {
                return Err(invalid_stored("runtime_events", item["id"].as_str().unwrap_or("<event>")));
            }
        }
        let revision = queue_revision(agent, &items)?;
        Ok(json!({ "items": items, "revision": revision, "capabilities": {"reorder": true, "receipts": true} }))
    }).await.map(Json)
}

pub(crate) async fn plan(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let agent_id = query_string(&query, "agent")?;
    read_agent(state, agent_id, |connection, agent, validators| {
        let stored =
            WorkItemsRepository::plan(connection, &agent.id.0).map_err(|_| ApiError::internal())?;
        let record = match stored {
            None => None,
            Some(row) => {
                if row.id != agent.id.0 {
                    return Err(invalid_stored("runtime_plans", &row.id));
                }
                let value = record_value("runtime_plans", &row)?;
                if !validators.validate_stored("PlanView", &value) {
                    return Err(invalid_stored("runtime_plans", &row.id));
                }
                Some(value)
            }
        };
        Ok(work_policy::plan_view(
            &agent.id.0,
            &agent.root_id.0,
            record,
        ))
    })
    .await
    .map(Json)
}

async fn read_agent<T, F>(state: HttpState, id: String, read: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Connection, &AgentRecord, &Registry) -> Result<T, ApiError> + Send + 'static,
{
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let agent = AgentRepository::get(&connection, &AgentId::from(id.as_str()))
            .map_err(|_| invalid_stored("runtime_agents", &id))?;
        let agent =
            agent.ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request"))?;
        if agent.id.0 != id {
            return Err(invalid_stored("runtime_agents", &id));
        }
        if !is_live(&agent) {
            return Err(ApiError::new(StatusCode::BAD_REQUEST, "Invalid request"));
        }
        read(&connection, &agent, &validators)
    })
    .await
    .map_err(|_| ApiError::internal())?
}

fn is_live(agent: &AgentRecord) -> bool {
    !agent
        .extra
        .get("deletedAt")
        .is_some_and(|value| match value {
            Value::Null => false,
            Value::Bool(value) => *value,
            Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
            Value::String(value) => !value.is_empty(),
            _ => true,
        })
}

fn stored_values(
    object: &'static str,
    schema: &str,
    records: Vec<StoredRecord>,
    validators: &Registry,
    remove_path: bool,
) -> Result<Vec<Value>, ApiError> {
    records
        .into_iter()
        .map(|row| {
            let mut value = record_value(object, &row)?;
            if value.get("id").and_then(Value::as_str) != Some(row.id.as_str()) {
                return Err(invalid_stored(object, &row.id));
            }
            if remove_path {
                value
                    .as_object_mut()
                    .expect("record is object")
                    .remove("path");
            }
            if !validators.validate_stored(schema, &value) {
                return Err(invalid_stored(object, &row.id));
            }
            Ok(value)
        })
        .collect()
}

fn record_value(object: &'static str, row: &StoredRecord) -> Result<Value, ApiError> {
    serde_json::from_str(
        row.record
            .as_deref()
            .ok_or_else(|| invalid_stored(object, &row.id))?,
    )
    .map_err(|_| invalid_stored(object, &row.id))
}

fn queue_revision(agent: &AgentRecord, items: &[Value]) -> Result<String, ApiError> {
    let revision = match agent.extra.get("queueMutationRevision") {
        None => 0,
        Some(value) => value
            .as_i64()
            .filter(|revision| *revision >= 0)
            .ok_or_else(|| invalid_stored("runtime_agents", &agent.id.0))?,
    };
    let bytes = serde_json::to_vec(&json!([agent.epoch, revision, items]))
        .map_err(|_| ApiError::internal())?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn query_string(query: &QueryParams, name: &str) -> Result<String, ApiError> {
    query
        .typed_value(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request"))
}

fn invalid_stored(object: &'static str, id: &str) -> ApiError {
    stored_error(WorkPolicyError::InvalidStoredObject {
        object,
        id: id.to_owned(),
    })
}

fn stored_error(error: WorkPolicyError) -> ApiError {
    ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string())
}

pub(crate) async fn queue_action(ApiJson(body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    validate_queue_action(&body)?;
    Err(dependency_error("POST", "/api/queue", &QUEUE_DEPENDENCY))
}
pub(crate) async fn save_plan(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let agent_id = optional_string(&body, "agent")
        .map(str::to_owned)
        .unwrap_or_else(|| state.workspace_id.clone());
    let expected_version = body
        .get("version")
        .and_then(Value::as_i64)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request"))?;
    let text = required_string(&body, "text")?.to_owned();
    let signature = request_signature(&body)?;
    let input = PlanWrite {
        agent_id,
        request_id: optional_string(&body, "id").map(str::to_owned),
        expected_version,
        text,
        signature,
    };
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let wake = state.event_wake.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = store
            .transaction_with_error(|connection| {
                event_write::save_plan_validated(connection, input, |record| {
                    validators.validate_stored("PlanView", record)
                })
            })
            .map_err(write_error)?;
        if result.event_inserted {
            wake.notify_one();
        }
        Ok(result.record)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

pub(crate) async fn annotation(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let agent_id = optional_string(&body, "agent")
        .map(str::to_owned)
        .unwrap_or_else(|| state.workspace_id.clone());
    let line = body
        .get("line")
        .and_then(Value::as_i64)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request"))?;
    let signature = python_operation_signature(&body)?;
    let input = AnnotationWrite {
        agent_id,
        id: optional_string(&body, "id").map(str::to_owned),
        path: required_string(&body, "path")?.to_owned(),
        line,
        text: required_string(&body, "text")?.to_owned(),
        turn_id: optional_string(&body, "turnId").map(str::to_owned),
        signature,
    };
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let wake = state.event_wake.clone();
    let result = tokio::task::spawn_blocking(move || {
        let result = store
            .transaction_with_error(|connection| {
                event_write::annotate_validated(connection, input, |record| {
                    validators.validate_stored("AnnotationReceipt", record)
                })
            })
            .map_err(write_error)?;
        if result.event_inserted {
            wake.notify_one();
        }
        Ok(result.record)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}
pub(crate) async fn organization(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(dependency_error(
        "POST",
        "/api/organization",
        &ORGANIZATION_DEPENDENCY,
    ))
}

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}

fn optional_string<'a>(body: &'a Value, field: &str) -> Option<&'a str> {
    body.get(field).and_then(Value::as_str)
}

fn required_string<'a>(body: &'a Value, field: &str) -> Result<&'a str, ApiError> {
    optional_string(body, field)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request"))
}

fn request_signature(body: &Value) -> Result<String, ApiError> {
    use sha2::{Digest, Sha256};

    let bytes = serde_json::to_vec(body).map_err(|_| ApiError::internal())?;
    let digest = Sha256::digest(bytes);
    let mut signature = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut signature, "{byte:02x}").map_err(|_| ApiError::internal())?;
    }
    Ok(signature)
}

pub(crate) fn work_request_signature(agent: &str, body: &Value) -> Result<String, ApiError> {
    use sha2::{Digest, Sha256};
    let value = json!({"agent": agent, "actor": null, "body": body});
    let canonical = python_json(&value);
    let digest = Sha256::digest(canonical.as_bytes());
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn python_operation_signature(body: &Value) -> Result<String, ApiError> {
    use sha2::{Digest, Sha256};
    let mut value = body.clone();
    value
        .as_object_mut()
        .ok_or_else(ApiError::internal)?
        .entry("agent")
        .or_insert(Value::Null);
    let digest = Sha256::digest(python_json(&value).as_bytes());
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn python_json(value: &Value) -> String {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => value.to_string(),
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
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c < ' ' => out.push_str(&format!("\\u{:04x}", c as u32)),
            c if c as u32 > 0x7f => {
                let n = c as u32;
                if n <= 0xffff {
                    out.push_str(&format!("\\u{n:04x}"));
                } else {
                    let n = n - 0x10000;
                    out.push_str(&format!(
                        "\\u{:04x}\\u{:04x}",
                        0xd800 + (n >> 10),
                        0xdc00 + (n & 0x3ff)
                    ));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub(crate) fn write_error(error: EventWriteError) -> ApiError {
    match error {
        EventWriteError::RuleDriverUnavailable => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires rule-driver/event-rules",
        ),
        EventWriteError::RemoteEventsUnavailable => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires cross-server/remote-events",
        ),
        EventWriteError::WorkerAssignmentUnavailable => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires worker-driver/work-assignment",
        ),
        EventWriteError::InvalidRequest(message) | EventWriteError::Conflict(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        EventWriteError::InvalidStoredObject { object, id } => invalid_stored(object, &id),
        EventWriteError::Store(_) | EventWriteError::Clock => ApiError::internal(),
    }
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
        return Err(ApiError::validation(vec![Value::String("body".into())]));
    }
    if action == "send_now"
        && !body
            .get("request_id")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty())
    {
        return Err(ApiError::validation(vec![Value::String("body".into())]));
    }
    Ok(())
}
