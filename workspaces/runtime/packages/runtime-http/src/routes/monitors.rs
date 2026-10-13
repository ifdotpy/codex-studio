//! Read a monitor's private log file using bounded streaming.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::State;
use axum::http::{Response, StatusCode, header};
use serde_json::Value;

use super::extractors::{ApiHeaders, QueryParams};
use super::manifest::{OperationStatus, RouteGroup, get};
use crate::error::ApiError;
use crate::router::HttpState;
use studio_runtime_store::{AgentId, AgentRepository, MonitorsRepository};

const LOG_CHUNK_BYTES: u64 = 65_536;

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
        .filter(|range| !range.is_empty())
        .map(str::to_owned);
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let state_dir = state.state_dir.clone();
    let response_id = id.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let monitor = MonitorsRepository::get(&connection, &id)
            .map_err(|_| ApiError::internal())?
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown monitor"))?;
        let monitor_id = monitor.id().as_ref();
        let value = monitor.value();
        let owner = value
            .get("agent")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_monitor(monitor_id))?;
        let agent = AgentRepository::get(&connection, &AgentId::from(owner))
            .map_err(|_| ApiError::internal())?
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown agent"))?;
        let agent_value = agent.value();
        if agent_value.get("deletedAt").is_some_and(is_truthy) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "This agent was deleted",
            ));
        }
        let path = value
            .get("log")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_monitor(monitor_id))?;
        let tail = value
            .get("tail")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_monitor(monitor_id))?
            .as_bytes()
            .to_vec();
        let bytes = value
            .get("bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| invalid_monitor(monitor_id))?;
        let (mut source, size) = open_log(&state_dir, path, monitor_id, tail)?;
        let truncated = bytes > size;
        let (start, end, status) = match range.as_deref() {
            Some(range) => match byte_range(range, size) {
                Some((start, end)) => (start, end, StatusCode::PARTIAL_CONTENT),
                None => return Ok(Prepared::unsatisfiable(size)),
            },
            None => (0, size, StatusCode::OK),
        };
        if let LogSource::File(file) = &mut source {
            file.seek(SeekFrom::Start(start))
                .map_err(|_| ApiError::internal())?;
        }
        Ok::<_, ApiError>(Prepared {
            source,
            size,
            truncated,
            start,
            end,
            status,
        })
    })
    .await
    .map_err(|_| ApiError::internal())??;
    prepared.response(&response_id)
}

enum LogSource {
    File(File),
    Bytes(Vec<u8>),
}

struct Prepared {
    source: LogSource,
    size: u64,
    truncated: bool,
    start: u64,
    end: u64,
    status: StatusCode,
}

impl Prepared {
    fn unsatisfiable(size: u64) -> Self {
        Self {
            source: LogSource::Bytes(Vec::new()),
            size,
            truncated: false,
            start: 0,
            end: 0,
            status: StatusCode::RANGE_NOT_SATISFIABLE,
        }
    }

    fn response(self, id: &str) -> Result<Response<Body>, ApiError> {
        if self.status == StatusCode::RANGE_NOT_SATISFIABLE {
            return Response::builder()
                .status(self.status)
                .header(header::CONTENT_RANGE, format!("bytes */{}", self.size))
                .body(Body::empty())
                .map_err(|_| ApiError::internal());
        }
        let length = self.end - self.start;
        let safe_name = id
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || "._-".contains(ch) {
                    ch
                } else {
                    '_'
                }
            })
            .collect::<String>();
        let mut builder = Response::builder()
            .status(self.status)
            .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .header(header::CONTENT_LENGTH, length)
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{safe_name}.log\""),
            )
            .header(header::ACCEPT_RANGES, "bytes")
            .header(header::CACHE_CONTROL, "no-store")
            .header("x-content-type-options", "nosniff")
            .header("x-log-truncated", self.truncated.to_string());
        if self.status == StatusCode::PARTIAL_CONTENT {
            builder = builder.header(
                header::CONTENT_RANGE,
                format!("bytes {}-{}/{}", self.start, self.end - 1, self.size),
            );
        }
        let body = match self.source {
            LogSource::Bytes(bytes) => {
                Body::from(bytes[self.start as usize..self.end as usize].to_vec())
            }
            LogSource::File(file) => {
                let stream = futures_util::stream::try_unfold(
                    (file, length),
                    |(mut file, remaining)| async move {
                        if remaining == 0 {
                            return Ok::<_, std::io::Error>(None);
                        }
                        let limit = remaining.min(LOG_CHUNK_BYTES) as usize;
                        let (file, chunk) = tokio::task::spawn_blocking(move || {
                            let mut chunk = vec![0; limit];
                            let count = file.read(&mut chunk)?;
                            chunk.truncate(count);
                            Ok::<_, std::io::Error>((file, chunk))
                        })
                        .await
                        .map_err(std::io::Error::other)??;
                        if chunk.is_empty() {
                            return Ok(None);
                        }
                        let count = chunk.len() as u64;
                        Ok(Some((chunk, (file, remaining - count))))
                    },
                );
                Body::from_stream(stream)
            }
        };
        builder.body(body).map_err(|_| ApiError::internal())
    }
}

fn open_log(
    state_dir: &str,
    stored: &str,
    id: &str,
    fallback: Vec<u8>,
) -> Result<(LogSource, u64), ApiError> {
    let root = fs::canonicalize(state_dir)
        .map_err(|_| ApiError::internal())?
        .join("monitor-logs");
    let path = Path::new(stored);
    if path.parent() != Some(root.as_path()) {
        return Err(invalid_monitor(id));
    }
    match fs::symlink_metadata(&root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((LogSource::Bytes(fallback.clone()), fallback.len() as u64));
        }
        Ok(metadata) if metadata.file_type().is_dir() => (),
        _ => return Err(invalid_monitor(id)),
    }
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok((LogSource::Bytes(fallback.clone()), fallback.len() as u64))
        }
        Ok(metadata) if metadata.file_type().is_file() => {
            let file = File::open(path).map_err(|_| ApiError::internal())?;
            let opened = file.metadata().map_err(|_| ApiError::internal())?;
            if !opened.is_file()
                || fs::canonicalize(path)
                    .ok()
                    .and_then(|path| path.parent().map(PathBuf::from))
                    != Some(root)
            {
                return Err(invalid_monitor(id));
            }
            Ok((LogSource::File(file), opened.len()))
        }
        _ => Err(invalid_monitor(id)),
    }
}

fn byte_range(value: &str, size: u64) -> Option<(u64, u64)> {
    let range = value.strip_prefix("bytes=")?;
    let (start, end) = range.split_once('-')?;
    if end.contains('-')
        || (!start.is_empty() && !start.bytes().all(|b| b.is_ascii_digit()))
        || (!end.is_empty() && !end.bytes().all(|b| b.is_ascii_digit()))
        || (start.is_empty() && end.is_empty())
    {
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

fn invalid_monitor(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object monitors/{id}"),
    )
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}
