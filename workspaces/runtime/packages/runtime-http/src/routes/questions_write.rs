//! HTTP adapters for question delivery and durable complaint writes.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use studio_runtime_services::questions_write::{self, ComplaintResponseWrite, QuestionsWriteError};
use studio_runtime_store::{RuntimeStore, Transaction};

use crate::error::ApiError;
use crate::router::HttpState;

const ANSWER_DEPENDENCIES: [Dependency; 4] = [
    Dependency::new("provider-session/question-answer"),
    Dependency::new("question-answer/async-question"),
    Dependency::new("question-answer/browser-open"),
    Dependency::new("question-answer/monitor-approval"),
];
const DELETE_DEPENDENCIES: [Dependency; 1] = [Dependency::new("provider-session/question-answer")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/answer",
            OperationStatus::partial(&ANSWER_DEPENDENCIES),
            answer,
        ),
        post("/api/complaints", OperationStatus::Implemented, complaints),
        post(
            "/api/questions/delete",
            OperationStatus::partial(&DELETE_DEPENDENCIES),
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
    State(state): State<HttpState>,
    monitor: Option<
        axum::Extension<Arc<studio_runtime_services::monitor_runtime_wiring::MonitorRuntime>>,
    >,
    ApiJson(body): ApiJson<crate::generated::AnswerBody>,
) -> Result<Json<crate::generated::AnswerResult>, ApiError> {
    let id = body.id.clone();
    let data = serde_json::to_value(&body).map_err(|_| ApiError::internal())?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let monitor_available = monitor.is_some();
    let monitor_id = id.clone();
    let monitor_data = data.clone();
    if let Some(answer) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/questions_write",
        move || {
            studio_runtime_services::monitor_runtime::answer_approval(
                &store,
                &monitor_id,
                &monitor_data,
                callback_clock().map_err(|_| {
                    studio_runtime_services::monitor_policy::MonitorError::Refused(
                        "The system clock is unavailable",
                    )
                })?,
                monitor_available,
            )
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(monitor_approval_error)?
    {
        if answer.accepted {
            monitor
                .as_ref()
                .ok_or_else(|| ApiError::dependency("monitor-driver/start-dispatch"))?
                .0
                .dispatch_wake_handle()
                .notify_one();
        }
        state.event_wake.notify_one();
        return Ok(Json(
            super::generated_json::<crate::generated::AnswerResult>(answer.response)?,
        ));
    }
    // Native callback delivery requires the retained reply runtime. Without
    // it, refuse before parsing callback-only stored context or writing.
    let Some(runtime) = state.callback_runtime.clone() else {
        return Err(answer_unavailable());
    };
    let value = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/questions_write",
        move || runtime.answer(&id, &data, callback_clock()?),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(answer_callback_error)?;
    state.event_wake.notify_one();
    Ok(Json(
        super::generated_json::<crate::generated::AnswerResult>(value)?,
    ))
}

fn answer_unavailable() -> ApiError {
    ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        format!(
            "Not implemented: requires {}",
            ANSWER_DEPENDENCIES
                .iter()
                .map(|dependency| dependency.name())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
}

fn answer_callback_error(
    error: studio_runtime_services::provider_callbacks::CallbackError,
) -> ApiError {
    if matches!(
        error,
        studio_runtime_services::provider_callbacks::CallbackError::Dependency(_)
    ) {
        answer_unavailable()
    } else {
        callback_error(error)
    }
}

fn monitor_approval_error(
    error: studio_runtime_services::monitor_policy::MonitorError,
) -> ApiError {
    use studio_runtime_services::monitor_policy::MonitorError as E;
    match error {
        E::Unavailable(name) => ApiError::dependency(name),
        E::Event(
            studio_runtime_services::event_write::EventWriteError::RemoteEventsUnavailable,
        ) => ApiError::dependency("cross-server/remote-events"),
        E::Refused(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        E::InvalidStored { id, location } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object monitors/{id} at {location}"),
        ),
        _ => ApiError::internal(),
    }
}

fn callback_clock() -> Result<f64, studio_runtime_services::provider_callbacks::CallbackError> {
    studio_runtime_services::event_write::unix_time().map_err(|_| {
        studio_runtime_services::provider_callbacks::CallbackError::Invalid(
            "The system clock is unavailable",
        )
    })
}
fn callback_error(error: studio_runtime_services::provider_callbacks::CallbackError) -> ApiError {
    use studio_runtime_services::provider_callbacks::CallbackError as E;
    match error {
        E::Dependency(name) => ApiError::dependency(name),
        E::Store(error) => ApiError::stored(error),
        E::Stored(location) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored callback at {location}"),
        ),
        E::Invalid(_) | E::Conflict | E::Unknown | E::Stale => {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
    }
}
async fn defer_question(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::QuestionDeferBody>,
) -> Result<Json<crate::generated::MutationReceipt>, ApiError> {
    let id = body.id;
    let deferred = body.deferred;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let result = write(store, move |transaction| {
        questions_write::defer_question(transaction, &id, deferred, |record| {
            validators.validate_table_record("runtime_requests", record)
        })
    })
    .await?;
    Ok(Json(super::generated_json::<
        crate::generated::MutationReceipt,
    >(result)?))
}

async fn delete_question(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::QuestionBody>,
) -> Result<Json<crate::generated::MutationReceipt>, ApiError> {
    let id = body.id;
    let result = if let Some(runtime) = state.callback_runtime.clone() {
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-http/routes/questions_write",
            move || runtime.delete_question(&id, callback_clock()?),
        )
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(callback_error)?
    } else {
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let validators = state.validators.clone();
        write(store, move |transaction| {
            questions_write::delete_question(transaction, &id, |record| {
                validators.validate_table_record("runtime_requests", record)
            })
        })
        .await?
    };
    if state.callback_runtime.is_some() {
        state.event_wake.notify_one();
    }
    Ok(Json(super::generated_json::<
        crate::generated::MutationReceipt,
    >(result)?))
}

async fn complaints(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::ComplaintBody>,
) -> Result<Json<crate::generated::ComplaintRecordResponse>, ApiError> {
    let request_id = body.id.clone();
    let text = body.text.clone();
    let action = body
        .action
        .as_ref()
        .map(ToString::to_string)
        .unwrap_or_else(|| "submit".to_owned());
    let lead_id = body.lead.clone().flatten().unwrap_or_default();
    let complaint_id = body.complaint_id.clone().flatten().unwrap_or_default();
    let status = body
        .status
        .clone()
        .flatten()
        .map(|value| value.to_string())
        .unwrap_or_default();
    let version = body.version.flatten().unwrap_or(-1);
    let validators = state.validators.clone();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let event_inserted = Arc::new(AtomicBool::new(false));
    let inserted = event_inserted.clone();
    let signature = match action.as_str() {
        "submit" => {
            if lead_id.trim().is_empty() {
                return Err(ApiError::validation(vec![Value::String(
                    "body.lead".into(),
                )]));
            }
            String::new()
        }
        "respond" => {
            if body
                .complaint_id
                .as_ref()
                .and_then(Option::as_ref)
                .is_none()
                || body.status.as_ref().and_then(Option::as_ref).is_none()
                || body.version.flatten().is_none()
            {
                return Err(ApiError::validation(vec![Value::String("body".into())]));
            }
            let mut data = serde_json::to_value(&body).map_err(|_| ApiError::internal())?;
            data.as_object_mut()
                .ok_or_else(ApiError::internal)?
                .insert("operation".to_owned(), json!("complaint_response"));
            studio_runtime_services::event_write::request_signature(&data)
        }
        _ => return Err(ApiError::internal()),
    };
    let result = write(store, move |transaction| {
        let result = if action == "submit" {
            questions_write::submit_complaint(transaction, &lead_id, &request_id, &text, |record| {
                validators.validate_table_record("runtime_complaints", record)
            })
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
                |record| validators.validate_table_record("runtime_complaints", record),
            )
        }?;
        inserted.store(result.wake_event_worker, Ordering::Relaxed);
        Ok(result.record)
    })
    .await?;
    if event_inserted.load(Ordering::Relaxed) {
        state.event_wake.notify_one();
    }
    Ok(Json(super::generated_json::<
        crate::generated::ComplaintRecordResponse,
    >(result)?))
}

async fn write(
    store: Arc<RuntimeStore>,
    operation: impl FnOnce(&Transaction<'_>) -> Result<Value, QuestionsWriteError> + Send + 'static,
) -> Result<Value, ApiError> {
    super::sync_delta::write(store, operation, api_error).await
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
        QuestionsWriteError::Store(studio_runtime_store::StoreError::InvalidStoredObject {
            table,
            id,
        }) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {table}/{id}"),
        ),
        QuestionsWriteError::Event(error) => super::work_items::write_error(error),
        QuestionsWriteError::Store(_)
        | QuestionsWriteError::Json(_)
        | QuestionsWriteError::Clock => ApiError::internal(),
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "questions_write",
        routes: Some(routes),
        dependencies: &[
            "callback-runtime/reply-port",
            "provider-session/question-answer",
            "question-answer/async-question",
            "question-answer/browser-open",
            "question-answer/monitor-approval",
            "cross-server/remote-events",
        ],
    }
}
