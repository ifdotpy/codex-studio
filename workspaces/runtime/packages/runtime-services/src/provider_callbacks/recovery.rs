//! Invoked by the account owner when a connection is lost or on startup.
use super::{CallbackError, cards, storage, tools};
use crate::tool_requests::{admission, lifecycle};
use serde_json::json;
use studio_runtime_store::{RuntimeStore, system_reads as records};

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
