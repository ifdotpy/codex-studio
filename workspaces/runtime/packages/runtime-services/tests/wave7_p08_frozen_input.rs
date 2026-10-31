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
    seed(store, case).await
}

async fn seed(store: RuntimeStore, case: &Value) -> RuntimeStore {
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
        let mut projected = case["frozen"].clone();
        if let Some(source) = projected.get_mut("source").and_then(Value::as_object_mut) {
            for field in ["effort", "fastMode", "nativeEffort", "parentId"] {
                if source.get(field).is_some_and(Value::is_null) {
                    source.remove(field);
                }
            }
        }
        assert_eq!(agent["startAttempt"]["claudeInputRequest"], projected);
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

#[tokio::test]
async fn retry_attempt_owner_preserves_nullable_identity_and_frozen_receipt() {
    use studio_runtime_store::ClaudeRetryRepository;
    let root = tempfile::tempdir().expect("scratch");
    let case = fixtures()
        .into_iter()
        .find(|case| case["retry"] != true)
        .unwrap();
    for fault in [
        "missing",
        "null",
        "empty",
        "missing_event",
        "wrong_id",
        "submitted_null",
        "submitted_missing",
    ] {
        let store = seeded(&root.path().join(fault), &case).await;
        let mut attempt = case["attempt"].clone();
        match fault {
            "missing" => {
                attempt.as_object_mut().unwrap().remove("events");
            }
            "null" => attempt["events"] = Value::Null,
            "empty" => attempt["events"] = json!([]),
            "missing_event" => attempt["events"] = json!(["absent"]),
            "wrong_id" => attempt["id"] = json!("other"),
            "submitted_null" => attempt["submitted"] = Value::Null,
            "submitted_missing" => {
                attempt.as_object_mut().unwrap().remove("submitted");
            }
            _ => unreachable!(),
        }
        let raw = attempt.to_string();
        store
            .transaction(move |tx| {
                tx.execute(
                    "UPDATE runtime_execution_attempts SET record=?1 WHERE id='attempt'",
                    [&raw],
                )?;
                if fault == "empty" {
                    tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.events',json('[]')) WHERE id='agent'", [])?;
                }
                let result = ClaudeRetryRepository::load(tx, "agent", "attempt");
                match fault {
                    "missing" | "null" | "wrong_id" => assert!(result.is_err(), "{fault}"),
                    "missing_event" => assert!(result.unwrap().is_none()),
                    "empty" | "submitted_null" | "submitted_missing" => assert!(result.unwrap().is_some(), "{fault}"),
                    _ => unreachable!(),
                }
                Ok::<_, StoreError>(())
            })
            .await
            .unwrap();
        assert!(
            freeze(&store, case.clone(), false).await.is_err(),
            "{fault}"
        );
    }

    let store = seeded(&root.path().join("duplicates"), &case).await;
    let mut attempt = case["attempt"].clone();
    attempt["future"] = json!({"opaque":[1,null]});
    let serialized = attempt.to_string();
    let raw = format!(
        r#"{{"id":"wrong","events":["bad"],"submitted":true,{}"#,
        &serialized[1..]
    );
    store
        .transaction(move |tx| {
            tx.execute(
                "UPDATE runtime_execution_attempts SET record=?1 WHERE id='attempt'",
                [&raw],
            )?;
            assert!(ClaudeRetryRepository::load(tx, "agent", "attempt").is_err());
            tx.execute(
                "UPDATE runtime_execution_attempts SET record=?1 WHERE id='attempt'",
                [&serialized],
            )?;
            let saved = ClaudeRetryRepository::load(tx, "agent", "attempt")?.unwrap();
            assert_eq!(saved.attempt.id(), "attempt");
            assert_eq!(saved.attempt.events().unwrap(), ["one", "two"]);
            assert_eq!(
                saved.attempt.extension("future"),
                Some(&json!({"opaque":[1,null]}))
            );
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
    freeze(&store, case.clone(), false).await.unwrap();
    let saved = row(
        &store.test_reader().unwrap(),
        "runtime_execution_attempts",
        "attempt",
    );
    assert_eq!(saved["future"], json!({"opaque":[1,null]}));
    assert_eq!(saved["claudeInputRequest"], case["frozen"]);
}

#[tokio::test]
async fn retry_metadata_admission_and_frozen_assets_keep_legacy_evidence() {
    use studio_runtime_store::ClaudeRetryRepository;
    let root = tempfile::tempdir().expect("scratch");
    let template = studio_runtime_store::TestStoreTemplate::new()
        .await
        .expect("template");
    let case = fixtures()
        .into_iter()
        .find(|case| case["retry"] != true)
        .unwrap();
    for (name, raw, expected) in [
        ("missing", None, Some(json!({}))),
        ("null", Some("null"), None),
        ("assets-null", Some(r#"{"assets":null}"#), None),
        (
            "valid",
            Some(r#"{"assets":[null,7,{"vendor":1.0}],"delivery":"after_turn","future":{"x":1}}"#),
            Some(
                json!({"assets":[null,7,{"vendor":1.0}],"delivery":"after_turn","future":{"x":1}}),
            ),
        ),
        (
            "duplicate",
            Some(r#"{"assets":null,"assets":["last"],"future":1,"future":2}"#),
            None,
        ),
        ("malformed", Some("{"), None),
    ] {
        let store = seed(
            template
                .open(&root.path().join(name))
                .await
                .expect("fixture"),
            &case,
        )
        .await;
        let raw = raw.map(str::to_owned);
        let expected_for_read = expected.clone();
        store
            .transaction(move |tx| {
                if name == "malformed" {
                    // The production expression indexes reject malformed writes; model an already-corrupt row.
                    tx.execute_batch("DROP INDEX runtime_conversion_event_meta_rootId; DROP INDEX runtime_conversion_event_meta_leadId; DROP INDEX runtime_receipt_transcript_item;")?;
                }
                match raw {
                    Some(raw) => {
                        tx.execute(
                            "UPDATE runtime_event_meta SET record=?1 WHERE id='one'",
                            [raw],
                        )?;
                    }
                    None => {
                        tx.execute("DELETE FROM runtime_event_meta WHERE id='one'", [])?;
                    }
                }
                match (name, ClaudeRetryRepository::load(tx, "agent", "attempt")) {
                    (
                        "null" | "assets-null" | "duplicate" | "malformed",
                        Err(StoreError::InvalidStoredRecord { object, id }),
                    ) => {
                        assert_eq!(object, "claude retry");
                        assert_eq!(id, "one");
                    }
                    (_, Ok(Some(saved))) => {
                        assert_eq!(saved.events[0]["metadata"], expected_for_read.unwrap())
                    }
                    _ => panic!("unexpected admission for {name}"),
                }
                Ok::<_, StoreError>(())
            })
            .await
            .unwrap();
        if let Some(metadata) = expected {
            freeze(&store, case.clone(), false)
                .await
                .expect("freeze valid metadata");
            let attempt = row(
                &store.test_reader().unwrap(),
                "runtime_execution_attempts",
                "attempt",
            );
            assert_eq!(
                attempt["claudeInputRequest"]["events"][0]["assets"],
                metadata.get("assets").cloned().unwrap_or(json!([]))
            );
        }
    }
    for (name, ids, missing_first) in [
        ("missing-first", json!(["absent", "one"]), true),
        ("invalid-first", json!(["one", "absent"]), false),
    ] {
        let store = seed(
            template
                .open(&root.path().join(name))
                .await
                .expect("fixture"),
            &case,
        )
        .await;
        let mut attempt = case["attempt"].clone();
        attempt["events"] = ids;
        store.transaction(move |tx| {
            tx.execute("UPDATE runtime_execution_attempts SET record=?1 WHERE id='attempt'", [attempt.to_string()])?;
            tx.execute("UPDATE runtime_event_meta SET record='null' WHERE id='one'", [])?;
            let result = ClaudeRetryRepository::load(tx, "agent", "attempt");
            if missing_first { assert!(matches!(result, Ok(None))); }
            else { assert!(matches!(result, Err(StoreError::InvalidStoredRecord { id, .. }) if id == "one")); }
            Ok::<_, StoreError>(())
        }).await.unwrap();
    }
}
