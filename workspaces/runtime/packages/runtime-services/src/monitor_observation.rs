//! Read-only bounded monitor observation. A wait never renews a command lease.
use crate::monitor_policy::MonitorError;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use studio_runtime_store::{RuntimeStore, StoreError};

pub(crate) async fn read(
    store: &RuntimeStore,
    owner: &str,
    epoch: i64,
    account: &str,
    id: &str,
) -> Result<(Value, u64, bool), MonitorError> {
    let (owner, account, id) = (owner.to_owned(), account.to_owned(), id.to_owned());
    // One read transaction keeps monitor, owner, and output events coherent.
    store
        .read_async("runtime-services/monitor_observation", move |tx| {
            read_in(tx, &owner, epoch, &account, &id)
        })
        .await
}

fn read_in(
    tx: &rusqlite::Connection,
    owner: &str,
    epoch: i64,
    account: &str,
    id: &str,
) -> Result<(Value, u64, bool), MonitorError> {
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
    // The authenticated actor uses its current model account. Its existing
    // monitor output remains owned by that actor and epoch after a transfer.
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transferred_monitor_output_belongs_to_its_current_actor_not_its_old_model_account() {
        let db = rusqlite::Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT);CREATE TABLE runtime_monitors(id TEXT PRIMARY KEY,record TEXT);CREATE TABLE runtime_events(id TEXT,agent TEXT,status TEXT,text TEXT);").expect("schema");
        db.execute(
            "INSERT INTO runtime_agents VALUES('owner',?1)",
            [json!({"id":"owner","epoch":1,"autoWake":true,"accountKey":"target"}).to_string()],
        )
        .expect("transferred owner");
        db.execute("INSERT INTO runtime_monitors VALUES('watch',?1)",[json!({"id":"watch","agent":"owner","epoch":1,"status":"running","tail":"kept output","operation":{"agent":"owner","epoch":1,"accountKey":"source","connectionId":"original"}}).to_string()]).expect("original source monitor");
        let (record, _, terminal) = read_in(&db, "owner", 1, "target", "watch")
            .expect("read the existing process after transfer");
        assert_eq!(record["tail"], "kept output");
        assert!(!terminal);
        for (owner, epoch, account) in [
            ("other", 1, "target"),
            ("owner", 2, "target"),
            ("owner", 1, "source"),
        ] {
            assert!(read_in(&db, owner, epoch, account, "watch").is_err());
        }
    }
}
