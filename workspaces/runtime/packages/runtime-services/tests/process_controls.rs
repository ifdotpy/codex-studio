#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use studio_runtime_services::{monitor_matches, process_controls};
use studio_runtime_store::{RuntimeStore, StoreError};

fn corpus() -> Value {
    serde_json::from_str(include_str!("fixtures/process-controls.json")).unwrap()
}
fn parse_request_fixture(
    value: &Value,
) -> studio_runtime_schema::process_controls::ProcessControlRequest {
    serde_json::from_value(value.clone()).unwrap()
}
#[tokio::test]
async fn leaf_listing_does_not_decode_unrelated_agents() {
    let root = tempfile::tempdir().unwrap();
    let store = RuntimeStore::open(Some(root.path())).await.unwrap();
    store.transaction_with(|tx| {
        tx.execute("INSERT INTO runtime_agents(id,record) VALUES('leaf','{\"id\":\"leaf\",\"provider\":\"codex\"}')", [])?;
        for index in 0..100 {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                rusqlite::params![format!("other-{index}"), format!("{{\"id\":\"other-{index}\",\"provider\":\"codex\",\"unused\":1e999}}")])?;
            tx.execute("INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)",
                rusqlite::params![format!("task-{index}"), format!("{{\"id\":\"task-{index}\",\"agent\":\"other-{index}\",\"status\":\"running\",\"unused\":1e999}}")])?;
        }
        Ok::<_, StoreError>(())
    }).await.unwrap();
    store
        .transaction_with(|tx| {
            let owners = process_controls::owners(tx, "leaf")?;
            let rows = process_controls::process_rows(tx, &owners)?;
            let result = process_controls::listing(
                tx,
                &owners,
                &rows,
                &process_controls::Connections::default(),
            )?;
            assert_eq!(result["processes"], json!([]));
            Ok::<_, process_controls::Error>(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn owners_follow_live_descendants_once_even_in_a_cycle() {
    let root = tempfile::tempdir().unwrap();
    let store = RuntimeStore::open(Some(root.path())).await.unwrap();
    store.transaction_with(|tx| {
        for (id, parent, deleted) in [("root", "child", false), ("child", "root", false), ("dead", "root", true), ("hidden", "dead", false)] {
            let record = json!({"id":id,"provider":"codex","parentId":parent,"deletedAt":if deleted { json!(1) } else { Value::Null }});
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)", rusqlite::params![id, record.to_string()]).map_err(StoreError::from)?;
        }
        let owners = process_controls::owners(tx, "root")?;
        assert_eq!(owners.iter().map(|owner| owner["id"].as_str().unwrap()).collect::<Vec<_>>(), vec!["root", "child"]);
        Ok::<_, process_controls::Error>(())
    }).await.unwrap();
}
#[test]
fn python_process_identities_and_capabilities_match() {
    for case in corpus()["processes"].as_array().unwrap() {
        if case["source"] == "monitor" { continue; }
        let mut actual = process_controls::project(
            case["source"].as_str().unwrap(),
            &case["raw"],
            &case["owner"],
            case["connection"].as_str(),
        );
        // These additive renderer fields have no legacy Python projection.
        for field in ["outputWakeBudget", "outputPatterns", "outputFile"] {
            actual.as_object_mut().unwrap().remove(field);
        }
        assert_eq!(actual, case["expected"], "{}", case["source"]);
    }
}
#[test]
fn python_output_state_survives_chunk_boundaries_and_restarts() {
    let corpus = corpus();
    let fixture = &corpus["output"];
    let mut initial = json!({"agent":"owner","status":"running","requestId":"request","created":0,"activityAt":0,"activityGeneration":0,"bytes":0,"command":"","cwd":std::env::temp_dir(),"interactive":false,"livenessCommand":"","log":std::env::temp_dir().join("watch.log"),"stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"tail":"","timeout_ms":3600000,"wakeOn":"exit"});
    initial.as_object_mut().unwrap().extend(fixture["initial"].as_object().unwrap().clone());
    let mut record: studio_runtime_store::MonitorRecord = serde_json::from_value(initial).unwrap();
    for step in fixture["steps"].as_array().unwrap() {
        let bytes: Vec<u8> = step["bytes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_u64().unwrap() as u8)
            .collect();
        monitor_matches::append(&mut record, step["stream"].as_str().unwrap(), &bytes, true)
            .unwrap();
        let event = monitor_matches::pending(&mut record, step["now"].as_f64().unwrap(), false)
            .map(|(id, payload)| json!({"id":id,"payload":payload}));
        assert_eq!(legacy_match_fields(json!(event)), step["event"]);
        let actual = legacy_match_fields(json!(&record));
        for (key, expected) in step["record"].as_object().unwrap() {
            assert_eq!(&actual[key], expected, "field {key}");
        }
        // Exercise the persisted wire representation between chunks.
        let mut persisted = Vec::new();
        serde_json::to_writer(&mut persisted, &record).unwrap();
        record = serde_json::from_slice(&persisted).unwrap();
    }
    monitor_matches::flush(&mut record).unwrap();
    let event = monitor_matches::pending(&mut record, 11.4, true)
        .map(|(id, payload)| json!({"id":id,"payload":payload}));
    assert_eq!(legacy_match_fields(json!(event)), fixture["final"]);
    assert_eq!(json!(monitor_matches::summary(&record)), fixture["summary"]);
}
// The Python corpus owns the pre-existing matcher fields. Rust's additive wake
// budget and excerpt fields have separate receipt/restart witnesses.
fn legacy_match_fields(mut value: Value) -> Value {
    match &mut value {
        Value::Object(object) => {
            for key in [
                "outputWakeBudget",
                "outputLineTail",
                "outputLineOffsets",
                "outputMatchExcerpts",
                "excerpt",
                "logOffset",
            ] {
                object.remove(key);
            }
            for child in object.values_mut() {
                *child = legacy_match_fields(child.take());
            }
        }
        Value::Array(values) => {
            for child in values {
                *child = legacy_match_fields(child.take());
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => (),
    }
    value
}

#[test]
fn invalid_matchers_are_rejected_before_admission() {
    for value in [vec![], vec![String::new()], vec!["x".into(), "x".into()], vec!["x".repeat(257)]] {
        assert!(monitor_matches::patterns(Some(&value)).is_err(), "{value:?}");
    }
    assert!(serde_json::from_value::<Vec<String>>(json!([true])).is_err());
}
#[tokio::test]
async fn action_receipt_replays_without_dispatch_and_rejects_identity_reuse() {
    let root = tempfile::tempdir().unwrap();
    let store = RuntimeStore::open(Some(root.path())).await.unwrap();
    let fixture = corpus()["processes"][0].clone();
    let mut owner = fixture["owner"].clone();
    owner["status"] = json!("idle");
    let mut raw = json!({"id":"watch","agent":"owner","epoch":3,"status":"running","requestId":"request","created":1,"activityAt":0,"activityGeneration":0,"bytes":0,"command":"","cwd":"","interactive":false,"livenessCommand":"","log":"","stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"tail":"","timeout_ms":3600000,"wakeOn":"exit"});
    raw.as_object_mut().unwrap().extend(fixture["raw"].as_object().unwrap().clone());
    raw["operation"]["agent"] = json!("owner");
    raw["operation"]["epoch"] = json!(3);
    store
        .transaction_with(move |tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES('owner',?)",
                [owner.to_string()],
            )?;
            tx.execute(
                "INSERT INTO runtime_monitors(id,record) VALUES('watch',?)",
                [raw.to_string()],
            )?;
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
    let process = fixture["expected"].clone();
    let request = json!({"action":"stop_selected","requestId":"request","agent":"owner","accountKey":"acct","epoch":3,"process":{"key":process["key"],"generation":process["generation"],"agent":"owner","accountKey":"acct","epoch":3}});
    let connections =
        process_controls::Connections::from([("owner".into(), "connection-7".into())]);
    let r = request.clone();
    let c = connections.clone();
    let result = store
        .transaction_with(move |tx| {
            process_controls::claim(tx, &parse_request_fixture(&r), &c, 1.0)
        })
        .await
        .unwrap();
    assert!(matches!(result,process_controls::Claim::New{ref targets,..} if targets.len()==1));
    let r = request.clone();
    let c = connections.clone();
    let replay = store
        .transaction_with(move |tx| {
            process_controls::claim(tx, &parse_request_fixture(&r), &c, 2.0)
        })
        .await
        .unwrap();
    assert!(
        matches!(replay,process_controls::Claim::Replay(ref result) if result.status == studio_runtime_schema::process_controls::ProcessActionStatus::Uncertain)
    );
    let mut r = request.clone();
    r["epoch"] = json!(4);
    assert!(
        store
            .transaction_with(move |tx| process_controls::claim(
                tx,
                &parse_request_fixture(&r),
                &connections,
                3.0
            ))
            .await
            .is_err()
    );
    let connection = store.test_reader().unwrap();
    let query = json!({"requestId":"request","agent":"owner","accountKey":"acct","epoch":3});
    assert!(matches!(
        process_controls::receipt(&connection, &query).unwrap(),
        studio_runtime_schema::process_controls::ProcessReceipt::Found(_)
    ));
    let mut decimal_epoch = query.clone();
    decimal_epoch["epoch"] = json!(3.0);
    assert!(matches!(
        process_controls::receipt(&connection, &decimal_epoch).unwrap(),
        studio_runtime_schema::process_controls::ProcessReceipt::Found(_)
    ));
    let mut foreign = query;
    foreign["accountKey"] = json!("foreign");
    assert!(!matches!(
        process_controls::receipt(&connection, &foreign).unwrap(),
        studio_runtime_schema::process_controls::ProcessReceipt::Found(_)
    ));
}
