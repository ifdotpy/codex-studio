//! Transactional publication of browser-owned sync documents.

use studio_runtime_store::{RuntimeStore, StoreError, SyncDocument, SyncDocumentsRepository};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DraftWrite {
    pub id: String,
    pub payload: String,
    pub deleted: bool,
    pub assumed_payload: Option<String>,
    pub assumed_deleted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DraftPushResult {
    pub conflicts: Vec<SyncDocument>,
    pub changed: bool,
}

/// Apply a batch atomically, returning the current master rows for conflicts.
pub fn push_drafts(
    store: &RuntimeStore,
    rows: &[DraftWrite],
) -> Result<DraftPushResult, StoreError> {
    store.transaction(|transaction| {
        let mut conflicts = Vec::new();
        let mut changed = false;
        for row in rows {
            let current = SyncDocumentsRepository::draft(transaction, &row.id)?;
            if let Some(current) = current {
                let assumed_matches = row.assumed_payload.as_deref()
                    == Some(current.payload.as_str())
                    && row.assumed_deleted == current.deleted;
                if !assumed_matches
                    && (row.payload != current.payload || row.deleted != current.deleted)
                {
                    conflicts.push(current);
                    continue;
                }
            }
            changed |= SyncDocumentsRepository::put_draft(
                transaction,
                &row.id,
                &row.payload,
                row.deleted,
            )?;
        }
        Ok(DraftPushResult { conflicts, changed })
    })
}
