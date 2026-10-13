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
pub fn push_drafts(
    store: &RuntimeStore,
    rows: &[DraftWrite],
    validate: impl Fn(&Value) -> bool,
) -> Result<DraftPushResult, DraftPushError> {
    store
        .transaction(|transaction| {
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
                    let value = serde_json::from_str::<Value>(&current.payload).map_err(|_| {
                        StoreError::InvalidSyncObject {
                            scope: "drafts".to_owned(),
                            id: row.id.clone(),
                        }
                    })?;
                    if !validate(&value)
                        || crate::sync_policy::validate_draft(&row.id, &value).is_err()
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
        })
        .map_err(|error| match error {
            StoreError::InvalidSyncObject { scope, id } if scope == "drafts" => {
                DraftPushError::InvalidStored { id }
            }
            error => DraftPushError::Store(error),
        })
}

/// Bound mutation deltas and advertise a watermark only when no rows were lost.
pub fn with_entity_delta(
    connection: &rusqlite::Connection,
    after: i64,
    mut result: Value,
) -> Result<Value, StoreError> {
    const LIMIT: usize = 500;
    let rows = SyncDocumentsRepository::entities_after(connection, after, (LIMIT + 1) as i64)?;
    if !rows.is_empty() {
        result["_syncEntities"] = Value::Array(
            rows.iter()
                .take(LIMIT)
                .map(|row| {
                    serde_json::json!({
                        "id": format!("entity:{}:{}", row.collection, row.id), "seq": row.sequence,
                        "payload": row.payload, "_deleted": row.deleted
                    })
                })
                .collect(),
        );
        if rows.len() <= LIMIT
            && SyncDocumentsRepository::entity_tombstone_floor(connection)? <= after
        {
            result["_syncEntitiesAfter"] = serde_json::json!(after);
        }
    }
    Ok(result)
}
