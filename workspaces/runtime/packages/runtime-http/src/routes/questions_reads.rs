//! HTTP adapters for persisted questions, complaints, and checkpoint reads.

use super::extractors::QueryParams;
use super::manifest::{OperationStatus, RouteGroup, get};
use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value};
use studio_runtime_services::questions_reads::{self, QuestionReadRecord, QuestionsReadsError};
use studio_runtime_store::QuestionsReadsRepository;

use crate::error::ApiError;
use crate::router::HttpState;
use crate::validation::Registry;

type ValidatedRecord = (String, Map<String, Value>);

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get("/api/questions", OperationStatus::Implemented, questions),
        get("/api/complaint", OperationStatus::Implemented, complaint),
        get(
            "/api/checkpoints",
            OperationStatus::Implemented,
            checkpoints,
        ),
    ])
}

pub(crate) async fn questions(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let agent_id = query
        .typed_value("agent")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let validators = state.validators.clone();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || {
        let (agents, records) = store
            .read_transaction(|transaction| {
                Ok((
                    QuestionsReadsRepository::agents(transaction)?,
                    QuestionsReadsRepository::questions(transaction)?,
                ))
            })
            .map_err(|_| ApiError::internal())?;
        let agents = validate_records(&validators, agents, "AgentRecordResponse", "agents")?;
        let requests = validate_question_records(&validators, records)?;
        let value = questions_reads::question_history(agent_id.as_deref(), agents, requests)
            .map_err(questions_error)?;
        Ok::<_, ApiError>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

pub(crate) async fn complaint(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let id = query
        .typed_value("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let validators = state.validators.clone();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let stored = QuestionsReadsRepository::complaint(&connection, &id)
            .map_err(|_| ApiError::internal())?;
        let stored = stored
            .map(|record| {
                validate_record(&validators, record, "ComplaintRecordResponse", "complaints")
            })
            .transpose()?;
        questions_reads::complaint_detail(stored).map_err(questions_error)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(value))
}

pub(crate) async fn checkpoints(
    query: QueryParams,
    State(state): State<HttpState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let agent_id = query
        .typed_value("agent")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let validators = state.validators.clone();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || {
        let (agents, records) = store
            .read_transaction(|transaction| {
                Ok((
                    QuestionsReadsRepository::agents(transaction)?,
                    QuestionsReadsRepository::checkpoints(transaction)?,
                ))
            })
            .map_err(|_| ApiError::internal())?;
        let agents = validate_records(&validators, agents, "AgentRecordResponse", "agents")?;
        let checkpoints =
            validate_records(&validators, records, "CheckpointSummary", "checkpoints")?;
        let value = questions_reads::checkpoint_list(agent_id.as_deref(), agents, checkpoints)
            .map_err(questions_error)?;
        Ok::<_, ApiError>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    checkpoint_response(value, headers)
}

fn checkpoint_response(value: Value, headers: HeaderMap) -> Result<Response, ApiError> {
    let body = serde_json::to_vec(&value).map_err(|_| ApiError::internal())?;
    let etag = format!("\"{}\"", studio_runtime_store::sha256_hex(&body));
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(',').any(|candidate| {
                let candidate = candidate
                    .trim()
                    .strip_prefix("W/")
                    .unwrap_or(candidate.trim());
                candidate == "*" || candidate == etag
            })
        })
    {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        response.headers_mut().insert(
            header::ETAG,
            HeaderValue::from_str(&etag).map_err(|_| ApiError::internal())?,
        );
        return Ok(response);
    }
    let mut response = Json(value).into_response();
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag).map_err(|_| ApiError::internal())?,
    );
    Ok(response)
}

fn validate_records(
    validators: &Registry,
    records: Vec<studio_runtime_store::QuestionsReadsRecord>,
    schema: &str,
    object: &'static str,
) -> Result<Vec<ValidatedRecord>, ApiError> {
    records
        .into_iter()
        .map(|record| validate_record(validators, record, schema, object))
        .collect()
}

fn validate_question_records(
    validators: &Registry,
    records: Vec<studio_runtime_store::QuestionsReadsRecord>,
) -> Result<Vec<QuestionReadRecord>, ApiError> {
    records
        .into_iter()
        .map(|record| {
            let value = validators
                .validated_stored("RequestRecordResponse", &record.record)
                .and_then(|value| value.as_object().cloned())
                .ok_or_else(|| invalid_stored_object("requests", &record.id))?;
            Ok(QuestionReadRecord {
                id: record.id,
                raw: record.record,
                value,
            })
        })
        .collect()
}

fn validate_record(
    validators: &Registry,
    record: studio_runtime_store::QuestionsReadsRecord,
    schema: &str,
    object: &'static str,
) -> Result<(String, Map<String, Value>), ApiError> {
    let value = validate_stored_value(validators, &record.id, &record.record, schema, object)?;
    Ok((record.id, value))
}

pub(super) fn validate_stored_value(
    validators: &Registry,
    id: &str,
    raw: &str,
    schema: &str,
    object: &'static str,
) -> Result<Map<String, Value>, ApiError> {
    let value = validators
        .validated_stored(schema, raw)
        .and_then(|value| value.as_object().cloned())
        .ok_or_else(|| invalid_stored_object(object, id))?;
    if value.get("id").and_then(Value::as_str) != Some(id) {
        return Err(invalid_stored_object(object, id));
    }
    Ok(value)
}

fn questions_error(error: QuestionsReadsError) -> ApiError {
    match error {
        QuestionsReadsError::ConversationRequired => {
            ApiError::new(StatusCode::BAD_REQUEST, "A conversation is required")
        }
        QuestionsReadsError::UnknownAgent => {
            ApiError::new(StatusCode::BAD_REQUEST, "Unknown agent")
        }
        QuestionsReadsError::DeletedAgent => {
            ApiError::new(StatusCode::BAD_REQUEST, "This agent was deleted")
        }
        QuestionsReadsError::UnknownComplaint => {
            ApiError::new(StatusCode::BAD_REQUEST, "Unknown complaint")
        }
        QuestionsReadsError::InvalidStoredObject { object, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}/{id}"),
        ),
    }
}

fn invalid_stored_object(object: &'static str, id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object {object}/{id}"),
    )
}

#[cfg(test)]
mod tests {
    use super::checkpoint_response;
    use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
    use serde_json::json;

    #[tokio::test]
    async fn checkpoint_response_supports_entity_tag_revalidation() {
        let first = checkpoint_response(json!({"checkpoints": []}), HeaderMap::new())
            .expect("response builds");
        let etag = first
            .headers()
            .get(header::ETAG)
            .expect("etag is present")
            .clone();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_str(etag.to_str().expect("etag is textual"))
                .expect("etag is a valid header value"),
        );
        let second = checkpoint_response(json!({"checkpoints": []}), headers)
            .expect("conditional response builds");
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
    }
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "questions_reads",
        routes: Some(routes),
        dependencies: &[],
    }
}
