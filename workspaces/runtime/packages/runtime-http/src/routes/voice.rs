//! Persisted voice routes and explicit provider-dependent operations.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post as axum_post;
use serde_json::Value;
use studio_runtime_services::voice::{self, VoiceError};

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, post};
use crate::error::ApiError;
use crate::router::HttpState;

const VOICE_PROVIDER: [Dependency; 1] = [Dependency::new("provider-session/voice")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/voice/approval_speech",
            OperationStatus::Implemented,
            approval_speech,
        ),
        post(
            "/api/voice/approvals",
            OperationStatus::Implemented,
            approvals,
        ),
        post("/api/voice/approve", OperationStatus::Implemented, approve),
        post("/api/voice/audio", OperationStatus::Implemented, audio),
        post("/api/voice/end", OperationStatus::Implemented, end),
        post("/api/voice/record", OperationStatus::Implemented, record),
        post("/api/voice/records", OperationStatus::Implemented, records),
        post("/api/voice/speech", OperationStatus::Implemented, speech),
        post("/api/voice/start", OperationStatus::Implemented, start),
        post("/api/voice/status", OperationStatus::Implemented, status),
        post("/api/voice/submit", OperationStatus::Implemented, submit),
    ])
}

pub(crate) fn fallback() -> Router<HttpState> {
    Router::new().route("/api/voice/{*action}", axum_post(unknown_action))
}

async fn unknown_action(
    State(state): State<HttpState>,
    Path(_action): Path<String>,
    Json(body): Json<Value>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if state.store.is_none() {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "Not found"));
    }
    if !body.is_object() {
        return Err(ApiError::validation(vec![Value::String("body".into())]));
    }
    Err(ApiError::new(
        StatusCode::BAD_REQUEST,
        "Unknown voice action",
    ))
}

async fn status(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceStatusRequest>,
) -> Result<Json<crate::generated::VoiceStatusResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let agent_id = body.agent;
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/voice",
        move || voice::status(&store, &agent_id),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(voice_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceStatusResponse,
    >(result)?))
}

async fn audio(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceAudioRequest>,
) -> Result<Json<crate::generated::VoiceRecordResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let (agent, session, chunk, encoded, mime) = (
        body.agent,
        body.session_id,
        body.chunk_id,
        body.audio,
        body.mime,
    );
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/voice",
        move || voice::save_audio(&store, &agent, &session, &chunk, &encoded, &mime),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(voice_audio_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceRecordResponse,
    >(result)?))
}

async fn record(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceRecordRequest>,
) -> Result<Json<crate::generated::VoiceRecordResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let (kind, agent, session, event, text, item, previous, payload) = (
        match body.kind {
            crate::generated::VoiceRecordKind::User => "user",
            crate::generated::VoiceRecordKind::Courier => "courier",
            crate::generated::VoiceRecordKind::Assistant => "assistant",
            crate::generated::VoiceRecordKind::Event => "event",
            crate::generated::VoiceRecordKind::Playback => "playback",
            crate::generated::VoiceRecordKind::Orchestrator => "orchestrator",
        },
        body.agent,
        body.session_id,
        body.event_id,
        body.text.unwrap_or_default(),
        body.item_id.unwrap_or_default(),
        body.previous_item_id.unwrap_or_default(),
        body.payload.flatten(),
    );
    let payload = payload
        .map(serde_json::to_value)
        .transpose()
        .map_err(|_| ApiError::internal())?;
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/voice",
        move || {
            voice::record(
                &store,
                voice::VoiceRecordInput {
                    agent_id: agent,
                    session_id: session,
                    event_id: event,
                    kind: kind.to_owned(),
                    text,
                    item_id: item,
                    previous_item_id: previous,
                    payload,
                    internal: false,
                },
            )
        },
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(voice_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceRecordResponse,
    >(result)?))
}

async fn records(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceRecordsRequest>,
) -> Result<Json<crate::generated::VoiceRecordsResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let agent_id = body.agent;
    let after = body.after.unwrap_or_default();
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/voice",
        move || voice::records(&store, &agent_id, after),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(voice_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceRecordsResponse,
    >(result)?))
}

async fn approvals(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceApprovalsRequest>,
) -> Result<Json<crate::generated::VoiceApprovalsResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let agent_id = body.agent;
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/voice",
        move || voice::approvals(&store, &agent_id),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(voice_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceApprovalsResponse,
    >(result)?))
}

async fn approval_speech(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceApprovalSpeechRequest>,
) -> Result<Json<crate::generated::VoiceRecordResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let (agent, request_id) = (body.agent, body.request_id);
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/voice",
        move || voice::approval_speech(&store, &agent, &request_id),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(voice_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceRecordResponse,
    >(result)?))
}

