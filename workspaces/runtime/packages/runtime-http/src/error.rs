use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use studio_runtime_schema::studio_errors::Cause;
pub(crate) use studio_runtime_schema::studio_errors::HttpErrorKind as InternalErrorKind;
pub(crate) use studio_runtime_services::studio_errors::store_error_kind;
use studio_runtime_services::studio_errors::{json_cause, store_causes};

use crate::schema::{API_SCHEMA_HASH_HEADER, api_schema_hash};

pub(crate) const CSP: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; connect-src 'self' https://api.openai.com https://*.ts.net:*; img-src 'self' data: blob: https: http:; media-src 'self' blob: data:; frame-src 'self' blob: http://*.localhost:*; frame-ancestors 'none'; base-uri 'none'";
const STORED_DATA_UNAVAILABLE: StatusCode = StatusCode::SERVICE_UNAVAILABLE;

/// Route-level error that always serializes using the server's common envelope.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
    location: Option<serde_json::Value>,
    outcome: Option<crate::generated::ErrorResponseOutcome>,
    content_type: Option<&'static str>,
    internal_cause: Option<InternalErrorKind>,
    diagnostic_causes: Vec<Cause>,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct InternalErrorCause(pub InternalErrorKind);

#[derive(Clone, Debug)]
pub(crate) struct DiagnosticCauses(pub Vec<Cause>);

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            location: None,
            outcome: None,
            content_type: None,
            internal_cause: None,
            diagnostic_causes: Vec::new(),
        }
    }

    /// Mark a rejected preflight as safely retryable; callers must use this
    /// only when the operation has not been persisted or sent to a child.
    pub(crate) fn not_applied(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message).with_not_applied()
    }

    /// Use only at a boundary that proves rollback or refusal before any effect.
    pub(crate) fn with_not_applied(mut self) -> Self {
        self.outcome = Some(crate::generated::ErrorResponseOutcome::NotApplied);
        self
    }

    pub(crate) fn outcome(&self) -> Option<&crate::generated::ErrorResponseOutcome> {
        self.outcome.as_ref()
    }

    pub(crate) fn validation(location: Vec<serde_json::Value>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid request".to_owned(),
            location: Some(serde_json::Value::Array(location)),
            outcome: None,
            content_type: None,
            internal_cause: None,
            diagnostic_causes: Vec::new(),
        }
    }

    pub(crate) fn json_transport(status: StatusCode, message: &str) -> Self {
        let mut error = Self::new(status, message);
        error.content_type = Some("application/json; charset=utf-8");
        error
    }

    pub fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
    }

    pub(crate) fn internal_with_cause(cause: InternalErrorKind) -> Self {
        let mut error = Self::internal();
        error.internal_cause = Some(cause);
        error
    }

    pub(crate) fn dependency(name: &str) -> Self {
        Self::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {name}"),
        )
    }

    /// Refuse an operation when persisted data cannot be trusted. Keep the
    /// affected identity in the safe response, without exposing field values.
    pub(crate) fn invalid_stored(object: &str, id: &str) -> Self {
        let mut error = Self::new(
            STORED_DATA_UNAVAILABLE,
            format!("Stored data {object}/{id} is unavailable"),
        );
        error.internal_cause = Some(InternalErrorKind::StoreData);
        error.diagnostic_causes.push(Cause::StoredRecord {
            object: object.into(),
            id: id.into(),
            pointer: None,
        });
        error
    }

    pub(crate) fn json(error: serde_json::Error) -> Self {
        let mut result = Self::internal_with_cause(InternalErrorKind::StoreData);
        result.diagnostic_causes.push(json_cause(&error));
        result
    }
}

/// Lets store reads run with the route's error type (`RuntimeStore::read_async`).
impl From<studio_runtime_store::StoreError> for ApiError {
    fn from(error: studio_runtime_store::StoreError) -> Self {
        Self::stored(error)
    }
}

