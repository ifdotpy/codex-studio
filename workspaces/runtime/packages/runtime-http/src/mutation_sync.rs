//! Adds Python's sync entity range to successful object-shaped mutation replies.

use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use studio_runtime_store::{EntityRow, SyncDocumentsRepository};

use crate::error::ApiError;
use crate::router::HttpState;
use crate::validation::ValidatedRequest;

// Keep aligned with codex_sync_entities.MAX_MUTATION_SYNC_ENTITIES (line 43).
const MAX_MUTATION_SYNC_ENTITIES: i64 = 500;
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
            cursor_store
                .read_transaction(|snapshot| SyncDocumentsRepository::entity_sequence(snapshot))
        })
        .await;
        let cursor = match cursor {
            Ok(Ok(cursor)) => cursor,
            _ => return ApiError::internal().into_response(),
        };
        if cursor > MAX_SAFE_ENTITY_SEQUENCE {
            return ApiError::internal().into_response();
        }
        request.extensions_mut().insert(EntityCursor(cursor));
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

    let (parts, body) = response.into_parts();
    let bytes = match to_bytes(body, usize::MAX).await {
        Ok(bytes) => bytes,
        Err(_) => return ApiError::internal().into_response(),
    };
    let Ok(mut value) = serde_json::from_slice::<Value>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    let Some(object) = value.as_object_mut() else {
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
        _ => return ApiError::internal().into_response(),
    };
    if delta
        .rows
        .iter()
        .any(|row| row.sequence > MAX_SAFE_ENTITY_SEQUENCE)
    {
        return ApiError::internal().into_response();
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
        return ApiError::internal().into_response();
    };
    object.insert("_syncEntities".to_owned(), Value::Array(entities));
    if complete {
        object.insert("_syncEntitiesAfter".to_owned(), json!(after));
    }
    let body = match serde_json::to_vec(&value) {
        Ok(body) => body,
        Err(_) => return ApiError::internal().into_response(),
    };
    let mut response = Response::from_parts(parts, Body::from(body.clone()));
    response.headers_mut().remove(header::CONTENT_LENGTH);
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

        let reader = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open read-only connection");
        reader
            .pragma_update(None, "query_only", "ON")
            .expect("set query-only mode");
        let delta = read_entity_delta(&reader, 5).expect("read delta with writer held");
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
}
