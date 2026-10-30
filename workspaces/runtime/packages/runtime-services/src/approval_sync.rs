//! Publishes an approval row to the renderer in the transaction that changed
//! the row. Both entities carry the typed `Approval`.
//!
//! - `request`: an approval that waits for the person (Pending) or whose answer
//!   is on its way (Answering). Every other state publishes a deletion.
//! - `questionHistory`: every state of a question or agent question, so a
//!   transcript card shows the answer after the request left the list.
//!
//! A hidden row is published as deleted in both.
use rusqlite::Transaction;
use studio_runtime_schema::approvals::{Approval, ApprovalState, Request};
use studio_runtime_store::{StoreError, SyncDocumentsRepository};

pub(crate) fn publish(tx: &Transaction<'_>, row: &Approval) -> Result<(), StoreError> {
    let visible = row.hidden.is_none();
    let open = visible
        && matches!(
            row.state,
            ApprovalState::Pending | ApprovalState::Answering { .. }
        );
    let asked = visible
        && matches!(
            row.request,
            Request::Question { .. } | Request::AgentQuestion { .. }
        );
    let value = serde_json::to_value(row)?;
    SyncDocumentsRepository::put_entity(tx, "request", row.id.as_str(), value.clone(), !open)?;
    SyncDocumentsRepository::put_entity(tx, "questionHistory", row.id.as_str(), value, !asked)?;
    Ok(())
}