async fn end(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceEndRequest>,
) -> Result<Json<crate::generated::VoiceSessionResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let (agent, session) = (body.agent, body.session_id);
    let Some(provider) = state.voice_request_port.clone() else {
        if let Some(result) = voice::provider::read_terminal_session(&store, &agent, &session)
            .map_err(provider_error)?
        {
            return Ok(Json(super::generated_json::<
                crate::generated::VoiceSessionResponse,
            >(result)?));
        }
        return Err(voice_provider_unavailable());
    };
    let now = now_seconds()?;
    let result = voice::provider::end(store, provider, agent, session, now)
        .await
        .map_err(provider_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceSessionResponse,
    >(result)?))
}

async fn speech(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceSpeechRequest>,
) -> Result<Json<crate::generated::VoiceSpeechReceiptResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let (agent, session, record) = (body.agent, body.session_id, body.record_id);
    let Some(provider) = state.voice_request_port.clone() else {
        if let Some(result) =
            voice::provider::read_speech_receipt(&store, &agent, &session, &record)
                .map_err(provider_error)?
        {
            return Ok(Json(super::generated_json::<
                crate::generated::VoiceSpeechReceiptResponse,
            >(result)?));
        }
        return Err(voice_provider_unavailable());
    };
    let result = voice::provider::speech(store, provider, agent, session, record, None)
        .await
        .map_err(provider_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceSpeechReceiptResponse,
    >(result)?))
}

async fn approve(
    State(state): State<HttpState>,
    monitor: Option<
        axum::Extension<
            std::sync::Arc<studio_runtime_services::monitor_runtime_wiring::MonitorRuntime>,
        >,
    >,
    ApiJson(body): ApiJson<crate::generated::VoiceApproveRequest>,
) -> Result<Json<crate::generated::VoiceApprovalResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let (agent_id, speech_id) = (body.agent.clone(), body.speech_id.clone());
    let store_for_lookup = store.clone();
    let monitor_approval = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/voice",
        move || voice::provider::is_monitor_approval(&store_for_lookup, &agent_id, &speech_id),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(provider_error)?;
    if monitor_approval {
        let result = voice::provider::approve_monitor(
            store,
            body.agent,
            body.session_id,
            body.speech_id,
            body.transcript_id,
            now_seconds()?,
            monitor.is_some(),
        )
        .await
        .map_err(provider_error)?;
        if let Some(monitor) = monitor {
            monitor.0.dispatch_wake_handle().notify_one();
        }
        state.event_wake.notify_one();
        return Ok(Json(super::generated_json::<
            crate::generated::VoiceApprovalResponse,
        >(result)?));
    }
    let provider = state
        .voice_request_port
        .clone()
        .ok_or_else(voice_provider_unavailable)?;
    let result = voice::provider::approve(
        store,
        provider,
        body.agent,
        body.session_id,
        body.speech_id,
        body.transcript_id,
        now_seconds()?,
    )
    .await
    .map_err(provider_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceApprovalResponse,
    >(result)?))
}

async fn submit(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceSubmitRequest>,
) -> Result<Json<crate::generated::VoiceDeliveryResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let (agent, message, record_ids, edited) = (
        body.agent,
        body.message_id,
        body.record_ids,
        body.edited_text.flatten(),
    );
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/voice",
        move || voice::submit(&store, &agent, &message, &record_ids, edited.as_deref()),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(voice_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceDeliveryResponse,
    >(result)?))
}

async fn start(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceStartRequest>,
) -> Result<Json<crate::generated::VoiceSessionResponse>, ApiError> {
    let provider = state
        .voice_request_port
        .clone()
        .ok_or_else(voice_provider_unavailable)?;
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = voice::provider::start(
        store,
        provider,
        body.agent,
        body.session_id,
        body.sdp,
        now_seconds()?,
    )
    .await
    .map_err(provider_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceSessionResponse,
    >(result)?))
}

fn now_seconds() -> Result<f64, ApiError> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs_f64())
}

fn voice_provider_unavailable() -> ApiError {
    ApiError::new(
        StatusCode::NOT_IMPLEMENTED,
        format!("Not implemented: requires {}", VOICE_PROVIDER[0].name()),
    )
}

