//! Persisted voice routes and explicit provider-dependent operations.

use axum::Json;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post as axum_post;
use serde_json::Value;
use studio_runtime_services::voice::{self, VoiceError};

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, post};
use crate::error::ApiError;
use crate::router::HttpState;

const VOICE_PROVIDER: [Dependency; 1] = [Dependency::new("provider-session/voice")];
const VOICE_AUDIO: [Dependency; 1] = [Dependency::new("payload-store/voice-audio")];

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
        post(
            "/api/voice/approve",
            OperationStatus::partial(&VOICE_PROVIDER),
            refuse,
        ),
        post(
            "/api/voice/audio",
            OperationStatus::partial(&VOICE_AUDIO),
            refuse,
        ),
        post(
            "/api/voice/end",
            OperationStatus::partial(&VOICE_PROVIDER),
            end,
        ),
        post("/api/voice/record", OperationStatus::Implemented, record),
        post("/api/voice/records", OperationStatus::Implemented, records),
        post(
            "/api/voice/speech",
            OperationStatus::partial(&VOICE_PROVIDER),
            speech,
        ),
        post(
            "/api/voice/start",
            OperationStatus::partial(&VOICE_PROVIDER),
            refuse,
        ),
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
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let agent_id = body.agent;
    let result = tokio::task::spawn_blocking(move || voice::status(&store, &agent_id))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(voice_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceStatusResponse,
    >(result)?))
}

async fn record(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceRecordRequest>,
) -> Result<Json<Value>, ApiError> {
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
    let result = tokio::task::spawn_blocking(move || {
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
    })
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
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let agent_id = body.agent;
    let after = body.after.unwrap_or_default();
    let result = tokio::task::spawn_blocking(move || voice::records(&store, &agent_id, after))
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
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let agent_id = body.agent;
    let result = tokio::task::spawn_blocking(move || voice::approvals(&store, &agent_id))
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
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let (agent, request_id) = (body.agent, body.request_id);
    let result =
        tokio::task::spawn_blocking(move || voice::approval_speech(&store, &agent, &request_id))
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
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let (agent, session) = (body.agent, body.session_id);
    let result = tokio::task::spawn_blocking(move || voice::end(&store, &agent, &session))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(voice_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceSessionResponse,
    >(result)?))
}

async fn speech(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceSpeechRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let (agent, session, record) = (body.agent, body.session_id, body.record_id);
    let result = tokio::task::spawn_blocking(move || {
        voice::speech_receipt(&store, &agent, &session, &record, None)
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(voice_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceSpeechReceiptResponse,
    >(result)?))
}

async fn submit(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceSubmitRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let (agent, message, record_ids, edited) = (
        body.agent,
        body.message_id,
        body.record_ids,
        body.edited_text.flatten(),
    );
    let result = tokio::task::spawn_blocking(move || {
        voice::submit(&store, &agent, &message, &record_ids, edited.as_deref())
    })
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(voice_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceDeliveryResponse,
    >(result)?))
}

async fn refuse(
    path: axum::extract::MatchedPath,
    ApiJson(_body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    Err(
        dependency_unavailable("POST", path.as_str(), &VOICE_PROVIDER)
            .or_else(|_| dependency_unavailable("POST", path.as_str(), &VOICE_AUDIO))
            .unwrap_or_else(|_| ApiError::internal()),
    )
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