impl From<studio_runtime_store::request_results::RequestResultError> for ApiError {
    fn from(error: studio_runtime_store::request_results::RequestResultError) -> Self {
        use studio_runtime_schema::persistence::RequestIdentityError;
        use studio_runtime_store::request_results::RequestResultError;
        match error {
            RequestResultError::Conflict { .. } => Self::new(
                StatusCode::CONFLICT,
                "This request ID has different content",
            ),
            RequestResultError::Expired => {
                Self::new(StatusCode::GONE, "This request ID has expired")
            }
            RequestResultError::Identity(
                RequestIdentityError::Expired { .. } | RequestIdentityError::InvalidUuidV7(_),
            ) => Self::new(StatusCode::GONE, "This request ID has expired"),
            RequestResultError::Identity(RequestIdentityError::FutureTimestamp { .. }) => {
                Self::new(
                    StatusCode::BAD_REQUEST,
                    "This request ID was issued in the future",
                )
            }
            RequestResultError::Json(error) => Self::json(error),
            RequestResultError::Sql(error) => Self::stored(error.into()),
        }
    }
}

impl From<studio_runtime_store::outbox::OutboxError> for ApiError {
    fn from(error: studio_runtime_store::outbox::OutboxError) -> Self {
        use studio_runtime_store::outbox::OutboxError;
        match error {
            OutboxError::Conflict(_) => Self::new(
                StatusCode::CONFLICT,
                "This request ID has different content",
            ),
            OutboxError::Json(error) => Self::json(error),
            OutboxError::Sql(error) => Self::stored(error.into()),
        }
    }
}

impl ApiError {
    /// HTTP status alone never establishes whether a mutation took effect.
    pub(crate) fn status(&self) -> StatusCode {
        self.status
    }

    pub(crate) fn message(&self) -> &str {
        &self.message
    }

    pub(crate) fn stored(error: studio_runtime_store::StoreError) -> Self {
        use studio_runtime_store::StoreError;
        let cause = store_error_kind(&error);
        let causes = store_causes(&error);
        let mut result = match error {
            StoreError::RequestResult(error) => error.into(),
            StoreError::Outbox(error) => error.into(),
            StoreError::Migration { source, .. } => Self::stored(*source),
            StoreError::InvalidSyncObject { scope, id } => Self::invalid_stored(&scope, &id),
            StoreError::InvalidStoredObject { table, id }
            | StoreError::InvalidStoredRecord { object: table, id } => {
                Self::invalid_stored(table, &id)
            }
            StoreError::InvalidSyncObjectField { scope, id, .. } => {
                Self::invalid_stored(&scope, &id)
            }
            StoreError::StateDirectory(_)
            | StoreError::AlreadyOwned
            | StoreError::MissingDatabase(_)
            | StoreError::UnsafeStateDirectory
            | StoreError::SchemaMismatch(_)
            | StoreError::SchemaObjectMismatch { .. }
            | StoreError::Sqlite(_)
            | StoreError::CommitOutcomeUnknown(_)
            | StoreError::Json(_)
            | StoreError::SchemaValidation(_)
            | StoreError::RecordContract(_)
            | StoreError::WriterPoisoned
            | StoreError::WriterActorInitialization(_)
            | StoreError::WriterActorControl(_)
            | StoreError::CommitActorOutcomeUnknown(_)
            | StoreError::ReadTaskFailed => Self::internal_with_cause(cause),
        };
        result.internal_cause = Some(cause);
        result.diagnostic_causes = causes;
        result
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = serde_json::json!({"error": self.message});
        if let Some(outcome) = self.outcome()
            && let Ok(outcome) = serde_json::to_value(outcome)
        {
            body["outcome"] = outcome;
        }
        if let Some(location) = self.location {
            body["location"] = location;
        }
        let mut response = (self.status, Json(body)).into_response();
        if let Some(outcome) = self.outcome {
            let certainty = match outcome {
                crate::generated::ErrorResponseOutcome::NotApplied => {
                    studio_runtime_schema::studio_errors::EffectCertainty::NotApplied
                }
            };
            response.extensions_mut().insert(certainty);
        }
        if let Some(cause) = self.internal_cause {
            response.extensions_mut().insert(InternalErrorCause(cause));
        }
        if !self.diagnostic_causes.is_empty() {
            response
                .extensions_mut()
                .insert(DiagnosticCauses(self.diagnostic_causes));
        }
        if let Some(content_type) = self.content_type
            && let Ok(value) = HeaderValue::from_str(content_type)
        {
            response.headers_mut().insert(header::CONTENT_TYPE, value);
        }
        response
    }
}

