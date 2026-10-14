//! Adds Python's sync entity range to successful object-shaped mutation replies.

use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{Method, Request, StatusCode, header, response::Parts};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{Value, json};
use studio_runtime_store::{EntityRow, SyncDocumentsRepository, Transaction};

use crate::error::ApiError;
use crate::router::HttpState;
use crate::validation::ValidatedRequest;

// Keep aligned with codex_sync_entities.MAX_MUTATION_SYNC_ENTITIES (line 43).
const MAX_MUTATION_SYNC_ENTITIES: i64 = 500;
const MAX_MUTATION_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
// Keep aligned with studio_api.models.MAX_SAFE_INTEGER (line 42).
const MAX_SAFE_ENTITY_SEQUENCE: i64 = 9_007_199_254_740_991;
const FEDERATION_POST_PATHS: [&str; 4] = [
    "/api/federation/v1/pair",
    "/api/federation/v1/status",
    "/api/federation/v1/message",
    "/api/federation/v1/pull",
];

#[derive(Clone, Copy)]
struct EntityCursor(i64);

struct EntityDelta {
    rows: Vec<EntityRow>,
    tombstone_floor: i64,
}

/// Sample the entity cursor before dispatch and attach the subsequent range after dispatch.
pub(crate) async fn envelope(
    State(state): State<HttpState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let Some(store) = state.store.clone() else {
        return next.run(request).await;
    };
    if should_sample(&request) {
        let cursor_store = store.clone();
        let cursor = tokio::task::spawn_blocking(move || {
            cursor_store.read_transaction(sample_entity_cursor)
        })
        .await;
        match cursor {
            Ok(Ok(cursor)) if cursor <= MAX_SAFE_ENTITY_SEQUENCE => {
                request.extensions_mut().insert(EntityCursor(cursor));
            }
            Ok(Ok(_)) => return ApiError::internal().into_response(),
            Ok(Err(error)) => {
                eprintln!("mutation sync envelope: cursor read failed: {error}");
                return ApiError::internal().into_response();
            }
            Err(error) => {
                eprintln!("mutation sync envelope: cursor read task failed: {error}");
                return ApiError::internal().into_response();
            }
        }
    }

    let cursor = request.extensions().get::<EntityCursor>().copied();
    let response = next.run(request).await;
    if response.status() >= StatusCode::BAD_REQUEST
        || response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_none_or(|value| !value.starts_with("application/json"))
    {
        return response;
    }
    let Some(EntityCursor(after)) = cursor else {
        return response;
    };

    if response
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_MUTATION_RESPONSE_BYTES)
    {
        return response;
    }
    let (parts, body) = response.into_parts();
    let bytes = match read_bounded_body(body, MAX_MUTATION_RESPONSE_BYTES).await {
        BoundedBody::Buffered(bytes) => bytes,
        BoundedBody::PassThrough(body) => return Response::from_parts(parts, body),
    };
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    let Some(object) = value.as_object() else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if object.contains_key("_syncEntities") {
        return Response::from_parts(parts, Body::from(bytes));
    }

    let delta_store = store;
    let delta = tokio::task::spawn_blocking(move || {
        delta_store.read_transaction(|snapshot| read_entity_delta(snapshot, after))
    })
    .await;
    let delta = match delta {
        Ok(Ok(delta)) => delta,
        Ok(Err(error)) => {
            return response_on_delta_error(parts, bytes, "ranged entity read", error);
        }
        Err(error) => {
            return response_on_delta_error(parts, bytes, "ranged entity read task", error);
        }
    };
    if delta
        .rows
        .iter()
        .any(|row| row.sequence > MAX_SAFE_ENTITY_SEQUENCE)
    {
        eprintln!("mutation sync envelope: entity sequence exceeds safe JSON range");
        return Response::from_parts(parts, Body::from(bytes));
    }
    if delta.rows.is_empty() {
        return Response::from_parts(parts, Body::from(bytes));
    }

    let complete =
        delta.rows.len() <= MAX_MUTATION_SYNC_ENTITIES as usize && delta.tombstone_floor <= after;
    let Some(entities) = delta
        .rows
        .iter()
        .take(MAX_MUTATION_SYNC_ENTITIES as usize)
        .map(entity_value)
        .collect::<Option<Vec<_>>>()
    else {
        eprintln!("mutation sync envelope: entity payload is missing");
        return Response::from_parts(parts, Body::from(bytes));
    };
    let body = match splice_envelope(&bytes, &entities, complete.then_some(after)) {
        Some(body) => body,
        None => {
            eprintln!("mutation sync envelope: unable to splice envelope into JSON object");
            return Response::from_parts(parts, Body::from(bytes));
        }
    };
    let mut response = Response::from_parts(parts, Body::from(body.clone()));
    if let Ok(value) = header::HeaderValue::from_bytes(body.len().to_string().as_bytes()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    response
}

