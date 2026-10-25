//! Compatibility for stored workspaces whose executor has been removed.
//! Keep the original row and workspace metadata. Never access its filesystem.
use serde_json::{Value, json};
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, StoreError};

pub fn is_legacy(record: &Value) -> bool {
    record["workspaceMode"] == "image" || record["imageWorkspace"] == true
}

/// Finish legacy agents without deleting their rows, transcript, or pending receipts.
pub async fn finish_stored_workspaces(store: &RuntimeStore) -> Result<usize, StoreError> {
    store.transaction(|tx| {
        let rows = {
            let mut statement = tx.prepare("SELECT record FROM runtime_agents WHERE (json_extract(record,'$.workspaceMode')='image' OR json_extract(record,'$.imageWorkspace')=1) AND COALESCE(json_extract(record,'$.legacyWorkspaceFinished'),0)=0")?;
            statement.query_map([], |row| row.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?
        };
        let mut count = 0;
        for row in rows {
            let mut value: Value = serde_json::from_str(&row).map_err(|_| StoreError::InvalidStoredObject { table: "runtime_agents", id: "legacy".into() })?;
            if !matches!(value["status"].as_str(), Some("archived" | "deleted")) {
                value["status"] = json!("completed");
            }
            value["autoWake"] = json!(false);
            value["inFlight"] = json!(false);
            value["legacyWorkspaceFinished"] = json!(true);
            let id = value["id"].as_str().unwrap_or("legacy").to_owned();
            let record = AgentRecord::from_value(value).map_err(|_| StoreError::InvalidStoredObject { table: "runtime_agents", id })?;
            AgentRepository::put(tx, &record)?;
            count += 1;
        }
        Ok(count)
    }).await
}
