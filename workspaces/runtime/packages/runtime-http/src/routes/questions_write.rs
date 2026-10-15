//! HTTP adapters for question delivery and durable complaint writes.

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, post};
use axum::Json;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use studio_runtime_services::questions_write::{self, ComplaintResponseWrite, QuestionsWriteError};
use studio_runtime_store::{RuntimeStore, Transaction};

use crate::error::ApiError;
use crate::router::HttpState;

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post("/api/answer", OperationStatus::Implemented, answer),
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
    State(state): State<HttpState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    monitor: Option<
        axum::Extension<Arc<studio_runtime_services::monitor_runtime_wiring::MonitorRuntime>>,
    >,
    ApiJson(body): ApiJson<crate::generated::AnswerBody>,
) -> Result<Json<crate::generated::AnswerResult>, ApiError> {
    let id = body.id.clone();
    let data = serde_json::to_value(&body).map_err(|_| ApiError::internal())?;
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let async_store = store.clone();
    let browser_store = store.clone();
    let browser_id = id.clone();
    let browser_data = data.clone();
    let validators = state.validators.clone();
    let local_window = super::browser::local_window(&headers, peer);
    let vm_state_dir = state.vm_state_dir.clone();
    let browser_guest = state.browser_answer_guest.clone();
    let browser_host_guest = state.browser_answer_host_guest.clone();
    let browser_local_forwarder = state.browser_local_forwarder.clone();
    let browser_host_local_forwarder = state.browser_host_local_forwarder.clone();
    if let Some(answer) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/questions_write",
        move || {
            let host_source = studio_runtime_services::browser_answer::is_host_source(
                &browser_store,
                &browser_id,
            )?;
            let (guest, forwarder): (
                Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc>,
                Option<Arc<studio_runtime_services::browser::BrowserForwarder>>,
            ) = if host_source {
                (
                    browser_host_guest.unwrap_or_else(|| {
                        Arc::new(studio_runtime_services::linux_vm_guest::UnixSocketGuestRpc::new(
                            browser_store.root()
                                .join(crate::router::BROWSER_HOST_FOLDER_NAME)
                                .join(crate::router::BROWSER_HOST_CONTROL_SOCKET_NAME),
                        ))
                    }),
                    browser_host_local_forwarder,
                )
            } else {
                (
                    browser_guest.unwrap_or_else(|| {
                        Arc::new(
                            studio_runtime_services::linux_vm_guest::UnixGuestRpc::for_runtime_state(
                                &vm_state_dir,
                            ),
                        )
                    }),
                    browser_local_forwarder,
                )
            };
            studio_runtime_services::browser_answer::answer(
                &browser_store,
                &browser_id,
                &browser_data,
                local_window,
                guest.as_ref(),
                forwarder.as_deref(),
                callback_clock().map_err(|_| {
                    studio_runtime_services::browser_answer::BrowserAnswerError::Invalid(
                        "The system clock is unavailable",
                    )
                })?,
                |record| validators.validate_table_record("runtime_requests", record),
            )
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(browser_answer_error)?
    {
        state.event_wake.notify_one();
        return Ok(Json(
            super::generated_json::<crate::generated::AnswerResult>(answer.response)?,
        ));
    }
    let id_for_async = id.clone();
    let data_for_async = data.clone();
    let validators = state.validators.clone();
    if let Some(answer) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/questions_write",
        move || {
            questions_write::answer_async_question_in_store(
                &async_store,
                &id_for_async,
                &data_for_async,
                callback_clock().map_err(|_| {
                    questions_write::QuestionsWriteError::InvalidRequest(
                        "The system clock is unavailable",
                    )
                })?,
                |record| validators.validate_table_record("runtime_requests", record),
            )
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(async_question_error)?
    {
        state.event_wake.notify_one();
        return Ok(Json(
            super::generated_json::<crate::generated::AnswerResult>(answer.response)?,
        ));
    }
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

fn browser_answer_error(
    error: studio_runtime_services::browser_answer::BrowserAnswerError,
) -> ApiError {
    use studio_runtime_services::browser_answer::BrowserAnswerError as E;
    match error {
        E::Invalid(message) | E::Conflict(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        E::Stale => ApiError::new(
            StatusCode::BAD_REQUEST,
            "The sign-in command already stopped. Ask the agent to run it again.",
        ),
        E::Unknown => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Studio could not confirm the browser answer. Retry the same request to inspect its receipt.",
        ),
        E::Guest(error)
            if error.code().is_some_and(|code| {
                matches!(code, "stale" | "limit" | "invalid_params" | "not_found")
            }) =>
        {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
        E::Guest(_) => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Studio could not reach the browser bridge. Retry the same answer.",
        ),
        E::Forward(error) => super::browser::map_browser_error(error),
        E::Store(error) => ApiError::stored(error),
        E::Sqlite(_) => ApiError::internal(),
    }
}

fn async_question_error(error: QuestionsWriteError) -> ApiError {
    use QuestionsWriteError as E;
    match error {
        E::InvalidRequest(message) | E::Conflict(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        E::InvalidStoredObject { object, id } => ApiError::invalid_stored(object, &id),
        E::Store(error) => ApiError::stored(error),
        E::Json(_) | E::Clock => ApiError::internal(),
        E::Coordination(error) => match error {
            studio_runtime_services::coordination::CoordinationError::RequiresDependency(name) => {
                ApiError::dependency(name)
            }
            studio_runtime_services::coordination::CoordinationError::Store(error) => {
                ApiError::stored(error)
            }
            studio_runtime_services::coordination::CoordinationError::InvalidStored {
                object,
                id,
            } => ApiError::invalid_stored(object, &id),
            other => ApiError::new(StatusCode::BAD_REQUEST, other.to_string()),
        },
        E::Projects(error) => match error {
            studio_runtime_services::projects::ProjectsError::Requires(name) => {
                ApiError::dependency(name)
            }
            studio_runtime_services::projects::ProjectsError::Store(error) => {
                ApiError::stored(error)
            }
            other => ApiError::new(StatusCode::BAD_REQUEST, other.to_string()),
        },
        E::QuestionAnswerUnavailable | E::Event(_) => ApiError::internal(),
    }
}

fn answer_unavailable() -> ApiError {
    ApiError::dependency("provider-session/question-answer")
}

fn answer_callback_error(
    error: studio_runtime_services::provider_callbacks::CallbackError,
) -> ApiError {
    match error {
        studio_runtime_services::provider_callbacks::CallbackError::Dependency(name) => {
            ApiError::dependency(name)
        }
        other => callback_error(other),
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
        E::InvalidStored { id, .. } => ApiError::invalid_stored("monitors", &id),
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
        E::Stored(location) => ApiError::invalid_stored("provider_callbacks", &location),
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
        QuestionsWriteError::InvalidStoredObject { object, id } => {
            ApiError::invalid_stored(object, &id)
        }
        QuestionsWriteError::Store(studio_runtime_store::StoreError::InvalidStoredObject {
            table,
            id,
        }) => ApiError::invalid_stored(table, &id),
        QuestionsWriteError::Event(error) => super::work_items::write_error(error),
        QuestionsWriteError::Store(_)
        | QuestionsWriteError::Json(_)
        | QuestionsWriteError::Clock => ApiError::internal(),
        QuestionsWriteError::Coordination(_) | QuestionsWriteError::Projects(_) => {
            ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
        }
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "questions_write",
        routes: Some(routes),
        dependencies: &[
            "callback-runtime/reply-port",
            "provider-session/question-answer",
            "question-answer/monitor-approval",
            "cross-server/remote-events",
        ],
    }
}
