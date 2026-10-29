//! HTTP handlers for persisted work-item, plan and queue reads.

use super::extractors::{ApiJson, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use studio_runtime_services::event_write::{self, AnnotationWrite, EventWriteError};
use studio_runtime_services::work_policy::{self, WorkPolicyError};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, Connection, WorkItemsRepository,
};

use crate::error::ApiError;
use crate::router::HttpState;
use crate::validation::Registry;

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/work", OperationStatus::Implemented, work),
        get("/api/queue", OperationStatus::Implemented, queue),
        post(
            "/api/queue",
            OperationStatus::Implemented,
            super::queue_write::queue_action,
        ),
        post("/api/annotation", OperationStatus::Implemented, annotation),
        post(
            "/api/organization",
            OperationStatus::Implemented,
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
    read_agent(state, agent_id, |connection, agent, _validators| {
        let records = WorkItemsRepository::work(connection).map_err(ApiError::stored)?;
        let mut list = work_policy::work_list(&agent.root_id.0, &records);
        let lead = AgentRepository::get(connection, &agent.root_id)?
            .ok_or_else(|| invalid_stored("runtime_agents", &agent.root_id.0))?;
        list["rootId"] = json!(agent.root_id.0);
        list["taskDefaultGate"] = lead
            .extra
            .get("taskDefaultGate")
            .cloned()
            .unwrap_or(Value::Null);
        let records_by_id: std::collections::BTreeMap<_, _> = records
            .iter()
            .map(|record| (record.id().0.as_str(), record))
            .collect();
        for item in list["items"].as_array_mut().expect("work list") {
            let id = item["id"].as_str().unwrap_or_default();
            let record = records_by_id
                .get(id)
                .ok_or_else(|| invalid_stored("runtime_work", id))?;
            item["submissionRefusal"] =
                studio_runtime_services::work_write::current_submission_refusal(
                    connection,
                    &record.value(),
                )
                .map_err(super::work_write::write_error)?;
        }
        list["tasks"] = list["items"].clone();
        Ok(list)
    })
    .await
}

pub(crate) async fn queue(
    State(state): State<HttpState>,
    query: QueryParams,
) -> Result<Json<Value>, ApiError> {
    let agent_id = query_string(&query, "agent")?;
    queue_for_agent(state, agent_id).await.map(Json)
}

pub(super) async fn queue_for_agent(state: HttpState, agent_id: String) -> Result<Value, ApiError> {
    read_agent(state, agent_id, |connection, agent, _validators| {
        let snapshot = super::queue_read::queue_snapshot(connection, agent)
            .map_err(super::queue_read::api_error)?;
        Ok(json!({
            "items": snapshot.items,
            "revision": snapshot.revision,
            "capabilities": {
                "reorder": studio_runtime_services::queue_write::QUEUE_REORDER_ENABLED,
                "receipts": studio_runtime_services::queue_write::QUEUE_RECEIPTS_ENABLED,
            },
        }))
    })
    .await
}

async fn read_agent<T, F>(state: HttpState, id: String, read: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&Connection, &AgentRecord, &Registry) -> Result<T, ApiError> + Send + 'static,
{
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    store
        .read_async("runtime-http/routes/work_items", move |connection| {
            let agent = AgentRepository::get(connection, &AgentId::from(id.as_str()))?
                .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request"))?;
            if !super::queue_read::is_live(&agent) {
                return Err(ApiError::new(StatusCode::BAD_REQUEST, "Invalid request"));
            }
            read(connection, &agent, &validators)
        })
        .await
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
    match error {
        WorkPolicyError::InvalidStoredObject { object, id } => {
            ApiError::invalid_stored(object, &id)
        }
    }
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
    let wake = state.event_wake.clone();
    let result = store
        .transaction_with_error(|connection| event_write::annotate(connection, input))
        .await
        .map_err(write_error)?;
    if result.event_inserted {
        wake.notify_one();
    }
    let result = result.record;
    Ok(Json(result))
}
pub(crate) async fn organization(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let result = store
        .transaction_with_error(move |tx| {
            studio_runtime_services::organization_effects::apply(tx, &body)
        })
        .await
        .map_err(write_error)?;
    Ok(Json(result))
}

fn optional_string<'a>(body: &'a Value, field: &str) -> Option<&'a str> {
    body.get(field).and_then(Value::as_str)
}

fn required_string<'a>(body: &'a Value, field: &str) -> Result<&'a str, ApiError> {
    optional_string(body, field)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid request"))
}

pub(crate) fn python_operation_signature(body: &Value) -> Result<String, ApiError> {
    let mut value = body.clone();
    let object = value.as_object_mut().ok_or_else(ApiError::internal)?;
    object.entry("agent").or_insert(Value::Null);
    let canonical = studio_runtime_services::event_write::python_json(&value);
    Ok(studio_runtime_store::sha256_hex(canonical.as_bytes()))
}

pub(crate) fn work_request_signature(
    agent: &str,
    actor: Option<&str>,
    body: &Value,
) -> Result<String, ApiError> {
    let value = json!({"agent": agent, "actor": actor, "body": body});
    let canonical = event_write::python_json(&value);
    Ok(studio_runtime_store::sha256_hex(canonical.as_bytes()))
}

pub(crate) fn write_error(error: EventWriteError) -> ApiError {
    match error {
        EventWriteError::RequestResult(error) => error.into(),
        EventWriteError::InvalidResultSchema(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
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
        EventWriteError::Store(error) => ApiError::stored(error),
        EventWriteError::Clock => ApiError::internal(),
    }
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "work_items",
        routes: Some(routes),
        dependencies: &["event-driver/work-items"],
    }
}
