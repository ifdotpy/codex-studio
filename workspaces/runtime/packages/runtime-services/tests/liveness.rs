//! Liveness detection must never change a chat or its pending input.
use serde_json::json;
use std::sync::Arc;
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore};

async fn seeded(now: f64) -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let root = tempfile::tempdir().expect("liveness state");
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
    let path = root.path().to_path_buf();
    store
        .transaction(move |tx| {
            for (id, fields) in [
                ("idle", json!({})),
                ("stopped", json!({"status":"paused","autoWake":false})),
                ("held", json!({"status":"waiting","nativeFailureHold":true})),
                ("unknown", json!({"startOutcomeHold":{"reason":"unknown"}})),
                ("codex", json!({"status":"running","inFlight":true,"turnId":"codex-turn",
                    "created":now - 1000.0,"activity":{"at":now - 700.0}})),
                ("claude", json!({"status":"running","provider":"claude","inFlight":true,
                    "turnId":"claude-turn","created":now - 1000.0,"activity":{"at":now - 700.0}})),
                ("session", json!({"status":"queued","startWait":{"reason":"provider_session","epoch":0}})),
                ("resume", json!({"status":"waiting","usageResume":{"id":"resume"}})),
                ("capacity", json!({"status":"waiting","capacityRetry":{"id":"capacity"}})),
                ("transfer", json!({"accountTransferId":"transfer"})),
            ] {
                let mut record = json!({"id":id,"rootId":id,"epoch":0,"status":"completed",
                    "autoWake":true,"cwd":path.to_string_lossy(),"accountKey":"default",
                    "provider":"codex","name":id});
                for (key, value) in fields.as_object().expect("fields") {
                    record[key] = value.clone();
                }
                AgentRepository::put(tx, &AgentRecord::from_value(record)?)?;
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                     VALUES (?1,?2,'user','continue','pending',?3,0)",
                    rusqlite::params![format!("{id}-message"), id, now - 180.0],
                )?;
            }
            tx.execute("INSERT INTO runtime_usage_resumes(id,agent,record) VALUES ('resume','resume',?1)",
                [json!({"id":"resume","status":"scheduled","dueAt":now - 1000.0}).to_string()])?;
            tx.execute("INSERT INTO runtime_capacity_retries(id,agent,record) VALUES ('capacity','capacity',?1)",
                [json!({"id":"capacity","status":"scheduled","dueAt":now - 1000.0}).to_string()])?;
            tx.execute("INSERT INTO runtime_account_transfers(id,record) VALUES ('transfer',?1)",
                [json!({"id":"transfer","leadId":"transfer","status":"pending","created":now - 1000.0,
                    "members":{"transfer":{"phase":"blocked","error":"catalog unavailable"}}}).to_string()])?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("seed");
    (root, store)
}

async fn stored_state(store: &RuntimeStore) -> Vec<String> {
    store
        .read_async("tests/liveness-state", |tx| {
            let mut result = Vec::new();
            for table in [
                "runtime_agents",
                "runtime_events",
                "runtime_operation_receipts",
                "runtime_usage_resumes",
                "runtime_capacity_retries",
                "runtime_account_transfers",
            ] {
                let mut statement = tx.prepare(&format!("SELECT * FROM {table} ORDER BY id"))?;
                let columns = statement.column_count();
                let rows = statement
                    .query_map([], |row| {
                        Ok((0..columns)
                            .map(|column| format!("{:?}", row.get_ref(column)))
                            .collect::<Vec<_>>()
                            .join("|"))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                result.push(format!("{table}: {rows:?}"));
            }
            Ok::<_, studio_runtime_store::StoreError>(result)
        })
        .await
        .expect("stored state")
}

#[tokio::test]
async fn detection_never_wakes_an_idle_chat_or_changes_held_input() {
    let now = studio_runtime_services::event_write::unix_time().expect("clock");
    let (_root, store) = seeded(now).await;
    let before = stored_state(&store).await;
    for at in [now, now + 120.0] {
        let list = store
            .read_async("tests/liveness", move |tx| {
                studio_runtime_services::studio_errors::list(tx, at)
            })
            .await
            .expect("Studio errors");
        let rows = list["items"].as_array().expect("error rows");
        for (agent, kind) in [
            ("idle", "input_not_started"),
            ("stopped", "input_not_started"),
            ("held", "input_not_started"),
            ("unknown", "input_not_started"),
            ("codex", "turn_stalled"),
            ("claude", "turn_stalled"),
            ("session", "session_not_started"),
            ("resume", "schedule_stalled"),
            ("capacity", "schedule_stalled"),
            ("transfer", "schedule_stalled"),
        ] {
            let row = rows
                .iter()
                .find(|row| row["agentId"] == agent && row["kind"] == kind)
                .unwrap_or_else(|| panic!("missing {agent}/{kind}: {list}"));
            assert_eq!(row["action"], "check", "{row}");
            for label in ["Account:", "Start:", "Last provider event:"] {
                assert!(
                    row["detail"].as_str().expect("detail").contains(label),
                    "{row}"
                );
            }
        }
        assert_eq!(
            stored_state(&store).await,
            before,
            "detection changes no stored state"
        );
    }
}
