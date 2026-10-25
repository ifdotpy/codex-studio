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
            crate::agents_create::publish_agent(tx, &record.value())?;
            count += 1;
        }
        Ok(count)
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_runtime_store::AgentId;

    #[tokio::test]
    async fn startup_retains_legacy_rows_and_prompt_snapshots_without_an_executor() {
        let directory = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store");
        let original: Value = serde_json::from_str(include_str!(
            "../tests/startup_recovery/legacy-image-agent.json"
        ))
        .expect("legacy fixture");
        let record = AgentRecord::from_value(original.clone()).expect("legacy stored record");
        store.transaction(move |tx| {
            AgentRepository::put(tx, &record)?;
            tx.execute("INSERT INTO runtime_items(id,agent,record,created) VALUES('legacy-item','legacy-agent',?1,1.0)", [json!({"id":"legacy-item","agent":"legacy-agent","text":"saved transcript"}).to_string()])?;
            Ok(())
        }).await.expect("fixture rows");
        assert_eq!(
            finish_stored_workspaces(&store)
                .await
                .expect("legacy recovery"),
            1
        );
        assert_eq!(
            finish_stored_workspaces(&store)
                .await
                .expect("idempotent recovery"),
            0
        );
        let db = store.test_reader().expect("reader");
        let saved = AgentRepository::get(&db, &AgentId::from("legacy-agent"))
            .expect("lookup")
            .expect("row retained")
            .value();
        let mut expected = original;
        expected["status"] = json!("completed");
        expected["autoWake"] = json!(false);
        expected["inFlight"] = json!(false);
        expected["legacyWorkspaceFinished"] = json!(true);
        for (key, value) in expected.as_object().expect("record") {
            assert_eq!(saved.get(key), Some(value), "{key} must remain stored");
        }
        let count: i64 = db
            .query_row(
                "SELECT count(*) FROM runtime_items WHERE id='legacy-item'",
                [],
                |row| row.get(0),
            )
            .expect("transcript lookup");
        assert_eq!(count, 1);
    }
}