fn should_sample(request: &Request<Body>) -> bool {
    matches!(
        *request.method(),
        Method::POST | Method::PUT | Method::PATCH
    ) && !FEDERATION_POST_PATHS.contains(&request.uri().path())
        && request
            .extensions()
            .get::<ValidatedRequest>()
            .and_then(|validated| validated.body.as_ref())
            .is_some_and(Value::is_object)
}

fn sample_entity_cursor(
    connection: &Transaction<'_>,
) -> Result<i64, studio_runtime_store::StoreError> {
    Ok(SyncDocumentsRepository::entity_sequence(connection)?
        .max(SyncDocumentsRepository::entity_tombstone_floor(connection)?))
}

fn original_response(parts: Parts, bytes: Bytes) -> Response {
    Response::from_parts(parts, Body::from(bytes))
}

fn response_on_delta_error(
    parts: Parts,
    bytes: Bytes,
    operation: &str,
    error: impl std::fmt::Display,
) -> Response {
    eprintln!("mutation sync envelope: {operation} failed: {error}");
    original_response(parts, bytes)
}

enum BoundedBody {
    Buffered(Bytes),
    PassThrough(Body),
}

async fn read_bounded_body(body: Body, limit: usize) -> BoundedBody {
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(item) = stream.next().await {
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(error) => {
                let prefix = Bytes::from(bytes);
                let resumed =
                    futures_util::stream::iter([Ok::<Bytes, axum::Error>(prefix), Err(error)])
                        .chain(stream);
                return BoundedBody::PassThrough(Body::from_stream(resumed));
            }
        };
        if bytes.len().saturating_add(chunk.len()) > limit {
            let prefix = Bytes::from(bytes);
            let resumed = futures_util::stream::iter([Ok::<Bytes, axum::Error>(prefix), Ok(chunk)])
                .chain(stream);
            return BoundedBody::PassThrough(Body::from_stream(resumed));
        }
        bytes.extend_from_slice(&chunk);
    }
    BoundedBody::Buffered(Bytes::from(bytes))
}

fn splice_envelope(bytes: &[u8], entities: &[Value], after: Option<i64>) -> Option<Vec<u8>> {
    let value = serde_json::from_slice::<Value>(bytes).ok()?;
    let object = value.as_object()?;
    if object.contains_key("_syncEntities") {
        return None;
    }
    let close = bytes.iter().rposition(|byte| !byte.is_ascii_whitespace())?;
    if bytes[close] != b'}' {
        return None;
    }
    let open = bytes.iter().position(|byte| !byte.is_ascii_whitespace())?;
    let has_members = close > open + 1
        && bytes[open + 1..close]
            .iter()
            .any(|byte| !byte.is_ascii_whitespace());
    let mut additions = Vec::new();
    if has_members {
        additions.push(b',');
    }
    additions.extend_from_slice(b"\"_syncEntities\":");
    additions.extend_from_slice(&serde_json::to_vec(entities).ok()?);
    if let Some(after) = after {
        additions.extend_from_slice(b",\"_syncEntitiesAfter\":");
        additions.extend_from_slice(&serde_json::to_vec(&after).ok()?);
    }
    let mut result = Vec::with_capacity(bytes.len() + additions.len());
    result.extend_from_slice(&bytes[..close]);
    result.extend_from_slice(&additions);
    result.extend_from_slice(&bytes[close..]);
    Some(result)
}

fn entity_value(row: &EntityRow) -> Option<Value> {
    Some(json!({
        "id": format!("entity:{}:{}", row.collection, row.id),
        "seq": row.sequence,
        "payload": row.payload.as_ref()?,
        "_deleted": row.deleted,
    }))
}

