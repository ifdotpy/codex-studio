//! Compatibility for stored workspaces whose executor has been removed.
//! Keep the original row and workspace metadata. Never access its filesystem.
use serde_json::{Value, json};
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, StoreError};

pub fn is_legacy(record: &Value) -> bool {
    record["workspaceMode"] == "image" || record["imageWorkspace"] == true
}

/// Finish legacy agents without deleting their rows, transcript, or pending receipts.
const EXECUTION_FIELDS: &[&str] = &[
    "turnId",
    "startAttempt",
    "restartRecovery",
    "disconnectRecovery",
    "connectionRecovery",
];

pub async fn finish_stored_workspaces(store: &RuntimeStore) -> Result<usize, StoreError> {
    let at = crate::event_write::unix_time()
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
    store.transaction(move |tx| {
        let rows = {
            let mut statement = tx.prepare("SELECT record FROM runtime_agents WHERE (json_extract(record,'$.workspaceMode')='image' OR json_extract(record,'$.imageWorkspace')=1) AND COALESCE(json_extract(record,'$.legacyWorkspaceSettled'),0)=0")?;
            statement.query_map([], |row| row.get::<_,String>(0))?.collect::<Result<Vec<_>,_>>()?
        };
        let mut count = 0;
        for row in rows {
            let mut value: Value = serde_json::from_str(&row).map_err(|_| StoreError::InvalidStoredObject { table: "runtime_agents", id: "legacy".into() })?;
            let mut execution = value.get("legacyWorkspaceExecution").and_then(Value::as_object).cloned().unwrap_or_default();
            for field in EXECUTION_FIELDS {
                if let Some(saved) = value.get(*field).filter(|saved| !saved.is_null()).cloned() {
                    execution.entry((*field).to_owned()).or_insert(saved);
                    value[*field] = Value::Null;
                }
            }
            if !execution.is_empty() { value["legacyWorkspaceExecution"] = Value::Object(execution); }
            if !matches!(value["status"].as_str(), Some("archived" | "deleted")) {
                value["status"] = json!("completed");
            }
            value["autoWake"] = json!(false);
            value["inFlight"] = json!(false);
            value["legacyWorkspaceFinished"] = json!(true);
            value["legacyWorkspaceSettled"] = json!(true);
            let id = value["id"].as_str().unwrap_or("legacy").to_owned();
            let record = AgentRecord::from_value(value).map_err(|_| StoreError::InvalidStoredObject { table: "runtime_agents", id: id.clone() })?;
            AgentRepository::put(tx, &record)?;
            crate::task_settlement::settle_agent_tasks(tx, &id, at)?;
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
            if EXECUTION_FIELDS.contains(&key.as_str()) && !value.is_null() {
                assert_eq!(
                    &saved["legacyWorkspaceExecution"][key], value,
                    "{key} retains its original receipt"
                );
                assert!(
                    saved[key].is_null(),
                    "{key} no longer references a live turn"
                );
            } else {
                assert_eq!(saved.get(key), Some(value), "{key} must remain stored");
            }
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
    #[tokio::test]
    async fn review_regression_legacy_finish_settles_turn_tasks() {
        let directory = tempfile::tempdir().expect("fixture operation succeeds");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("fixture operation succeeds");
        store.transaction(|tx| {
            let agent = json!({"id":"legacy-worker", "rootId":"lead", "parentId":"lead", "epoch":1,
                "workspaceMode":"image", "status":"running", "autoWake":true, "inFlight":true,
                "turnId":"old-turn", "startAttempt":{"turnId":"old-turn"},
                "restartRecovery":{"turnId":"old-turn","stage":"pending"}});
            AgentRepository::put(tx, &AgentRecord::from_value(agent)?)?;
            let task = json!({"id":"legacy-task", "agent":"legacy-worker", "itemId":"item",
                "turnId":"old-turn", "created":1.0, "kind":"command", "status":"running",
                "type":"commandExecution", "name":"commandExecution"});
            crate::notification_dispatch::put_task_record(tx, &task).expect("fixture operation succeeds");
            let mut background=task;
            background["id"]=json!("legacy-background"); background["processId"]=json!("old-process");
            crate::notification_dispatch::put_task_record(tx, &background).expect("fixture operation succeeds");
            Ok(())
        }).await.expect("fixture operation succeeds");
        finish_stored_workspaces(&store)
            .await
            .expect("fixture operation succeeds");
        let db = store.test_reader().expect("fixture operation succeeds");
        let agent = AgentRepository::get(&db, &AgentId::from("legacy-worker"))
            .expect("fixture operation succeeds")
            .expect("fixture operation succeeds")
            .value();
        assert!(
            !crate::task_settlement::turn_live(&agent, "old-turn", false),
            "{agent}"
        );
        let task: String = db
            .query_row(
                "SELECT record FROM runtime_tasks WHERE id='legacy-task'",
                [],
                |row| row.get(0),
            )
            .expect("fixture operation succeeds");
        assert_eq!(
            serde_json::from_str::<Value>(&task).expect("fixture operation succeeds")["status"],
            crate::task_settlement::SETTLED_TASK_STATUS
        );
        let background: String = db
            .query_row(
                "SELECT record FROM runtime_tasks WHERE id='legacy-background'",
                [],
                |row| row.get(0),
            )
            .expect("fixture operation succeeds");
        assert_eq!(
            serde_json::from_str::<Value>(&background).expect("fixture operation succeeds")["status"],
            crate::task_settlement::LOST_TASK_STATUS
        );
    }
}
