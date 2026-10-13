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
use studio_runtime_store::{MonitorsRepository, RulesRepository};

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
        if monitor.id != id {
            return Err(invalid_monitor(&monitor.id));
        }
        let record =
            serde_json::from_str::<crate::generated::MonitorRecordResponse>(&monitor.record)
                .map_err(|_| invalid_monitor(&monitor.id))?;
        let value = serde_json::to_value(record).map_err(|_| ApiError::internal())?;
        let owner = value
            .get("agent")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_monitor(&monitor.id))?;
        let agent = RulesRepository::agent(&connection, owner)
            .map_err(|_| ApiError::internal())?
            .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Unknown agent"))?;
        let agent_value =
            serde_json::from_str::<Value>(&agent.record).map_err(|_| invalid_agent(&agent.id))?;
        if agent.id != owner || agent_value.get("id").and_then(Value::as_str) != Some(owner) {
            return Err(invalid_agent(&agent.id));
        }
        if agent_value.get("deletedAt").is_some_and(is_truthy) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "This agent was deleted",
            ));
        }
        let path = value
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
        let (mut source, size) = open_log(&state_dir, path, &monitor.id, tail)?;
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
    if size == 0 {
        return None;
    }
    // Preserve the existing behavior for a suffix longer than the file: it
    // requests the complete file, though the parser crate marks it unsatisfiable.
    if value
        .strip_prefix("bytes=-")
        .and_then(|suffix| suffix.parse::<u64>().ok())
        .is_some_and(|suffix| suffix > size)
    {
        return Some((0, size));
    }
    let parsed = http_range_header::parse_range_header(value).ok()?;
    if parsed.ranges.len() != 1 {
        return None;
    }
    let range = parsed.validate(size).ok()?.into_iter().next()?;
    Some((*range.start(), range.end().checked_add(1)?))
}

fn invalid_monitor(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object monitors/{id}"),
    )
}

fn invalid_agent(id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object agents/{id}"),
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

#[cfg(test)]
mod tests {
    use super::byte_range;

    #[test]
    fn byte_ranges_cover_open_suffix_and_parser_edge_cases() {
        assert_eq!(byte_range("bytes=2-5", 10), Some((2, 6)));
        assert_eq!(byte_range("bytes=2-", 10), Some((2, 10)));
        assert_eq!(byte_range("bytes=-3", 10), Some((7, 10)));
        assert_eq!(byte_range("bytes=-100", 10), Some((0, 10)));
        // The crate rejects leading zeroes; the previous parser accepted them.
        assert_eq!(byte_range("bytes=00-2", 10), None);
        assert_eq!(byte_range("bytes=0-2,4-5", 10), None);
        assert_eq!(byte_range("bytes=0-2", 0), None);
    }
}
