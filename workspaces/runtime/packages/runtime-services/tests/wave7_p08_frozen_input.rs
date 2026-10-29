use serde_json::{Value, json};
use std::path::Path;
use studio_runtime_services::claude_input_recovery::runtime::freeze_start;
use studio_runtime_store::{RuntimeStore, StoreError};

fn fixtures() -> Vec<Value> {
    #[derive(serde::Deserialize)]
    struct Captured {
        case: Value,
        database: serde::de::IgnoredAny,
    }
    let captured: Vec<Captured> =
        crate::turn_fixtures::load(include_bytes!("wave7_p08_input.json"));
    captured
        .into_iter()
        .map(|value| {
            let _ = value.database;
            value.case
        })
        .collect()
}

async fn seeded(path: &Path, case: &Value) -> RuntimeStore {
    std::fs::create_dir_all(path).expect("fixture directory");
    let store = RuntimeStore::open(Some(path))
        .await
        .expect("fresh current store");
    let agent = studio_runtime_store::AgentRecord::from_value(case["agent"].clone())
        .expect("fixture agent");
    let attempt = case["attempt"].to_string();
    store.transaction(move |tx| {
        studio_runtime_store::AgentRepository::put(tx, &agent)?;
        tx.execute("INSERT INTO runtime_execution_runs(id,agent,account,epoch,thread,created,record) VALUES('run','agent','account',2,'thread',130,?1)", [r#"{"id":"run","agent":"agent","accountKey":"account","epoch":2,"threadId":"thread","created":130,"status":"unsent"}"#])?;
        tx.execute("INSERT INTO runtime_execution_attempts(id,run,record) VALUES('attempt','run',?1)", [attempt])?;
        for (id, text, created) in [("one", "original one", 100.0), ("two", "original two", 101.0)] {
            tx.execute("INSERT INTO runtime_events(id,agent,epoch,kind,text,created,status) VALUES(?1,'agent',2,'user',?2,?3,'reserved')", rusqlite::params![id,text,created])?;
            tx.execute("INSERT INTO runtime_event_meta(id,record) VALUES(?1,'{}')", [id])?;
        }
        Ok::<_, StoreError>(())
    }).await.expect("seed current fixture rows");
    store
}

async fn freeze(store: &RuntimeStore, case: Value, rollback: bool) -> Result<(), StoreError> {
    store
        .transaction(move |tx| {
            freeze_start(
                tx,
                "agent",
                case["attempt"]["id"].as_str().expect("attempt"),
                serde_json::from_value(case["transport"].clone())?,
                &case["params"],
                case["text"].as_str().expect("text"),
            )?;
            if rollback {
                return Err(StoreError::RecordContract("before commit failpoint"));
            }
            Ok(())
        })
        .await
}

fn row(db: &rusqlite::Connection, table: &str, id: &str) -> Value {
    let text: String = db
        .query_row(
            &format!("SELECT record FROM {table} WHERE id=?1"),
            [id],
            |row| row.get(0),
        )
        .expect("record");
    let mut record = serde_json::from_str(&text).expect("json");
    studio_runtime_store::agent_volatile::merge_if_agent(db, table, id, &mut record)
        .expect("volatile agent fields");
    record
}
#[tokio::test]
async fn fresh_freeze_matches_python_capture_and_legacy_retry_stays_held() {
    let root = tempfile::tempdir().expect("scratch");
    for (index, case) in fixtures().into_iter().enumerate() {
        let path = root.path().join(index.to_string());
        let store = seeded(&path, &case).await;
        let before = row(
            &store.test_reader().expect("reader"),
            "runtime_agents",
            "agent",
        );
        let result = freeze(&store, case.clone(), false).await;
        if case["retry"] == true {
            assert!(result.is_err());
            assert_eq!(
                row(
                    &store.test_reader().expect("reader"),
                    "runtime_agents",
                    "agent"
                ),
                before
            );
            continue;
        }
        result.expect("freeze");
        let db = store.test_reader().expect("reader");
        assert_eq!(
            row(&db, "runtime_execution_attempts", "attempt")["claudeInputRequest"],
            case["frozen"]
        );
        let agent = row(&db, "runtime_agents", "agent");
        assert_eq!(agent["startAttempt"]["claudeInputRequest"], case["frozen"]);
        assert!(agent.get("claudePreInputRetry").is_none());
        drop(db);
        drop(store);
        let store = RuntimeStore::open(Some(&path)).await.expect("restart");
        assert_eq!(
            row(
                &store.test_reader().expect("reader"),
                "runtime_execution_attempts",
                "attempt"
            )["claudeInputRequest"],
            case["frozen"]
        );
    }
}

#[tokio::test]
async fn freeze_failpoints_roll_back_and_restart_without_losing_the_original_input() {
    let root = tempfile::tempdir().expect("scratch");
    for (index, case) in fixtures()
        .into_iter()
        .filter(|case| case["retry"] != true)
        .enumerate()
    {
        for boundary in ["attempt", "agent", "before_commit", "after_commit"] {
            let path = root.path().join(format!("{index}-{boundary}"));
            let store = seeded(&path, &case).await;
            let db = store.test_reader().expect("reader");
            let before_agent = row(&db, "runtime_agents", "agent");
            let before_attempt = row(&db, "runtime_execution_attempts", "attempt");
            drop(db);
            let table = match boundary {
                "attempt" => Some("runtime_execution_attempts"),
                "agent" => Some("runtime_agents"),
                _ => None,
            };
            if let Some(table) = table {
                store.transaction(move |tx| { tx.execute_batch(&format!("CREATE TRIGGER cut AFTER UPDATE ON {table} BEGIN SELECT RAISE(ABORT,'cut'); END;"))?; Ok(()) }).await.expect("fault");
            }
            let result = freeze(&store, case.clone(), boundary == "before_commit").await;
            assert_eq!(result.is_err(), boundary != "after_commit", "{boundary}");
            if table.is_some() {
                store
                    .transaction(|tx| {
                        tx.execute_batch("DROP TRIGGER cut")?;
                        Ok(())
                    })
                    .await
                    .expect("remove fault");
            }
            drop(store);
            let store = RuntimeStore::open(Some(&path)).await.expect("restart");
            if boundary != "after_commit" {
                let db = store.test_reader().expect("reader");
                assert_eq!(row(&db, "runtime_agents", "agent"), before_agent);
                assert_eq!(
                    row(&db, "runtime_execution_attempts", "attempt"),
                    before_attempt
                );
                drop(db);
                freeze(&store, case.clone(), false)
                    .await
                    .expect("recover before submission");
            }
            assert_eq!(
                row(
                    &store.test_reader().expect("reader"),
                    "runtime_execution_attempts",
                    "attempt"
                )["claudeInputRequest"],
                case["frozen"]
            );
        }
    }
}

#[tokio::test]
async fn changed_configuration_submitted_attempt_and_unbound_events_are_never_frozen() {
    let root = tempfile::tempdir().expect("scratch");
    for (index, case) in fixtures().into_iter().enumerate() {
        for fault in [
            "submitted",
            "event_epoch",
            "configuration",
            "account",
            "closed",
            "missing_marker",
        ] {
            if (fault == "configuration" || fault == "missing_marker") && case["retry"] != true {
                continue;
            }
            let path = root.path().join(format!("{index}-{fault}"));
            let store = seeded(&path, &case).await;
            let mut changed = case.clone();
            match fault {
                "submitted" => {
                    store.transaction(|tx| {tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.submitted',json('true'))",[])?; Ok(())}).await.expect("submitted");
                }
                "event_epoch" => {
                    store
                        .transaction(|tx| {
                            tx.execute("UPDATE runtime_events SET epoch=3", [])?;
                            Ok(())
                        })
                        .await
                        .expect("event");
                }
                "configuration" => changed["params"]["model"] = json!("changed"),
                "account" => changed["transport"]["accountKey"] = json!("foreign"),
                "closed" => changed["transport"]["closed"] = json!(true),
                "missing_marker" => {
                    store.transaction(|tx| {tx.execute("UPDATE runtime_agents SET record=json_remove(record,'$.claudePreInputRetry')",[])?; Ok(())}).await.expect("marker");
                }
                _ => unreachable!(),
            }
            let before = row(
                &store.test_reader().expect("reader"),
                "runtime_agents",
                "agent",
            );
            assert!(freeze(&store, changed, false).await.is_err(), "{fault}");
            assert_eq!(
                row(
                    &store.test_reader().expect("reader"),
                    "runtime_agents",
                    "agent"
                ),
                before
            );
        }
    }
}
