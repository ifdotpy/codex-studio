use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::schema::{API_SCHEMA_HASH_HEADER, api_schema_hash};

pub(crate) const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' https://api.openai.com https://*.ts.net:*; img-src 'self' data: blob: https: http:; media-src 'self' blob: data:; frame-src 'self' blob: http://*.localhost:*; frame-ancestors 'none'; base-uri 'none'";
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
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InternalErrorKind {
    HandlerPanic,
    StoreConfiguration,
    StoreData,
    StoreDatabase,
    StoreIo,
    StoreOwnership,
    StoreSchema,
    StoreConcurrency,
    ResponseBodyRead,
    Unclassified,
}

impl InternalErrorKind {
    pub(crate) fn log_class(self) -> &'static str {
        match self {
            Self::HandlerPanic => "handler_panic",
            Self::StoreConfiguration => "store_configuration",
            Self::StoreData => "store_data",
            Self::StoreDatabase => "store_database",
            Self::StoreIo => "store_io",
            Self::StoreOwnership => "store_ownership",
            Self::StoreSchema => "store_schema",
            Self::StoreConcurrency => "store_concurrency",
            Self::ResponseBodyRead => "response_body_read",
            Self::Unclassified => "unclassified",
        }
    }

    pub(crate) fn from_store_error(error: &studio_runtime_store::StoreError) -> Self {
        use studio_runtime_store::StoreError;
        match error {
            StoreError::StateDirectory(_) => Self::StoreIo,
            StoreError::AlreadyOwned => Self::StoreOwnership,
            StoreError::MissingDatabase(_) | StoreError::UnsafeStateDirectory => {
                Self::StoreConfiguration
            }
            StoreError::SchemaMismatch(_)
            | StoreError::SchemaObjectMismatch { .. }
            | StoreError::RecordContract(_) => Self::StoreSchema,
            StoreError::InvalidSyncObject { .. }
            | StoreError::InvalidStoredRecord { .. }
            | StoreError::InvalidStoredObject { .. }
            | StoreError::InvalidSyncObjectField { .. }
            | StoreError::Json(_)
            | StoreError::SchemaValidation(_) => Self::StoreData,
            StoreError::Sqlite(_)
            | StoreError::CommitOutcomeUnknown(_)
            | StoreError::CommitActorOutcomeUnknown(_)
            | StoreError::WriterActorInitialization(_)
            | StoreError::WriterActorControl(_) => Self::StoreDatabase,
            StoreError::WriterPoisoned | StoreError::NestedRead => Self::StoreConcurrency,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct InternalErrorCause(pub InternalErrorKind);

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            location: None,
            outcome: None,
            content_type: None,
            internal_cause: None,
        }
    }

    /// Mark a rejected preflight as safely retryable; callers must use this
    /// only when the operation has not been persisted or sent to a child.
    pub(crate) fn not_applied(message: impl Into<String>) -> Self {
        let mut error = Self::new(StatusCode::BAD_REQUEST, message);
        error.outcome = Some(crate::generated::ErrorResponseOutcome::NotApplied);
        error
    }

    pub(crate) fn validation(location: Vec<serde_json::Value>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid request".to_owned(),
            location: Some(serde_json::Value::Array(location)),
            outcome: None,
            content_type: None,
            internal_cause: None,
        }
    }

    #[allow(
        dead_code,
        reason = "reserved for JSON transport rejections from route extractors"
    )]
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
        Self::new(
            STORED_DATA_UNAVAILABLE,
            format!("Stored data {object}/{id} is unavailable"),
        )
    }
}

impl ApiError {
    pub(crate) fn stored(error: studio_runtime_store::StoreError) -> Self {
        use studio_runtime_store::StoreError;
        let cause = InternalErrorKind::from_store_error(&error);
        match error {
            StoreError::InvalidSyncObject { scope, id } => Self::invalid_stored(&scope, &id),
            StoreError::InvalidStoredObject { table, id }
            | StoreError::InvalidStoredRecord { object: table, id } => {
                Self::invalid_stored(table, &id)
            }
            StoreError::InvalidSyncObjectField { scope, id, .. } => {
                Self::invalid_stored(&scope, &id)
            }
            _ => Self::internal_with_cause(cause),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = serde_json::json!({"error": self.message});
        if let Some(location) = self.location {
            body["location"] = location;
        }
        if let Some(outcome) = self.outcome
            && let Ok(outcome) = serde_json::to_value(outcome)
        {
            body["outcome"] = outcome;
        }
        let mut response = (self.status, Json(body)).into_response();
        if self.status == StatusCode::INTERNAL_SERVER_ERROR
            && let Some(cause) = self.internal_cause
        {
            response.extensions_mut().insert(InternalErrorCause(cause));
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
