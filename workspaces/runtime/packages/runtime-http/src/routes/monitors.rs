//! Read a monitor's bounded, private log file.

use std::fs::{self, File};
use std::io::Read;
use std::path::Path;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, Response, StatusCode, header};
use serde_json::Value;

use super::extractors::{ApiHeaders, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get};
use crate::error::ApiError;
use crate::router::HttpState;
use studio_runtime_store::MonitorsRepository;

const MAX_LOG_READ_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![get(
        "/api/monitor/log",
        OperationStatus::Implemented,
        log,
    )])
}

async fn log(
    State(state): State<HttpState>,
    query: QueryParams,
    ApiHeaders(headers): ApiHeaders,
) -> Result<Response<Body>, ApiError> {
    let id = query
        .typed_value("id")
        .and_then(Value::as_str)
        .ok_or_else(ApiError::internal)?
        .to_owned();
    let range = headers
        .get("range")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let state_dir = state.state_dir.clone();
    let response_id = id.clone();
    let log = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let monitor = MonitorsRepository::get(&connection, &id)
            .map_err(|_| ApiError::internal())?
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown monitor"))?;
        if monitor.id != id {
            return Err(invalid_monitor(&monitor.id));
        }
        let record =
            serde_json::from_str::<crate::generated::MonitorRecordResponse>(&monitor.record)
                .map_err(|_| invalid_monitor(&monitor.id))?;
        let value = serde_json::to_value(record).map_err(|_| ApiError::internal())?;
        let log_path = value
            .get("log")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_monitor(&monitor.id))?;
        let tail = value
            .get("tail")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_monitor(&monitor.id))?
            .as_bytes()
            .to_vec();
        let bytes = value
            .get("bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_monitor(&monitor.id))?;
        let data = read_log(&state_dir, log_path, &monitor.id, &tail)?;
        let truncated = bytes > data.len() as u64;
        Ok::<_, ApiError>((data, truncated))
    })
    .await
    .map_err(|_| ApiError::internal())??;

    log_response(log.0, log.1, &response_id, range.as_deref())
}

fn read_log(
    state_dir: &str,
    stored_path: &str,
    id: &str,
    fallback: &[u8],
) -> Result<Vec<u8>, ApiError> {
    let state_root = fs::canonicalize(state_dir).map_err(|_| ApiError::internal())?;
    let logs_root = state_root.join("monitor-logs");
    let path = Path::new(stored_path);
    let contained = path.starts_with(&logs_root) && path.parent() == Some(logs_root.as_path());
    if !contained {
        return Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Invalid stored object monitors/{id}"),
        ));
    }
    match fs::symlink_metadata(&logs_root) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Invalid stored object monitors/{id}"),
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(fallback.to_vec());
        }
        Err(_) => return Err(ApiError::internal()),
    }
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            if metadata.len() > MAX_LOG_READ_BYTES {
                return Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Monitor log exceeds the bounded read limit",
                ));
            }
            let mut file = File::open(path).map_err(|_| ApiError::internal())?;
            let opened = file.metadata().map_err(|_| ApiError::internal())?;
            let canonical_path = fs::canonicalize(path).map_err(|_| ApiError::internal())?;
            if opened.len() > MAX_LOG_READ_BYTES
                || !opened.is_file()
                || canonical_path.parent() != Some(logs_root.as_path())
            {
                return Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Monitor log exceeds the bounded read limit",
                ));
            }
            let mut data = Vec::with_capacity(opened.len() as usize);
            file.by_ref()
                .take(MAX_LOG_READ_BYTES + 1)
                .read_to_end(&mut data)
                .map_err(|_| ApiError::internal())?;
            if data.len() as u64 > MAX_LOG_READ_BYTES {
                return Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Monitor log exceeds the bounded read limit",
                ));
            }
            Ok(data)
        }
        Ok(_) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Invalid stored monitor log",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(fallback.to_vec()),
        Err(_) => Err(ApiError::internal()),
    }
}

fn log_response(
    data: Vec<u8>,
    truncated: bool,
    id: &str,
    range: Option<&str>,
) -> Result<Response<Body>, ApiError> {
    let size = data.len() as u64;
    let (start, end, status) = match range {
        Some(range) => match byte_range(range, size) {
            Some((start, end)) => (start, end, StatusCode::PARTIAL_CONTENT),
            None => {
                let mut response = Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .body(Body::empty())
                    .map_err(|_| ApiError::internal())?;
                set_header(
                    response.headers_mut(),
                    header::CONTENT_RANGE,
                    &format!("bytes */{size}"),
                )?;
                return Ok(response);
            }
        },
        None => (0, size, StatusCode::OK),
    };
    let length = end - start;
    if length > MAX_LOG_READ_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Requested monitor log range exceeds the bounded read limit",
        ));
    }
    let body = data
        .get(start as usize..end as usize)
        .ok_or_else(ApiError::internal)?
        .to_vec();
    let name = id
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"._-".contains(&byte) {
                char::from(byte)
            } else {
                '_'
            }
        })
        .collect::<String>();
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/plain")
        .header(header::CONTENT_LENGTH, length)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{name}.log\""),
        )
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .header("x-log-truncated", truncated.to_string())
        .body(Body::from(body))
        .map_err(|_| ApiError::internal())?;
    if status == StatusCode::PARTIAL_CONTENT {
        set_header(
            response.headers_mut(),
            header::CONTENT_RANGE,
            &format!("bytes {}-{}/{size}", start, end - 1),
        )?;
    }
    Ok(response)
}

fn byte_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let range = value.strip_prefix("bytes=")?;
    let (start, end) = range.split_once('-')?;
    if end.contains('-') || (start.is_empty() && end.is_empty()) {
        return None;
    }
    let (start, end) = if start.is_empty() {
        let suffix = end.parse::<u64>().ok()?;
        if suffix == 0 || size == 0 {
            return None;
        }
        (size.saturating_sub(suffix), size)
    } else {
        let start = start.parse::<u64>().ok()?;
        let end = if end.is_empty() {
            size
        } else {
            end.parse::<u64>().ok()?.saturating_add(1).min(size)
        };
        (start, end)
    };
    (start < size && end > start).then_some((start, end))
}

fn set_header(
    headers: &mut axum::http::HeaderMap,
    name: axum::http::header::HeaderName,
    value: &str,
) -> Result<(), ApiError> {
    let value = HeaderValue::from_str(value).map_err(|_| ApiError::internal())?;
    headers.insert(name, value);
    Ok(())
}

fn invalid_monitor(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object monitors/{id}"),
    )
}
