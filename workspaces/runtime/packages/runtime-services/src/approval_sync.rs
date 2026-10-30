//! Publishes an approval row as the renderer's `request` sync entity, in the
//! transaction that changed the row. The entity is the typed `Approval`; only a
//! pending approval is shown, every other state publishes a deletion.
use rusqlite::Transaction;
use studio_runtime_schema::approvals::{Approval, ApprovalState};
use studio_runtime_store::{StoreError, SyncDocumentsRepository};

pub(crate) fn publish(tx: &Transaction<'_>, row: &Approval) -> Result<(), StoreError> {
    let shown = matches!(row.state, ApprovalState::Pending) && row.hidden.is_none();
    SyncDocumentsRepository::put_entity(
        tx,
        "request",
        row.id.as_str(),
        serde_json::to_value(row)?,
        !shown,
    )?;
    Ok(())
}
