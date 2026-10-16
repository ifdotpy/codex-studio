//! Invoked by the account owner when a connection is lost or on startup.
use super::{CallbackError, cards, storage, tools};
use crate::tool_requests::{admission, lifecycle};
use serde_json::Value;
use serde_json::json;
use std::{collections::BTreeMap, path::Path};
use studio_runtime_store::{Connection, RuntimeStore, Transaction, system_reads as records};

/// A read-only snapshot of unresolved tool receipts. Filesystem-backed payloads
/// are resolved before the caller opens its write transaction; the raw database
/// identities are rechecked in that transaction before this snapshot is used.
#[derive(Clone, Debug)]
pub struct ContextToolReceiptSnapshot {
    requests: BTreeMap<String, Value>,
    results: BTreeMap<String, Option<Value>>,
    unresolved: Vec<Value>,
}

impl ContextToolReceiptSnapshot {
    pub fn empty() -> Self {
        Self {
            requests: BTreeMap::new(),
            results: BTreeMap::new(),
            unresolved: Vec::new(),
        }
    }
}

/// Read unknown tool executions and resolve payload blobs outside a write
/// transaction. The returned raw records are the identities later revalidated.
pub fn prepare_unresolved_context_tool_receipts(
    connection: &Connection,
    state_root: &Path,
    agent: &str,
    account: &str,
    thread: &str,
    account_history: &[Value],
) -> Result<ContextToolReceiptSnapshot, CallbackError> {
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
    let mut snapshot = ContextToolReceiptSnapshot::empty();
    for raw in records::all(connection, "runtime_tool_requests")? {
        if raw["agent"] != agent || raw["outcome"] != "unknown" {
            continue;
        }
        let id = raw["id"]
            .as_str()
            .ok_or_else(|| CallbackError::Stored("tool request id".into()))?
            .to_owned();
        let record = crate::payload_blobs::resolve_record(state_root, &raw)
            .map_err(|error| CallbackError::Stored(error.to_string()))?;
        snapshot.requests.insert(id.clone(), raw);
        if matches!(record["stage"].as_str(), Some("completed" | "failed"))
            && record.get("finished").is_some_and(|value| !value.is_null())
            && record["result"]["success"].is_boolean()
        {
            continue;
        }
        let cached_raw = records::get(connection, "runtime_tool_results", &id)?;
        let cached = cached_raw
            .as_ref()
            .map(|cached| crate::payload_blobs::resolve_record(state_root, cached))
            .transpose()
            .map_err(|error| CallbackError::Stored(error.to_string()))?;
        snapshot.results.insert(id.clone(), cached_raw);
        if let Some(cached) = cached
            && cached["success"].is_boolean()
        {
            continue;
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
        snapshot.unresolved.push(record);
    }
    Ok(snapshot)
}

/// Recheck every database row used to resolve a tool receipt. This function is
/// database-only and is safe to call while the context-repair write transaction
/// is open.
pub fn revalidate_unresolved_context_tool_receipts(
    tx: &Transaction<'_>,
    agent: &str,
    snapshot: &ContextToolReceiptSnapshot,
) -> Result<Vec<Value>, CallbackError> {
    let current_requests = records::all(tx, "runtime_tool_requests")?
        .into_iter()
        .filter(|record| record["agent"] == agent && record["outcome"] == "unknown")
        .map(|record| {
            let id = record["id"]
                .as_str()
                .ok_or_else(|| CallbackError::Stored("tool request id".into()))?
                .to_owned();
            Ok((id, record))
        })
        .collect::<Result<BTreeMap<_, _>, CallbackError>>()?;
    if current_requests != snapshot.requests {
        return Err(CallbackError::Stale);
    }
    for (id, previous) in &snapshot.results {
        if records::get(tx, "runtime_tool_results", id)? != *previous {
            return Err(CallbackError::Stale);
        }
    }
    Ok(snapshot.unresolved.clone())
}

/// Safe to repeat after interruption. A surviving transport still requires its
/// own generation and write-receipt proof; this never schedules a response.
pub async fn recover_connection(
    store: &RuntimeStore,
    account: &str,
    connection: &str,
    now: f64,
) -> Result<usize, CallbackError> {
    if !now.is_finite() || now < 0.0 {
        return Err(CallbackError::Invalid("Invalid callback clock"));
    }
    let cards = cards::expire(store, account, connection).await?;
    let account = account.to_owned();
    let connection = connection.to_owned();
    store.transaction_with(move |tx| {
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
        let mut statement = tx.prepare_cached("SELECT id FROM runtime_operation_receipts WHERE json_extract(result,'$.identity.context.account')=?1 AND json_extract(result,'$.identity.context.connection')=?2 AND json_extract(result,'$.delivery')='submitted'")?;
        let ids = statement.query_map([account, connection], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        for id in ids {
            let mut reply = storage::read(tx, &id)?.ok_or(CallbackError::Unknown)?;
            reply["delivery"] = json!("unknown");
            storage::update(tx, &id, &reply)?;
            count += 1;
        }
        Ok(count)
    }).await
}

#[cfg(test)]
mod context_tool_receipt_snapshot_tests {
    use super::{
        prepare_unresolved_context_tool_receipts, revalidate_unresolved_context_tool_receipts,
    };
    use crate::payload_blobs::{decide_externalize, write_blobs};
    use serde_json::{Value, json};
    use studio_runtime_store::RuntimeStore;

    fn externalized_request(text: char) -> (Value, Vec<crate::payload_blobs::BlobWrite>) {
        let record = json!({
            "id":"tool-request",
            "agent":"agent",
            "outcome":"unknown",
            "stage":"running",
            "accountKey":"default",
            "threadId":"thread",
            "result":{"text":text.to_string().repeat(70_000)}
        });
        let decision =
            decide_externalize("tool_requests", &record).expect("externalize tool payload");
        assert!(decision.record.get("_payloadBlobs").is_some());
        (decision.record, decision.writes)
    }

    #[test]
    fn changed_blob_reference_after_resolution_is_detected_before_repair_commit() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = RuntimeStore::open(Some(directory.path())).expect("open runtime store");
        let (before, before_blobs) = externalized_request('a');
        write_blobs(store.root(), &before_blobs).expect("write first payload blob");
        store
            .transaction_with::<_, crate::provider_callbacks::CallbackError>(|tx| {
                let raw = before.to_string();
                tx.execute(
                    "INSERT INTO runtime_tool_requests(id,record) VALUES(?1,?2)",
                    ["tool-request", raw.as_str()],
                )?;
                Ok(())
            })
            .expect("insert first blob-backed tool request");

        let connection = store.read().expect("open read connection");
        let prepared = prepare_unresolved_context_tool_receipts(
            &connection,
            store.root(),
            "agent",
            "default",
            "thread",
            &[],
        )
        .expect("resolve payload before write transaction");
        drop(connection);
        assert_eq!(
            prepared.unresolved[0]["result"]["text"]
                .as_str()
                .expect("fixture result text is a string")
                .len(),
            70_000
        );

        let (after, after_blobs) = externalized_request('b');
        assert_ne!(before["_payloadBlobs"], after["_payloadBlobs"]);
        write_blobs(store.root(), &after_blobs).expect("write replacement payload blob");
        store
            .transaction_with::<_, crate::provider_callbacks::CallbackError>(|tx| {
                let raw = after.to_string();
                tx.execute(
                    "UPDATE runtime_tool_requests SET record=?1 WHERE id=?2",
                    [raw.as_str(), "tool-request"],
                )?;
                Ok(())
            })
            .expect("replace blob reference after preflight read");

        let validation = store.transaction_with(|tx| {
            revalidate_unresolved_context_tool_receipts(tx, "agent", &prepared)
        });
        assert!(matches!(
            validation,
            Err(crate::provider_callbacks::CallbackError::Stale)
        ));
    }
}