fn read_entity_delta(
    connection: &studio_runtime_store::Connection,
    after: i64,
) -> Result<EntityDelta, studio_runtime_store::StoreError> {
    Ok(EntityDelta {
        rows: SyncDocumentsRepository::entities_after(
            connection,
            after,
            MAX_MUTATION_SYNC_ENTITIES + 1,
        )?,
        tombstone_floor: SyncDocumentsRepository::entity_tombstone_floor(connection)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{Connection, OpenFlags};
    use std::path::Path;

    fn data_version(connection: &Connection) -> i64 {
        connection
            .query_row("PRAGMA data_version", [], |row| row.get(0))
            .expect("data_version is readable")
    }

    #[test]
    fn ranged_envelope_read_uses_a_reader_while_writer_is_held() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let path = directory.path().join("canvas.sqlite3");
        create_sync_database(&path);
        let writer = Connection::open(&path).expect("open writer");
        writer
            .execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE")
            .expect("hold writer lock");
        let before = data_version(&writer);

        let mut reader = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open read-only connection");
        reader
            .pragma_update(None, "query_only", "ON")
            .expect("set query-only mode");
        let snapshot = reader.transaction().expect("begin read snapshot");
        assert_eq!(
            sample_entity_cursor(&snapshot).expect("sample cursor with writer held"),
            6
        );
        let delta = read_entity_delta(&snapshot, 5).expect("read delta with writer held");
        assert_eq!(delta.rows.len(), 1);
        assert_eq!(delta.rows[0].sequence, 6);
        assert_eq!(data_version(&writer), before);
        writer
            .execute_batch("ROLLBACK")
            .expect("release writer lock");
    }

    fn create_sync_database(path: &Path) {
        let connection = Connection::open(path).expect("create test database");
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,payload TEXT,deleted INTEGER);
                 CREATE TABLE sync_entity_meta(key TEXT,value TEXT);
                 INSERT INTO sync_entities VALUES('agent','old',5,'{}',0);
                 INSERT INTO sync_entities VALUES('agent','new',6,'{}',0);
                 INSERT INTO sync_entity_meta VALUES('entity_tombstone_floor','0');",
            )
            .expect("seed sync rows");
    }

    #[test]
    fn sampled_cursor_includes_tombstone_floor_above_entity_sequence() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let path = directory.path().join("canvas.sqlite3");
        create_sync_database(&path);
        let mut connection = Connection::open(&path).expect("open test database");
        connection
            .execute(
                "UPDATE sync_entity_meta SET value='12' WHERE key='entity_tombstone_floor'",
                [],
            )
            .expect("raise tombstone floor");
        let snapshot = connection.transaction().expect("begin read snapshot");
        assert_eq!(sample_entity_cursor(&snapshot).expect("sample cursor"), 12);
    }

    #[test]
    fn envelope_splice_preserves_existing_json_bytes() {
        let original = br#"{ "n" : 1e+03, "name":"kept" }"#;
        let changed = splice_envelope(original, &[], Some(7)).expect("splice envelope");
        let changed = std::str::from_utf8(&changed).expect("UTF-8 JSON");
        assert!(changed.starts_with(r#"{ "n" : 1e+03, "name":"kept","_syncEntities":[]"#));
        assert!(changed.ends_with(r#", "_syncEntitiesAfter":7 }"#));
    }

    #[tokio::test]
    async fn failed_envelope_read_returns_successful_handler_response_unchanged() {
        let original = br#"{ "ok" : true, "n":1e+03 }"#;
        let handler_response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CONTENT_LENGTH, original.len())
            .body(Body::from(original.as_slice()))
            .expect("successful handler response");
        let (parts, body) = handler_response.into_parts();
        let BoundedBody::Buffered(bytes) = read_bounded_body(body, 1024).await else {
            panic!("small handler response should buffer");
        };
        let response = response_on_delta_error(
            parts,
            bytes,
            "ranged entity read",
            "simulated ranged read failure",
        );
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .expect("original response body");
        assert_eq!(body.as_ref(), original);
    }

    #[tokio::test]
    async fn oversized_body_is_returned_without_buffering_or_envelope() {
        let original = Bytes::from_static(b"response larger than the configured limit");
        let BoundedBody::PassThrough(body) =
            read_bounded_body(Body::from(original.clone()), 8).await
        else {
            panic!("oversized response must pass through");
        };
        let body = axum::body::to_bytes(body, 1024)
            .await
            .expect("pass-through body");
        assert_eq!(body, original);
    }
}
