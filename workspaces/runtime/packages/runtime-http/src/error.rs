use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::schema::{API_SCHEMA_HASH_HEADER, api_schema_hash};

pub(crate) const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self' https://api.openai.com https://*.ts.net:*; img-src 'self' data: blob: https: http:; media-src 'self' blob: data:; frame-src 'self' blob: http://*.localhost:*; frame-ancestors 'none'; base-uri 'none'";

/// Route-level error that always serializes using the server's common envelope.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
    location: Option<serde_json::Value>,
    content_type: Option<&'static str>,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
            location: None,
            content_type: None,
        }
    }

    pub(crate) fn validation(location: Vec<serde_json::Value>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: "Invalid request".to_owned(),
            location: Some(serde_json::Value::Array(location)),
            content_type: None,
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

    pub(crate) fn dependency(name: &str) -> Self {
        Self::new(
            StatusCode::NOT_IMPLEMENTED,
            format!("Not implemented: requires {name}"),
        )
    }
}

impl ApiError {
    pub(crate) fn stored(error: studio_runtime_store::StoreError) -> Self {
        use studio_runtime_store::StoreError;
        match error {
            StoreError::InvalidSyncObject { scope, id } => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid stored object {scope}/{id}"),
            ),
            StoreError::InvalidStoredObject { table, id } => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid stored object {table}/{id}"),
            ),
            _ => Self::internal(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = serde_json::json!({"error": self.message});
        if let Some(location) = self.location {
            body["location"] = location;
        }
        let mut response = (self.status, Json(body)).into_response();
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
