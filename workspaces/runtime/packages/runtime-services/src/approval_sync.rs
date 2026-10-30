//! Publishes an approval row to the renderer in the transaction that changed
//! the row. Both entities carry the typed `Approval`.
//!
//! - `request`: an approval that waits for the person (Pending) or whose answer
//!   is on its way (Answering). Every other state publishes a deletion.
//! - `questionHistory`: every state of a question or agent question, so a
//!   transcript card shows the answer after the request left the list.
//!
//! A hidden row is published as deleted in both. An asynchronous question
//! also carries `detail.params.itemId`, the agent message item named by its id.
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
    let mut value = serde_json::to_value(row)?;
    // The transcript item that an asynchronous question belongs to is part of its id.
    if matches!(row.request, Request::AgentQuestion { .. })
        && let Some(item) =
            crate::questions_reads::async_question_item(row.agent.as_str(), row.id.as_str())
        && let Some(params) = value["detail"]["params"].as_object_mut()
    {
        params
            .entry("itemId")
            .or_insert_with(|| serde_json::Value::String(item.to_owned()));
    }
    SyncDocumentsRepository::put_entity(tx, "request", row.id.as_str(), value.clone(), !open)?;
    SyncDocumentsRepository::put_entity(tx, "questionHistory", row.id.as_str(), value, !asked)?;
    Ok(())
}
