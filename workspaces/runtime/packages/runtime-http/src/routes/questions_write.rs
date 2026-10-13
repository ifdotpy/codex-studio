//! HTTP adapters for question delivery and durable complaint writes.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_services::questions_write::{self, ComplaintResponseWrite, QuestionsWriteError};
use studio_runtime_store::{EntityRow, RuntimeStore, SyncDocumentsRepository, Transaction};

use crate::error::ApiError;
use crate::router::HttpState;

const QUESTION_ANSWER_DEPENDENCY: [Dependency; 1] =
    [Dependency::new("provider-session/question-answer")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/answer",
            OperationStatus::partial(&QUESTION_ANSWER_DEPENDENCY),
            answer,
        ),
        post("/api/complaints", OperationStatus::Implemented, complaints),
        post(
            "/api/questions/delete",
            OperationStatus::Implemented,
            delete_question,
        ),
        post(
            "/api/questions/defer",
            OperationStatus::Implemented,
            defer_question,
        ),
    ])
}

async fn answer(
    State(_state): State<HttpState>,
    ApiJson(_body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    Err(
        dependency_unavailable("POST", "/api/answer", &QUESTION_ANSWER_DEPENDENCY)
            .map_err(|_| ApiError::internal())?,
    )
}

async fn defer_question(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let id = field_string(&body, "id")?;
    let deferred = body
        .get("deferred")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::validation(vec![Value::String("body".into())]))?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let result = write(store, move |transaction| {
        questions_write::defer_question(transaction, &id, deferred, |record| {
            validators.validate_stored("RequestRecordResponse", record)
        })
    })
    .await?;
    Ok(Json(generated_receipt(result)?))
}

async fn delete_question(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let id = field_string(&body, "id")?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let result = write(store, move |transaction| {
        questions_write::delete_question(transaction, &id, |record| {
            validators.validate_stored("RequestRecordResponse", record)
        })
    })
    .await?;
    Ok(Json(generated_receipt(result)?))
}

async fn complaints(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let request_id = field_string(&body, "id")?;
    let text = field_string(&body, "text")?;
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("submit")
        .to_owned();
    let lead_id = body
        .get("lead")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let complaint_id = body
        .get("complaint_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let status = body
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let version = body.get("version").and_then(Value::as_i64).unwrap_or(-1);
    let validators = state.validators.clone();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let signature = match action.as_str() {
        "submit" => {
            if lead_id.is_empty() {
                return Err(ApiError::validation(vec![Value::String(
                    "body.lead".into(),
                )]));
            }
            questions_write::request_signature(&json!({
                "operation":"complaint_submit", "id":request_id, "lead":lead_id, "text":text
            }))
        }
        "respond" => {
            if body.get("complaint_id").and_then(Value::as_str).is_none()
                || body.get("status").and_then(Value::as_str).is_none()
                || body.get("version").and_then(Value::as_i64).is_none()
            {
                return Err(ApiError::validation(vec![Value::String("body".into())]));
            }
            questions_write::request_signature(&json!({
            "operation":"complaint_response", "id":request_id,
            "complaint_id":body.get("complaint_id"), "text":text,
            "status":body.get("status"), "version":body.get("version")
            }))
        }
        _ => {
            return Err(ApiError::validation(vec![Value::String(
                "body.action".into(),
            )]));
        }
    };
    let result = write(store, move |transaction| {
        if action == "submit" {
            questions_write::submit_complaint(
                transaction,
                &lead_id,
                &request_id,
                &text,
                &signature,
                |record| validators.validate_stored("ComplaintRecordResponse", record),
            )
        } else {
            questions_write::respond_to_complaint(
                transaction,
                ComplaintResponseWrite {
                    complaint_id: &complaint_id,
                    request_id: &request_id,
                    text: &text,
                    status: &status,
                    version,
                    signature: &signature,
                },
                |record| validators.validate_stored("ComplaintRecordResponse", record),
            )
        }
    })
    .await?;
    state.event_wake.notify_one();
    Ok(Json(super::generated_json::<
        crate::generated::ComplaintRecordResponse,
    >(result)?))
}

async fn write(
    store: Arc<RuntimeStore>,
    operation: impl FnOnce(&Transaction<'_>) -> Result<Value, QuestionsWriteError> + Send + 'static,
) -> Result<Value, ApiError> {
    let before = store.read().map_err(|_| ApiError::internal())?;
    let before_sequence =
        SyncDocumentsRepository::entity_sequence(&before).map_err(|_| ApiError::internal())?;
    drop(before);
    tokio::task::spawn_blocking(move || {
        let result = store.transaction_with_error(operation).map_err(api_error)?;
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let entities = SyncDocumentsRepository::entities_after(&connection, before_sequence, 501)
            .map_err(|_| ApiError::internal())?;
        let floor = SyncDocumentsRepository::entity_tombstone_floor(&connection)
            .map_err(|_| ApiError::internal())?;
        let sync_after =
            (entities.len() <= 500 && floor <= before_sequence).then_some(before_sequence);
        Ok(with_sync_entities(result, entities, sync_after))
    })
    .await
    .map_err(|_| ApiError::internal())?
}

fn with_sync_entities(
    mut result: Value,
    entities: Vec<EntityRow>,
    sync_after: Option<i64>,
) -> Value {
    if entities.is_empty() {
        return result;
    }
    result["_syncEntities"] = Value::Array(
        entities
            .iter()
            .take(500)
            .map(|row| {
                json!({
                    "id": format!("entity:{}:{}", row.collection, row.id),
                    "seq": row.sequence,
                    "payload": row.payload,
                    "_deleted": row.deleted,
                })
            })
            .collect(),
    );
    if let Some(sync_after) = sync_after {
        result["_syncEntitiesAfter"] = json!(sync_after);
    }
    result
}

fn field_string(body: &Value, field: &str) -> Result<String, ApiError> {
    body.get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ApiError::validation(vec![Value::String(format!("body.{field}"))]))
}

fn generated_receipt(value: Value) -> Result<Value, ApiError> {
    super::generated_json::<crate::generated::MutationReceipt>(value)
}

fn api_error(error: QuestionsWriteError) -> ApiError {
    match error {
        QuestionsWriteError::InvalidRequest(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        QuestionsWriteError::Conflict(message) => ApiError::new(StatusCode::CONFLICT, message),
        QuestionsWriteError::QuestionAnswerUnavailable => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires provider-session/question-answer",
        ),
        QuestionsWriteError::InvalidStoredObject { object, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}/{id}"),
        ),
        QuestionsWriteError::Event(error) => super::work_items::write_error(error),
        QuestionsWriteError::Store(_)
        | QuestionsWriteError::Json(_)
        | QuestionsWriteError::Clock => ApiError::internal(),
    }
}
