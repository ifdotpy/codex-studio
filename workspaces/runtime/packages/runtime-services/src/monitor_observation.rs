//! Read-only bounded monitor observation. A wait never renews a command lease.
use crate::monitor_policy::MonitorError;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use studio_runtime_store::{RuntimeStore, StoreError};

pub(crate) fn read(
    store: &RuntimeStore,
    owner: &str,
    epoch: i64,
    account: &str,
    id: &str,
) -> Result<(Value, u64, bool), MonitorError> {
    let connection = store.read()?;
    // One read transaction keeps monitor, owner, and output events coherent.
    let tx = connection
        .unchecked_transaction()
        .map_err(StoreError::from)?;
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_monitors WHERE id=?",
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::from)?;
    let mut record: Value =
        serde_json::from_str(&raw.ok_or(MonitorError::Refused("Unknown command watch"))?)
            .map_err(StoreError::from)?;
    if record["agent"] != owner || record["epoch"].as_i64() != Some(epoch) {
        return Err(MonitorError::Refused(
            "This monitor belongs to another agent or epoch",
        ));
    }
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?",
            [owner],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::from)?;
    let agent: Value = serde_json::from_str(&raw.ok_or(MonitorError::Refused("Unknown agent"))?)
        .map_err(StoreError::from)?;
    if studio_runtime_store::truthy(agent.get("deletedAt"))
        || agent["epoch"].as_i64() != Some(epoch)
        || agent["autoWake"] != true
    {
        return Err(MonitorError::Refused(
            "The monitor owner epoch is no longer active",
        ));
    }
    if agent["accountKey"].as_str().unwrap_or("default") != account {
        return Err(MonitorError::Refused(
            "The monitor belongs to another account",
        ));
    }
    if record["operation"].is_object()
        && record["operation"]["accountKey"]
            .as_str()
            .unwrap_or("default")
            != account
    {
        return Err(MonitorError::Refused("The monitor account changed"));
    }
    let sequence = record["outputEventSequence"].as_u64().unwrap_or(0);
    let terminal = !matches!(
        record["status"].as_str(),
        Some("approval" | "starting" | "running")
    );
    let mut events = Vec::new();
    for id in record["outputEventIds"].as_array().into_iter().flatten() {
        let id = id.as_str().ok_or(MonitorError::InvalidOutputReceipt(
            "invalid saved output event ID",
        ))?;
        let event: Option<(String, String)> = tx
            .query_row(
                "SELECT status,text FROM runtime_events WHERE id=? AND agent=?",
                [id, owner],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(StoreError::from)?;
        if let Some((status, text)) = event {
            let mut event: Value = serde_json::from_str(&text).map_err(StoreError::from)?;
            event["id"] = json!(id);
            event["status"] = json!(status);
            events.push(event);
        }
    }
    record["outputEvents"] = json!(events);
    record["waitExpired"] = json!(false);
    Ok((record, sequence, terminal))
}
