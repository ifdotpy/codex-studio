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
    rows: Vec<EntityRow>,
    sync_after: Option<i64>,
    page_limit: usize,
) -> Value {
    if !rows.is_empty() {
        result["_syncEntities"] = Value::Array(
            rows.iter()
                .take(page_limit)
                .map(|row| {
                    json!({
                        "id": format!("entity:{}:{}", row.collection, row.id),
                        "seq": row.sequence,
                        "payload": row.payload,
                        "_deleted": row.deleted,
                    })
                })
                .collect(),
        );
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
