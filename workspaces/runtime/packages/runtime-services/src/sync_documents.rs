//! Transactional publication of browser-owned sync documents.

use serde_json::Value;
use studio_runtime_store::{RuntimeStore, StoreError, SyncDocument, SyncDocumentsRepository};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq)]
pub struct DraftWrite {
    pub id: String,
    pub payload: Value,
    pub deleted: bool,
    pub assumed_payload: Option<Value>,
    pub assumed_deleted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DraftPushResult {
    pub conflicts: Vec<SyncDocument>,
    pub changed: bool,
}

#[derive(Debug, Error)]
pub enum DraftPushError {
    #[error("invalid stored sync object drafts/{id}")]
    InvalidStored { id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Apply a batch atomically, returning current masters for conflicts.
pub async fn push_drafts(
    store: &RuntimeStore,
    rows: Vec<DraftWrite>,
    post_commit: impl FnOnce() + Send + 'static,
) -> Result<DraftPushResult, DraftPushError> {
    let (result, _) = store
        .transaction_with_post_commit(
            move |transaction| {
                let mut conflicts = Vec::new();
                let mut changed = false;
                for row in rows {
                    let current = SyncDocumentsRepository::draft(transaction, &row.id)?;
                    let current_value = if let Some(current) = &current {
                        if crate::sync_policy::contains_unrepresentable_integer(&current.payload) {
                            return Err(StoreError::InvalidSyncObject {
                                scope: "drafts".to_owned(),
                                id: row.id.clone(),
                            });
                        }
                        let value =
                            serde_json::from_str::<Value>(&current.payload).map_err(|_| {
                                StoreError::InvalidSyncObject {
                                    scope: "drafts".to_owned(),
                                    id: row.id.clone(),
                                }
                            })?;
                        if studio_runtime_schema::drafts::DraftVersion::from_stored_value(
                            &row.id,
                            value.clone(),
                        )
                        .is_err()
                        {
                            return Err(StoreError::InvalidSyncObject {
                                scope: "drafts".to_owned(),
                                id: row.id.clone(),
                            });
                        }
                        Some(value)
                    } else {
                        None
                    };

                    if let (Some(current), Some(current_value)) = (&current, &current_value) {
                        let same = row.payload == *current_value && row.deleted == current.deleted;
                        let assumed_matches = row.assumed_payload.as_ref() == Some(current_value)
                            && row.assumed_deleted == current.deleted;
                        if !assumed_matches && !same {
                            conflicts.push(current.clone());
                            continue;
                        }
                        if same {
                            continue;
                        }
                    }

                    let payload = serde_json::to_string(&row.payload).map_err(StoreError::from)?;
                    changed |= SyncDocumentsRepository::put_draft(
                        transaction,
                        &row.id,
                        &payload,
                        row.deleted,
                    )?;
                }
                Ok(DraftPushResult { conflicts, changed })
            },
            move |result| {
                if result.changed {
                    post_commit();
                }
            },
        )
        .await
        .map_err(|error| match error {
            StoreError::InvalidSyncObject { scope, id } if scope == "drafts" => {
                DraftPushError::InvalidStored { id }
            }
            error => DraftPushError::Store(error),
        })?;
    Ok(result)
}

/// Bound mutation deltas and advertise a watermark only when no rows were lost.
pub fn with_entity_delta(
    connection: &rusqlite::Connection,
    after: i64,
    mut result: Value,
) -> Result<Value, StoreError> {
    const LIMIT: usize = studio_runtime_schema::shared_values::SYNC_ENTITY_PAGE_LIMIT as usize;
    let rows = SyncDocumentsRepository::entities_after(connection, after, (LIMIT + 1) as i64)?;
    if !rows.is_empty() {
        result["_syncEntities"] =
            Value::Array(rows.iter().take(LIMIT).filter_map(entity_change).collect());
        if rows.len() <= LIMIT
            && SyncDocumentsRepository::entity_tombstone_floor(connection)? <= after
        {
            result["_syncEntitiesAfter"] = serde_json::json!(after);
        }
    }
    Ok(result)
}

/// Normalize a historical stored entity envelope without serializing it again.
/// Pull, stream, and every mutation envelope share this read boundary.
pub fn entity_payload(row: &studio_runtime_store::EntityRow) -> Option<Value> {
    let mut payload: Value = serde_json::from_str(row.payload.as_deref()?).ok()?;
    if !matches!(
        studio_runtime_schema::normalize_sync_entity_payload(
            &row.collection,
            &row.id,
            row.deleted,
            &mut payload
        ),
        Ok(None)
    ) {
        return None;
    }
    Some(payload)
}

/// Reuse the parsed envelope map when a mutation response is already a Value.
pub fn entity_change(row: &studio_runtime_store::EntityRow) -> Option<Value> {
    let mut payload = entity_payload(row)?;
    let object = payload.as_object_mut()?;
    if row.deleted {
        object.remove("value");
    }
    object.insert(
        "op".to_owned(),
        Value::String(if row.deleted { "delete" } else { "upsert" }.to_owned()),
    );
    object.insert("seq".to_owned(), Value::from(row.sequence));
    Some(payload)
}