pub(crate) fn with_api_headers(
    mut response: Response,
    request_id: Option<&HeaderValue>,
) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        API_SCHEMA_HASH_HEADER,
        HeaderValue::from_static(api_schema_hash()),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert("content-security-policy", HeaderValue::from_static(CSP));
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if let Some(value) = request_id {
        headers.insert("x-studio-request-id", value.clone());
    }
    response
}

pub(crate) fn method_not_allowed_response(path: &str) -> Response {
    let mut response = error_response(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
    if let Some(methods) = crate::schema::methods_for_path(path)
        && let Ok(value) = HeaderValue::from_str(&methods.join(", "))
    {
        response.headers_mut().insert(header::ALLOW, value);
    }
    response
}

pub(crate) fn error_response(status: StatusCode, message: &str) -> Response {
    let mut response = (status, Json(json!({"error": message}))).into_response();
    if message == "Invalid JSON" {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
    }
    response
}

#[cfg(test)]
mod tests {
    use super::{ApiError, InternalErrorCause, InternalErrorKind};
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use studio_runtime_store::StoreError;

    #[tokio::test]
    async fn proven_precommit_refusal_preserves_status_location_and_explicit_outcome() {
        let error = ApiError::stored(StoreError::InvalidSyncObjectField {
            scope: "runtime_work".into(),
            id: "task".into(),
            pointer: "/status".into(),
            reason: "password=private-canary".into(),
        })
        .with_not_applied();
        assert!(matches!(
            error.outcome(),
            Some(crate::generated::ErrorResponseOutcome::NotApplied)
        ));
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let facts = response
            .extensions()
            .get::<super::DiagnosticCauses>()
            .expect("typed facts");
        assert!(
            matches!(&facts.0[0],studio_runtime_schema::studio_errors::Cause::StoredRecord{pointer:Some(pointer),..} if pointer=="/status")
        );
        let body = to_bytes(response.into_body(), 4096).await.expect("body");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(body["outcome"], "not_applied");
        assert!(!body.to_string().contains("private-canary"));
    }

    #[tokio::test]
    async fn not_applied_outcome_is_optional_in_the_common_error_envelope() {
        let response = ApiError::not_applied("invalid local input").into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 4096)
            .await
            .expect("response body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("JSON body");
        assert_eq!(value["error"], "invalid local input");
        assert_eq!(value["outcome"], "not_applied");

        let response = ApiError::new(StatusCode::BAD_REQUEST, "ordinary error").into_response();
        let body = to_bytes(response.into_body(), 4096)
            .await
            .expect("response body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("JSON body");
        assert!(value.get("outcome").is_none());
    }

    #[test]
    fn malformed_stored_rows_are_unavailable_without_reclassifying_store_failures() {
        let malformed = ApiError::stored(StoreError::InvalidStoredObject {
            table: "runtime_agents",
            id: "agent-7".to_owned(),
        })
        .into_response();
        assert_eq!(malformed.status(), StatusCode::SERVICE_UNAVAILABLE);

        let database_failure =
            ApiError::stored(StoreError::Sqlite(rusqlite::Error::InvalidQuery)).into_response();
        assert_eq!(
            database_failure
                .extensions()
                .get::<InternalErrorCause>()
                .map(|cause| cause.0),
            Some(InternalErrorKind::StoreDatabase)
        );
    }
}
