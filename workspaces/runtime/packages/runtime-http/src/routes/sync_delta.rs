//! Shared post-write entity deltas for HTTP mutation responses.

use std::future::Future;
use std::sync::Arc;

use serde_json::{Value, json};
use studio_runtime_schema::shared_values::SYNC_ENTITY_PAGE_LIMIT;
use studio_runtime_store::{
    EntityRow, RuntimeStore, StoreError, SyncDocumentsRepository, Transaction,
};

use crate::error::ApiError;

pub(crate) async fn write<E>(
    store: Arc<RuntimeStore>,
    operation: impl FnOnce(&Transaction<'_>) -> Result<Value, E> + Send + 'static,
    map_error: impl FnOnce(E) -> ApiError + Send + 'static,
) -> Result<Value, ApiError>
where
    E: From<StoreError> + Send + 'static,
{
    let sequence_store = store.clone();
    let sequence = sequence_store
        .read_async("runtime-http/routes/sync_delta", |connection| {
            SyncDocumentsRepository::entity_sequence(connection).map_err(|_| ApiError::internal())
        })
        .await?;
    let page_limit = usize::try_from(SYNC_ENTITY_PAGE_LIMIT).map_err(|_| ApiError::internal())?;
    let fetch_limit = SYNC_ENTITY_PAGE_LIMIT
        .checked_add(1)
        .ok_or_else(ApiError::internal)?;
    let result = store
        .transaction_with_error(operation)
        .await
        .map_err(map_error)?;
    let read_store = Arc::clone(&store);
    let (rows, floor) = read_store
        .read_async("runtime-http/routes/sync_delta", move |connection| {
            let rows = SyncDocumentsRepository::entities_after(connection, sequence, fetch_limit)
                .map_err(|_| ApiError::internal())?;
            let floor = SyncDocumentsRepository::entity_tombstone_floor(connection)
                .map_err(|_| ApiError::internal())?;
            Ok::<_, ApiError>((rows, floor))
        })
        .await?;
    let sync_after = (rows.len() <= page_limit && floor <= sequence).then_some(sequence);
    Ok(attach(result, rows, sync_after, page_limit))
}

pub(crate) async fn write_external_async<E, F>(
    store: Arc<RuntimeStore>,
    operation: impl FnOnce() -> F + Send + 'static,
    map_error: impl FnOnce(E) -> ApiError,
) -> Result<Value, ApiError>
where
    E: From<StoreError>,
    F: Future<Output = Result<Value, E>> + Send,
{
    let sequence_store = Arc::clone(&store);
    let sequence = sequence_store
        .read_async("runtime-http/routes/sync_delta", |connection| {
            SyncDocumentsRepository::entity_sequence(connection).map_err(|_| ApiError::internal())
        })
        .await?;
    let page_limit = usize::try_from(SYNC_ENTITY_PAGE_LIMIT).map_err(|_| ApiError::internal())?;
    let fetch_limit = SYNC_ENTITY_PAGE_LIMIT
        .checked_add(1)
        .ok_or_else(ApiError::internal)?;
    let result = operation().await.map_err(map_error)?;
    let (rows, floor) = store
        .read_async("runtime-http/routes/sync_delta", move |connection| {
            let rows = SyncDocumentsRepository::entities_after(connection, sequence, fetch_limit)
                .map_err(|_| ApiError::internal())?;
            let floor = SyncDocumentsRepository::entity_tombstone_floor(connection)
                .map_err(|_| ApiError::internal())?;
            Ok::<_, ApiError>((rows, floor))
        })
        .await?;
    let sync_after = (rows.len() <= page_limit && floor <= sequence).then_some(sequence);
    Ok(attach(result, rows, sync_after, page_limit))
}

pub(crate) fn attach(
    mut result: Value,
    mut rows: Vec<EntityRow>,
    sync_after: Option<i64>,
    page_limit: usize,
) -> Value {
    // In-place coalescing avoids a second map/allocation and preserves sequence order.
    // Repository pages are already unique by primary key; callers may combine rows.
    rows.sort_unstable_by(|a, b| {
        (&a.collection, &a.id)
            .cmp(&(&b.collection, &b.id))
            .then_with(|| b.sequence.cmp(&a.sequence))
    });
    rows.dedup_by(|a, b| a.collection == b.collection && a.id == b.id);
    rows.sort_unstable_by_key(|row| row.sequence);
    if !rows.is_empty() {
        let documents = rows
            .iter()
            .take(page_limit)
            .filter_map(studio_runtime_services::sync_documents::entity_change)
            .collect();
        result["_syncEntities"] = Value::Array(documents);
        if let Some(sync_after) = sync_after {
            result["_syncEntitiesAfter"] = json!(sync_after);
        }
    }
    result
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "sync_delta",
        routes: None,
        dependencies: &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(sequence: i64, deleted: bool) -> EntityRow {
        EntityRow {
            collection: "workspace".into(),
            id: "current".into(),
            sequence,
            payload: Some(
                json!({"collection":"workspace","id":"current","value":{"stateDir":"temporary"}})
                    .to_string(),
            ),
            deleted,
        }
    }

    #[test]
    fn entity_v4_uses_structured_values_and_compact_deletes() {
        let value = attach(json!({}), vec![row(1, false)], Some(0), 500);
        assert_eq!(
            value["_syncEntities"][0],
            json!({"op":"upsert","collection":"workspace","id":"current","seq":1,"value":{"stateDir":"temporary"}})
        );
        let value = attach(json!({}), vec![row(2, true)], Some(1), 500);
        assert_eq!(
            value["_syncEntities"][0],
            json!({"op":"delete","collection":"workspace","id":"current","seq":2})
        );
    }

    #[test]
    fn entity_v4_coalesces_latest_change_per_key_before_limiting() {
        let value = attach(
            json!({}),
            vec![row(3, false), row(1, false), row(4, true), row(2, false)],
            Some(0),
            1,
        );
        assert_eq!(value["_syncEntities"].as_array().unwrap().len(), 1);
        assert_eq!(value["_syncEntities"][0]["seq"], 4);
        assert_eq!(value["_syncEntities"][0]["op"], "delete");
    }
    #[test]
    fn entity_v4_agent_batch_removes_json_in_json_and_measures_bytes() {
        let rows: Vec<_> = (0..32).map(|index| {
            let id = format!("agent-{index:03}");
            EntityRow {
                collection: "agent".into(), id: id.clone(), sequence: index + 1, deleted: false,
                payload: Some(json!({"collection":"agent","id":id,"value":{
                    "id":id,"name":format!("Worker {index}: \"sync\""),"status":"idle","source":"managed",
                    "provider":"codex","model":"gpt-6","project":"project-1","lastAnswer":"Inspect the sync protocol and preserve sequence ordering.",
                    "tail":"Structured values avoid escaped JSON on the wire."}}).to_string()),
            }
        }).collect();
        for row in &rows {
            let payload: Value = serde_json::from_str(row.payload.as_deref().unwrap()).unwrap();
            assert!(
                studio_runtime_schema::validate_sync_entity_payload_strict(
                    &row.collection,
                    &row.id,
                    row.deleted,
                    &payload
                )
                .unwrap()
            );
        }
        let old: Vec<_> = rows.iter().map(|row| json!({"id":format!("entity:{}:{}",row.collection,row.id),"seq":row.sequence,"payload":row.payload,"_deleted":row.deleted})).collect();
        let old_bytes = serde_json::to_vec(&old).unwrap().len();
        let new = attach(json!({}), rows, Some(0), 500);
        let new_bytes = serde_json::to_vec(&new["_syncEntities"]).unwrap().len();
        assert!(new_bytes < old_bytes);
        assert!(
            new["_syncEntities"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["value"].is_object() && row.get("payload").is_none())
        );
        println!(
            "entity_v4_agent_batch: agents=32 old_bytes={old_bytes} new_bytes={new_bytes} saved_bytes={} reduction_pct={:.2}",
            old_bytes - new_bytes,
            (old_bytes - new_bytes) as f64 * 100.0 / old_bytes as f64
        );
    }
}
