#![allow(clippy::unwrap_used)]

use serde_json::{Value, json};
use studio_runtime_services::{monitor_matches, process_controls};
use studio_runtime_store::{RuntimeStore, StoreError};

fn corpus() -> Value {
    serde_json::from_str(include_str!("fixtures/process-controls.json")).unwrap()
}
#[test]
fn python_process_identities_and_capabilities_match() {
    for case in corpus()["processes"].as_array().unwrap() {
        let actual = process_controls::project(
            case["source"].as_str().unwrap(),
            &case["raw"],
            &case["owner"],
            case["connection"].as_str(),
        );
        assert_eq!(actual, case["expected"], "{}", case["source"]);
    }
}
#[test]
fn python_output_state_survives_chunk_boundaries_and_restarts() {
    let corpus = corpus();
    let fixture = &corpus["output"];
    let mut record = fixture["initial"].clone();
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
        assert_eq!(json!(event), step["event"]);
        assert_eq!(record, step["record"]);
        // The next chunk uses only persisted JSON, including incomplete UTF-8.
        record = serde_json::from_str(&record.to_string()).unwrap();
    }
    monitor_matches::flush(&mut record).unwrap();
    let event = monitor_matches::pending(&mut record, 11.4, true)
        .map(|(id, payload)| json!({"id":id,"payload":payload}));
    assert_eq!(json!(event), fixture["final"]);
    assert_eq!(json!(monitor_matches::summary(&record)), fixture["summary"]);
}
#[test]
fn invalid_matchers_are_rejected_before_admission() {
    for value in [
        json!([]),
        json!([""]),
        json!(["x", "x"]),
        json!(["x".repeat(257)]),
        json!([true]),
    ] {
        assert!(monitor_matches::patterns(Some(&value)).is_err(), "{value}");
    }
}
#[tokio::test]
async fn action_receipt_replays_without_dispatch_and_rejects_identity_reuse() {
    let root = tempfile::tempdir().unwrap();
    let store = RuntimeStore::open(Some(root.path())).await.unwrap();
    let fixture = corpus()["processes"][0].clone();
    let owner = fixture["owner"].clone();
    let raw = fixture["raw"].clone();
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
        .transaction_with(move |tx| process_controls::claim(tx, &r, &c, 1.0))
        .await
        .unwrap();
    assert!(matches!(result,process_controls::Claim::New{ref targets,..} if targets.len()==1));
    let r = request.clone();
    let c = connections.clone();
    let replay = store
        .transaction_with(move |tx| process_controls::claim(tx, &r, &c, 2.0))
        .await
        .unwrap();
    assert!(
        matches!(replay,process_controls::Claim::Replay(ref result) if result["status"]=="uncertain")
    );
    let mut r = request.clone();
    r["epoch"] = json!(4);
    assert!(
        store
            .transaction_with(move |tx| process_controls::claim(tx, &r, &connections, 3.0))
            .await
            .is_err()
    );
    let connection = store.read().unwrap();
    let query = json!({"requestId":"request","agent":"owner","accountKey":"acct","epoch":3});
    assert_eq!(
        process_controls::receipt(&connection, &query).unwrap()["found"],
        true
    );
    let mut foreign = query;
    foreign["accountKey"] = json!("foreign");
    assert_eq!(
        process_controls::receipt(&connection, &foreign).unwrap()["found"],
        false
    );
}
