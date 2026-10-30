//! Persisted voice routes and explicit provider-dependent operations.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use studio_runtime_services::voice::{self, VoiceError};

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, post};
use crate::error::ApiError;
use crate::router::HttpState;

const VOICE_PROVIDER: [Dependency; 1] = [Dependency::new("provider-session/voice")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post("/api/voice/end", OperationStatus::Implemented, end),
        post("/api/voice/records", OperationStatus::Implemented, records),
        post("/api/voice/start", OperationStatus::Implemented, start),
        post("/api/voice/status", OperationStatus::Implemented, status),
    ])
}

async fn status(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceStatusRequest>,
) -> Result<Json<crate::generated::VoiceStatusResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let agent_id = body.agent;
    let result = voice::status(&store, &agent_id)
        .await
        .map_err(voice_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceStatusResponse,
    >(result)?))
}

async fn records(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceRecordsRequest>,
) -> Result<Json<crate::generated::VoiceRecordsResponse>, ApiError> {
    let store = state.store.ok_or_else(ApiError::internal)?;
    let agent_id = body.agent;
    let after = body.after.unwrap_or_default();
    let result = voice::records(&store, &agent_id, after)
        .await
        .map_err(voice_error)?;
    Ok(Json(super::generated_json::<
        crate::generated::VoiceRecordsResponse,
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
            .await
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

async fn start(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<crate::generated::VoiceStartRequest>,
) -> Result<Json<crate::generated::VoiceSessionResponse>, ApiError> {
    let provider = state.voice_request_port.clone();
    let store = state.store.ok_or_else(ApiError::internal)?;
    let result = voice::provider::start_admitted(
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
        E::Expired => ApiError::new(StatusCode::GONE, error.to_string()),
        E::RequestResult(error) => error.into(),
        E::Outbox(error) => error.into(),
        E::Provider(_) => ApiError::internal(),
        E::RequiresDependency(dependency) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {dependency}"),
        ),
        E::ContextUnavailable | E::AdapterUnavailable => ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "voice provider context is unavailable",
        ),
        E::ContextChanged => ApiError::new(StatusCode::CONFLICT, "voice provider context changed"),
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
            studio_runtime_services::monitor_policy::MonitorError::Expired => {
                ApiError::new(StatusCode::GONE, error.to_string())
            }
            studio_runtime_services::monitor_policy::MonitorError::Store(error) => {
                ApiError::stored(error)
            }
            studio_runtime_services::monitor_policy::MonitorError::Refused(message) => {
                ApiError::new(StatusCode::BAD_REQUEST, message)
            }
            studio_runtime_services::monitor_policy::MonitorError::Unavailable(name) => {
                ApiError::dependency(name)
            }
            studio_runtime_services::monitor_policy::MonitorError::InvalidStored {
                id,
                location: _,
            } => ApiError::invalid_stored("monitors", &id),
            studio_runtime_services::monitor_policy::MonitorError::Event(
                studio_runtime_services::event_write::EventWriteError::RemoteEventsUnavailable,
            ) => ApiError::dependency("cross-server/remote-events"),
            studio_runtime_services::monitor_policy::MonitorError::InvalidResultReceipt(_)
            | studio_runtime_services::monitor_policy::MonitorError::InvalidDeliveryIdentity(_)
            | studio_runtime_services::monitor_policy::MonitorError::InvalidOutputReceipt(_)
            | studio_runtime_services::monitor_policy::MonitorError::TaskFailed
            | studio_runtime_services::monitor_policy::MonitorError::InputOutcomeUnknown
            | studio_runtime_services::monitor_policy::MonitorError::ProviderRequest { .. }
            | studio_runtime_services::monitor_policy::MonitorError::ConfigurationRejected {
                ..
            }
            | studio_runtime_services::monitor_policy::MonitorError::ConfigurationReceipt {
                ..
            }
            | studio_runtime_services::monitor_policy::MonitorError::Event(_)
            | studio_runtime_services::monitor_policy::MonitorError::Rules(_)
            | studio_runtime_services::monitor_policy::MonitorError::Io(_) => ApiError::internal(),
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
        E::Store(_) | E::Operation => ApiError::internal(),
    }
}

fn voice_error(error: VoiceError) -> ApiError {
    match error {
        VoiceError::InvalidRequest(_) => ApiError::validation(vec!["body".into()]),
        VoiceError::Conflict(message) => ApiError::new(StatusCode::BAD_REQUEST, message),
        VoiceError::InvalidStoredObject { object, id } => ApiError::invalid_stored(object, &id),
        VoiceError::RequiresDependency(dependency) => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {dependency}"),
        ),
        VoiceError::Coordination(error) => match error {
            studio_runtime_services::coordination::CoordinationError::RequestResult(error) => {
                error.into()
            }
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
            } => ApiError::invalid_stored(object, &id),
            studio_runtime_services::coordination::CoordinationError::AdmissionDenied(message) => {
                ApiError::new(StatusCode::BAD_REQUEST, message)
            }
            studio_runtime_services::coordination::CoordinationError::Store(_) => {
                ApiError::internal()
            }
        },
        VoiceError::Store(studio_runtime_store::StoreError::InvalidStoredRecord { object, id }) => {
            ApiError::invalid_stored(object, &id)
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

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "voice",
        routes: Some(routes),
        dependencies: &["provider-session/voice"],
    }
}
