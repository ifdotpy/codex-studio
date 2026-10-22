use serde_json::{Value, json};
use studio_runtime_services::{
    account_worker::commit_prepared_turn,
    claude_input_recovery::{
        TransportSnapshot,
        runtime::{SubmittedBinding, bind_submitted, freeze_start, submission::commit_start},
    },
    notification_dispatch::{PreparedSubmission, SubmittedInput},
    scheduler::TurnIntent,
};
use studio_runtime_store::sha256_hex;

/// Open a Python-created state as Rust does: with the volatile agent storage
/// of 2026-10-20 installed.
fn open_state(path: impl AsRef<std::path::Path>) -> rusqlite::Connection {
    let mut db = rusqlite::Connection::open(path).expect("state opens");
    let tx = db.transaction().expect("transaction");
    studio_runtime_store::agent_volatile::prepare(&tx).expect("volatile agent storage");
    tx.commit().expect("commit");
    db
}

fn corpus(root: &std::path::Path) -> Vec<Value> {
    crate::turn_fixtures::database_cases(include_bytes!("wave7_p08_submission.json"), root)
}
fn state(db: &rusqlite::Connection) -> Value {
    let mut result = serde_json::Map::new();
    for table in [
        "runtime_agents",
        "runtime_execution_runs",
        "runtime_execution_attempts",
        "runtime_execution_inputs",
        "runtime_events",
        "runtime_event_meta",
        "runtime_items",
        "runtime_item_fulltext",
        "runtime_operation_receipts",
    ] {
        let mut stmt = db
            .prepare(&format!("SELECT * FROM {table} ORDER BY 1,2"))
            .expect("table");
        let names = stmt
            .column_names()
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        let rows = stmt
            .query_map([], |row| {
                let mut value = serde_json::Map::new();
                for (i, name) in names.iter().enumerate() {
                    use rusqlite::types::ValueRef;
                    let v = match row.get_ref(i)? {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(v) => json!(v),
                        ValueRef::Real(v) => json!(v),
                        ValueRef::Text(v) => {
                            let s = std::str::from_utf8(v).expect("UTF8");
                            if name == "record" {
                                serde_json::from_str(s).expect("JSON")
                            } else {
                                json!(s)
                            }
                        }
                        ValueRef::Blob(_) => panic!("unexpected blob"),
                    };
                    value.insert(name.clone(), v);
                }
                Ok(Value::Object(value))
            })
            .expect("rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows");
        let mut rows = rows;
        if table == "runtime_agents" {
            studio_runtime_store::agent_volatile::merge_dumped_rows(db, &mut rows)
                .expect("volatile agent fields");
        }
        result.insert(table.into(), json!(rows));
    }
    Value::Object(result)
}
fn agent(state: &Value) -> &Value {
    &state["runtime_agents"][0]["record"]
}
fn transport() -> TransportSnapshot {
    serde_json::from_value(json!({"accountKey":"account","connectionId":"connection","supervisorIdentity":null,"closed":false})).expect("transport")
}
fn binding(generation: u64) -> SubmittedBinding {
    SubmittedBinding {
        transport: transport(),
        generation,
    }
}
fn intent(state: &Value) -> TurnIntent {
    let a = agent(state);
    TurnIntent {
        agent_id: "agent".into(),
        epoch: 2,
        attempt_id: a["startAttempt"]["id"].as_str().expect("id").into(),
        account_key: "account".into(),
        provider: studio_runtime_providers::ProviderKind::Claude,
        thread_id: Some("thread".into()),
        cwd: Some("/fixture".into()),
        model: Some(a["model"].as_str().expect("model").into()),
        radio_authorized: false,
        events: vec![],
    }
}
fn submission(state: &Value, at: f64) -> PreparedSubmission {
    PreparedSubmission {
        first_event_id: "one".into(),
        submitted_at: at,
        created_at: at,
        text: "compiled original".into(),
        assets: json!([]),
        inputs: state["runtime_events"]
            .as_array()
            .expect("events")
            .iter()
            .map(|e| SubmittedInput {
                id: e["id"].as_str().expect("id").into(),
                kind: e["kind"].as_str().expect("kind").into(),
                text: e["text"].as_str().expect("text").into(),
                at: e["created"].as_f64().expect("time"),
                assets: json!([]),
            })
            .collect(),
        radio_authorized: false,
    }
}
fn params(state: &Value) -> Value {
    json!({"threadId":"thread","model":agent(state)["model"],"cwd":"/fixture","serviceTier":"default",
        "clientUserMessageId":"one","input":[{"type":"text","text":"compiled original"}],"dynamicTools":[{"name":"refreshed"}]})
}
// Python does not persist Rust's request/receipt audit envelopes. Check those
// exact additional fields and rows, while comparing every Python column/record.
// The transport connection is nevertheless part of Python's persisted
// startAttempt identity (codex_runtime.py Runtime.start, lines 7455-7458).
fn add_receipts(expected: &mut Value, attempts: &[&str], transcript: bool) {
    for row in expected["runtime_execution_attempts"]
        .as_array_mut()
        .expect("attempts")
    {
        let id = row["id"].as_str().expect("id");
        if attempts.contains(&id) {
            row["record"]["receipts"] = json!({"submission":{"input":{"agent":"agent","epoch":2,
                "ticket":format!("request:{id}"),"account":"account","connection":"connection",
                "generation":7,"thread":"thread"},"result":true}});
        }
    }
    if transcript {
        let item = &expected["runtime_items"][0];
        let signature = json!({"agent":"agent","firstEventId":"one","submittedAt":150.0,
            "createdAt":150.0,"record":item["record"],"text":"compiled original","inputTexts":["original one","original two"]});
        expected["runtime_operation_receipts"] = json!([{"id":format!("provider-submission:{}",sha256_hex(b"agent:one")),
            "signature":sha256_hex(&serde_json::to_vec(&signature).expect("JSON")),"result":"{\"status\":\"written\"}"}]);
    }
}
fn compare(db: &rusqlite::Connection, expected: &Value, label: &str) {
    let actual = state(db);
    for (table, rows) in expected.as_object().expect("state") {
        assert_eq!(&actual[table], rows, "{label}: {table}");
    }
}
#[test]
fn real_submission_transaction_matches_python_complete_rows_and_crash_boundaries() {
    let root = tempfile::tempdir().expect("tmpfs");
    for case in corpus(root.path()) {
        let path = case["path"].as_str().expect("path");
        let mut db = open_state(path);
        compare(&db, &case["before"], "Python prestate");
        let retry = case["retry"] == true;
        let before = case["before"].clone();
        if retry {
            // Python's exact-retry attempts have no Rust retry path: the
            // legacy record stays held and no submission row is written.
            let i = intent(&before);
            let tx = db.transaction().expect("tx");
            assert!(
                commit_start(
                    &tx,
                    &i,
                    &format!("request:{}", i.attempt_id),
                    binding(7),
                    &params(&before),
                    &submission(&before, 170.0),
                )
                .is_err(),
                "legacy retry must stay held"
            );
            drop(tx);
            compare(&db, &before, "legacy retry held without writes");
            continue;
        }
        let i = intent(&before);
        if case["fault"] == "bind" {
            db.execute_batch("CREATE TRIGGER cut BEFORE UPDATE ON runtime_agents WHEN json_extract(NEW.record,'$.startAttempt.connectionId')='connection' AND json_extract(NEW.record,'$.startAttempt.submitted')=1 BEGIN SELECT RAISE(ABORT,'bind cut'); END").expect("fault");
        }
        let tx = db.transaction().expect("tx");
        let result = commit_start(
            &tx,
            &i,
            &format!("request:{}", i.attempt_id),
            binding(7),
            &params(&before),
            &submission(&before, 150.0),
        );
        let failed = matches!(case["fault"].as_str(), Some("configuration" | "bind"));
        assert_eq!(
            result.is_err(),
            failed,
            "case {:?}: {result:?}",
            case["fault"]
        );
        let committed = !failed && case["fault"] != "before_commit";
        if committed {
            tx.commit().expect("commit");
        } else {
            drop(tx);
        }
        if case["fault"] == "bind" {
            db.execute_batch("DROP TRIGGER cut").expect("clear fault");
        }
        drop(db);
        let db = open_state(path);
        let mut expected = case["after"].clone();
        let submitted_attempt = i.attempt_id.as_str();
        add_receipts(
            &mut expected,
            if committed {
                std::slice::from_ref(&submitted_attempt)
            } else {
                &[]
            },
            committed,
        );
        compare(&db, &expected, &format!("fault={}", case["fault"]));
    }
}
#[test]
fn initial_binding_refuses_wrong_or_missing_prepared_generation_and_foreign_transport() {
    let root = tempfile::tempdir().expect("tmpfs");
    let cases = corpus(root.path());
    let fresh = &cases[0];
    let mut db = open_state(fresh["path"].as_str().expect("path"));
    let before = state(&db);
    for fault in [
        "generation",
        "saved_generation",
        "missing_generation",
        "foreign_connection",
        "foreign_supervisor",
    ] {
        let tx = db.transaction().expect("tx");
        freeze_start(
            &tx,
            "agent",
            "attempt",
            transport(),
            &params(&before),
            "compiled original",
        )
        .expect("freeze");
        assert!(
            commit_prepared_turn(
                &tx,
                &intent(&before),
                "request:attempt",
                7,
                "test-connection",
                &submission(&before, 150.0)
            )
            .expect("Submitted")
        );
        match fault {
            "saved_generation" => {
                tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.connectionGeneration',8)",[]).expect("fault");
            }
            "missing_generation" => {
                tx.execute("UPDATE runtime_agents SET record=json_remove(record,'$.startAttempt.connectionGeneration')",[]).expect("fault");
            }
            "foreign_connection" => {
                tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.connectionId','foreign')",[]).expect("fault");
                tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.connectionId','foreign')",[]).expect("fault");
            }
            "foreign_supervisor" => {
                tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.supervisorIdentity',json('{\"handle\":\"foreign\"}'))",[]).expect("fault");
            }
            _ => {}
        }
        if fault == "foreign_supervisor" {
            tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.supervisorIdentity',json('{\"handle\":\"foreign\"}'))",[]).expect("fault");
        }
        let unbound = state(&tx);
        assert!(
            bind_submitted(
                &tx,
                "agent",
                "attempt",
                binding(if fault == "generation" { 8 } else { 7 })
            )
            .is_err(),
            "{fault}"
        );
        compare(&tx, &unbound, "binding refusal has no writes");
        drop(tx);
        compare(&db, &before, "outer rollback restores complete prestate");
    }
}