fn provider_error(error: voice::provider::VoiceProviderError) -> ApiError {
    use voice::provider::VoiceProviderError as E;
    match error {
        E::RequiresDependency(dependency) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {dependency}"),
        ),
        E::ContextUnavailable | E::AdapterUnavailable => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "voice provider context is unavailable",
        ),
        E::ContextChanged => ApiError::new(StatusCode::CONFLICT, "voice provider context changed"),
        E::AdmissionDenied(message) => ApiError::new(StatusCode::CONFLICT, message),
        E::Policy(error) => match error {
            studio_runtime_services::voice::policy::VoiceError::InvalidSessionIdentity
            | studio_runtime_services::voice::policy::VoiceError::InvalidOffer
            | studio_runtime_services::voice::policy::VoiceError::SessionIdentityConflict
            | studio_runtime_services::voice::policy::VoiceError::ActiveSessionExists
            | studio_runtime_services::voice::policy::VoiceError::UnknownSession
            | studio_runtime_services::voice::policy::VoiceError::UnsupportedAudioFormat
            | studio_runtime_services::voice::policy::VoiceError::InvalidAudioEncoding
            | studio_runtime_services::voice::policy::VoiceError::AudioTooLarge
            | studio_runtime_services::voice::policy::VoiceError::AudioIdentityConflict => {
                ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
            }
            studio_runtime_services::voice::policy::VoiceError::InvalidStoredSession
            | studio_runtime_services::voice::policy::VoiceError::InvalidClock => {
                ApiError::internal()
            }
        },
        E::Voice(error) => voice_error(error),
        E::Monitor(error) => match error {
            studio_runtime_services::monitor_policy::MonitorError::Refused(message) => {
                ApiError::new(StatusCode::BAD_REQUEST, message)
            }
            studio_runtime_services::monitor_policy::MonitorError::Unavailable(name) => {
                ApiError::dependency(name)
            }
            studio_runtime_services::monitor_policy::MonitorError::InvalidStored {
                id,
                location,
            } => ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid stored object monitors/{id} at {location}"),
            ),
            studio_runtime_services::monitor_policy::MonitorError::Event(
                studio_runtime_services::event_write::EventWriteError::RemoteEventsUnavailable,
            ) => ApiError::dependency("cross-server/remote-events"),
            _ => ApiError::internal(),
        },
        E::NotSent => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "voice request was not sent",
        ),
        E::Unknown => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "voice request outcome is unknown",
        ),
        E::Rejected => ApiError::new(StatusCode::BAD_GATEWAY, "voice provider rejected request"),
        _ => ApiError::internal(),
    }
}

fn voice_error(error: VoiceError) -> ApiError {
    match error {
        VoiceError::InvalidRequest(_) => ApiError::validation(vec![Value::String("body".into())]),
        VoiceError::Conflict(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        VoiceError::InvalidStoredObject { object, id } => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}/{id}"),
        ),
        VoiceError::RequiresDependency(dependency) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {dependency}"),
        ),
        VoiceError::Coordination(error) => match error {
            studio_runtime_services::coordination::CoordinationError::RequiresDependency(dep) => {
                ApiError::new(
                    StatusCode::NOT_IMPLEMENTED,
                    format!("Not implemented: requires {dep}"),
                )
            }
            studio_runtime_services::coordination::CoordinationError::InvalidRequest(message)
            | studio_runtime_services::coordination::CoordinationError::Conflict(message) => {
                ApiError::new(StatusCode::BAD_REQUEST, message)
            }
            studio_runtime_services::coordination::CoordinationError::InvalidStored {
                object,
                id,
            } => ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid stored object {object}/{id}"),
            ),
            studio_runtime_services::coordination::CoordinationError::AdmissionDenied(message) => {
                ApiError::new(StatusCode::BAD_REQUEST, message)
            }
            studio_runtime_services::coordination::CoordinationError::BudgetRuntime(_) => {
                ApiError::internal()
            }
            studio_runtime_services::coordination::CoordinationError::Store(_) => {
                ApiError::internal()
            }
        },
        VoiceError::Store(studio_runtime_store::StoreError::InvalidStoredRecord { object, id }) => {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid stored object {object}/{id}"),
            )
        }
        VoiceError::Store(studio_runtime_store::StoreError::AlreadyOwned) => ApiError::new(
            StatusCode::CONFLICT,
            "another runtime owns the state directory",
        ),
        VoiceError::Store(_) | VoiceError::EventWrite(_) | VoiceError::Json(_) => {
            ApiError::internal()
        }
    }
}

fn voice_audio_error(error: voice::VoiceAudioError) -> ApiError {
    match error {
        voice::VoiceAudioError::Voice(error) => voice_error(error),
        voice::VoiceAudioError::Request(error) => match error {
            voice::policy::VoiceError::UnknownSession
            | voice::policy::VoiceError::UnsupportedAudioFormat
            | voice::policy::VoiceError::InvalidAudioEncoding
            | voice::policy::VoiceError::AudioTooLarge => {
                ApiError::validation(vec![Value::String("body".into())])
            }
            voice::policy::VoiceError::AudioIdentityConflict
            | voice::policy::VoiceError::SessionIdentityConflict => ApiError::new(
                StatusCode::BAD_REQUEST,
                "Voice session or audio identity conflicts with its original content",
            ),
            _ => ApiError::internal(),
        },
        voice::VoiceAudioError::Store(studio_runtime_store::StoreError::InvalidStoredRecord {
            object,
            id,
        }) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object {object}/{id}"),
        ),
        voice::VoiceAudioError::Store(studio_runtime_store::StoreError::AlreadyOwned) => {
            ApiError::new(
                StatusCode::CONFLICT,
                "another runtime owns the state directory",
            )
        }
        voice::VoiceAudioError::Store(_) | voice::VoiceAudioError::Io(_) => ApiError::internal(),
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "voice",
        routes: Some(routes),
        dependencies: &["provider-session/voice"],
    }
}
