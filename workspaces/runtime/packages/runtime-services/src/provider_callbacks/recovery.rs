//! Invoked by the account owner when a connection is lost or on startup.
use super::{CallbackError, cards, storage, tools};
use crate::tool_requests::{admission, lifecycle};
use serde_json::Value;
use serde_json::json;
use std::path::Path;
use studio_runtime_store::{RuntimeStore, Transaction, system_reads as records};

/// Return only unknown tool executions that still need exact native item
/// evidence before context repair. Payload references are resolved with the
/// same content-addressed store used by callback recovery.
pub fn unresolved_context_tool_receipts(
    tx: &Transaction<'_>,
    state_root: &Path,
    agent: &str,
    account: &str,
    thread: &str,
    account_history: &[Value],
) -> Result<Vec<Value>, CallbackError> {
    let history = account_history
        .iter()
        .filter_map(|entry| {
            Some((
                entry
                    .get("accountKey")?
                    .as_str()
                    .unwrap_or("default")
                    .to_owned(),
                entry.get("threadId")?.as_str()?.to_owned(),
            ))
        })
        .collect::<std::collections::BTreeSet<_>>();
    let mut unresolved = Vec::new();
    for raw in records::all(tx, "runtime_tool_requests")? {
        if raw["agent"] != agent || raw["outcome"] != "unknown" {
            continue;
        }
        let record = crate::payload_blobs::resolve_record(state_root, &raw)
            .map_err(|error| CallbackError::Stored(error.to_string()))?;
        if matches!(record["stage"].as_str(), Some("completed" | "failed"))
            && record.get("finished").is_some_and(|value| !value.is_null())
            && record["result"]["success"].is_boolean()
        {
            continue;
        }
        let id = record["id"]
            .as_str()
            .ok_or_else(|| CallbackError::Stored("tool request id".into()))?;
        if let Some(cached) = records::get(tx, "runtime_tool_results", id)? {
            let cached = crate::payload_blobs::resolve_record(state_root, &cached)
                .map_err(|error| CallbackError::Stored(error.to_string()))?;
            if cached["success"].is_boolean() {
                continue;
            }
        }
        let receipt_scope = (
            record["accountKey"].as_str().unwrap_or("default"),
            record["threadId"].as_str().unwrap_or_default(),
        );
        if receipt_scope != (account, thread) {
            if history.contains(&(receipt_scope.0.to_owned(), receipt_scope.1.to_owned()))
                && matches!(
                    record["stage"].as_str(),
                    Some("completed" | "failed" | "interrupted")
                )
            {
                continue;
            }
            return Err(CallbackError::Unknown);
        }
        unresolved.push(record);
    }
    Ok(unresolved)
}

/// Safe to repeat after interruption. A surviving transport still requires its
/// own generation and write-receipt proof; this never schedules a response.
pub fn recover_connection(
    store: &RuntimeStore,
    account: &str,
    connection: &str,
    now: f64,
) -> Result<usize, CallbackError> {
    if !now.is_finite() || now < 0.0 {
        return Err(CallbackError::Invalid("Invalid callback clock"));
    }
    let cards = cards::expire(store, account, connection)?;
    store.transaction_with(|tx| {
        let mut count = cards;
        for mut record in records::all(tx, "runtime_tool_requests")? {
            if record["accountKey"].as_str().unwrap_or("default") != account
                || record["connectionId"] != connection
                || !matches!(record["stage"].as_str(), Some("queued" | "running"))
            {
                continue;
            }
            if let Some(result) = records::get(tx, "runtime_tool_results", record["id"].as_str().ok_or(CallbackError::Stored("tool request id".into()))?)? {
                // Existing results are evidence, not permission to run again.
                lifecycle::settle(&mut record, &result, false)?;
                record["result"] = result;
            } else {
                admission::restart(&mut record)?;
                if record["stage"] == "cancelled" {
                    record["error"] = json!("Server restarted before execution");
                    record["result"] = super::tool_result(json!({
                        "requestId":record["id"], "stage":"cancelled", "outcome":"not_applied",
                        "cancelRequested":record["cancelRequested"], "message":record["error"]
                    }), false);
                } else {
                    record["stage"] = json!("interrupted");
                    record["error"] = json!("Server restarted before the execution receipt");
                }
            }
            record["updated"] = json!(now);
            tools::put(tx, &record)?;
            count += 1;
        }
        let mut statement = tx.prepare("SELECT id FROM runtime_operation_receipts WHERE json_extract(result,'$.identity.context.account')=?1 AND json_extract(result,'$.identity.context.connection')=?2 AND json_extract(result,'$.delivery')='submitted'")?;
        let ids = statement.query_map([account, connection], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        for id in ids {
            let mut reply = storage::read(tx, &id)?.ok_or(CallbackError::Unknown)?;
            reply["delivery"] = json!("unknown");
            storage::update(tx, &id, &reply)?;
            count += 1;
        }
        Ok(count)
    })
}