#[test]
fn submission_write_failpoints_restore_every_row_before_a_safe_retry() {
    let root = tempfile::tempdir().expect("tmpfs");
    let cases = corpus(root.path());
    let fresh = &cases[0];
    let stages = [
        (
            "freeze_attempt",
            "UPDATE",
            "runtime_execution_attempts",
            "1",
        ),
        (
            "freeze_agent",
            "UPDATE",
            "runtime_agents",
            "json_extract(NEW.record,'$.startAttempt.submitted')=0",
        ),
        (
            "submitted",
            "UPDATE",
            "runtime_agents",
            "json_extract(NEW.record,'$.startAttempt.submitted')=1",
        ),
        ("event", "UPDATE", "runtime_events", "1"),
        ("transcript", "INSERT", "runtime_items", "1"),
        ("metadata", "UPDATE", "runtime_event_meta", "1"),
        ("receipt", "INSERT", "runtime_operation_receipts", "1"),
        (
            "bind_attempt",
            "UPDATE",
            "runtime_execution_attempts",
            "json_extract(NEW.record,'$.submitted')=1 AND json_extract(NEW.record,'$.connectionId')='connection'",
        ),
        (
            "bind_agent",
            "UPDATE",
            "runtime_agents",
            "json_extract(NEW.record,'$.startAttempt.submitted')=1 AND json_extract(NEW.record,'$.startAttempt.connectionId')='connection'",
        ),
    ];
    for (stage, action, table, condition) in stages {
        for when in ["BEFORE", "AFTER"] {
            let path = root.path().join(format!("{stage}-{when}.sqlite3"));
            std::fs::copy(fresh["path"].as_str().expect("path"), &path).expect("copy");
            let mut db = open_state(&path);
            let before = state(&db);
            db.execute_batch(&format!("CREATE TRIGGER cut {when} {action} ON {table} WHEN {condition} BEGIN SELECT RAISE(ABORT,'cut'); END")).expect("fault");
            let tx = db.transaction().expect("tx");
            assert!(
                commit_start(
                    &tx,
                    &intent(&before),
                    "request:attempt",
                    binding(7),
                    &params(&before),
                    &submission(&before, 150.0)
                )
                .is_err(),
                "{stage} {when}"
            );
            drop(tx);
            db.execute_batch("DROP TRIGGER cut").expect("clear fault");
            drop(db);
            let mut db = open_state(&path);
            compare(
                &db,
                &before,
                &format!("{stage} {when} rolled back every row"),
            );
            let tx = db.transaction().expect("tx");
            commit_start(
                &tx,
                &intent(&before),
                "request:attempt",
                binding(7),
                &params(&before),
                &submission(&before, 150.0),
            )
            .expect("safe retry before native submission");
            tx.commit().expect("commit");
            let mut expected = fresh["after"].clone();
            add_receipts(&mut expected, &["attempt"], true);
            compare(&db, &expected, &format!("{stage} {when} recovered"));
        }
    }
    let mut db = open_state(fresh["path"].as_str().expect("path"));
    let before = state(&db);
    let mut stale = intent(&before);
    stale.epoch += 1;
    let tx = db.transaction().expect("tx");
    assert!(
        commit_start(
            &tx,
            &stale,
            "request:attempt",
            binding(7),
            &params(&before),
            &submission(&before, 150.0)
        )
        .is_err()
    );
    drop(tx);
    compare(&db, &before, "ownership loss after freeze rolls back");
}
