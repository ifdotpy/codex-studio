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
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal server error")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        error_response(self.status, &self.message)
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
    (status, Json(json!({"error": message}))).into_response()
}
