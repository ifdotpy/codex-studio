#[path = "support/prepared_python.rs"]
mod python_test_support;
use serde_json::{Value, json};
use studio_runtime_services::{
    account_worker::{WorkerError, commit_prepared_turn},
    claude_input_recovery::{
        Rejection, TransportSnapshot,
        runtime::{
            SubmittedBinding, bind_submitted, freeze_start, start_input, submission::commit_start,
        },
        store::recover_rejected_start,
    },
    notification_dispatch::{PreparedSubmission, SubmittedInput, record_user_submission},
    scheduler::TurnIntent,
};
use studio_runtime_store::{AgentRecord, AgentRepository, ExecutionHistoryRepository, sha256_hex};

fn corpus(root: &std::path::Path) -> Vec<Value> {
    let output = std::process::Command::new(python_test_support::prepared_python(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    ))
    .arg("-B")
    .arg(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/wave7_p08_submission_oracle.py"
    ))
    .arg(root)
    .output()
    .expect("oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).expect("corpus")["cases"]
        .as_array()
        .expect("cases")
        .clone()
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
        provider: "claude".into(),
        thread_id: Some("thread".into()),
        cwd: Some("/fixture".into()),
        model: Some(a["model"].as_str().expect("model").into()),
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
    }
}
fn params(state: &Value) -> Value {
    json!({"threadId":"thread","model":agent(state)["model"],"cwd":"/fixture","serviceTier":"default",
        "clientUserMessageId":"one","input":[{"type":"text","text":"compiled original"}],"dynamicTools":[{"name":"refreshed"}]})
}
// Python does not persist Rust's request/receipt audit envelopes. Check those
// exact additional fields and rows, while comparing every Python column/record.
fn add_receipts(expected: &mut Value, attempts: &[&str], transcript: bool) {
    for row in expected["runtime_execution_attempts"]
        .as_array_mut()
        .expect("attempts")
    {
        let id = row["id"].as_str().expect("id");
        if attempts.contains(&id) {
            row["record"]["receipts"] = json!({"submission":{"input":{"agent":"agent","epoch":2,
                "ticket":format!("request:{id}"),"account":"account","generation":7,"thread":"thread"},"result":true}});
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
        let mut db = rusqlite::Connection::open(path).expect("db");
        compare(&db, &case["before"], "Python prestate");
        let retry = case["retry"] == true;
        let mut before = case["before"].clone();
        if retry {
            let tx = db.transaction().expect("tx");
            record_user_submission(&tx, "agent", &submission(&before, 150.0))
                .expect("existing transcript receipt");
            tx.commit().expect("commit");
            add_receipts(&mut before, &[], true);
            compare(&db, &before, "retry prestate with verified receipt");
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
            &submission(&before, if retry { 170.0 } else { 150.0 }),
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
        let db = rusqlite::Connection::open(path).expect("restart");
        let mut expected = case["after"].clone();
        let submitted_attempt = i.attempt_id.as_str();
        add_receipts(
            &mut expected,
            if committed {
                std::slice::from_ref(&submitted_attempt)
            } else {
                &[]
            },
            retry || committed,
        );
        compare(
            &db,
            &expected,
            &format!("retry={retry} fault={}", case["fault"]),
        );
    }
}
#[test]
fn first_submission_rejection_and_new_attempt_preserve_the_complete_original_receipt() {
    let root = tempfile::tempdir().expect("tmpfs");
    let cases = corpus(root.path());
    let fresh = cases
        .iter()
        .find(|c| c["retry"] == false && c["fault"].is_null())
        .expect("fresh");
    let retry = cases
        .iter()
        .find(|c| c["retry"] == true && c["fault"].is_null())
        .expect("retry");
    let mut db = rusqlite::Connection::open(fresh["path"].as_str().expect("path")).expect("db");
    let tx = db.transaction().expect("tx");
    let first = submission(&fresh["before"], 150.0);
    commit_start(
        &tx,
        &intent(&fresh["before"]),
        "request:attempt",
        binding(7),
        &params(&fresh["before"]),
        &first,
    )
    .expect("first submission");
    tx.commit().expect("commit");
    let tx = db.transaction().expect("tx");
    let rejection: Rejection = serde_json::from_value(
        json!({"kind":"rpc","code":-32000,"data":{"turnStartOutcome":"not_applied"}}),
    )
    .expect("rejection");
    assert_eq!(
        serde_json::to_value(
            recover_rejected_start(&tx, "agent", "attempt", transport(), false, &rejection)
                .expect("rejection")
        )
        .expect("outcome"),
        json!("retry")
    );
    tx.commit().expect("commit");
    // The exact-retry reservation belongs to the scheduler integrator. Seed only
    // the NEW UNSENT reservation, leaving first Submitted/receipt rows untouched.
    let tx = db.transaction().expect("tx");
    let a = AgentRecord::from_value(agent(&retry["before"]).clone()).expect("agent");
    AgentRepository::put(&tx, &a).expect("reserve agent");
    ExecutionHistoryRepository::record(&tx, &a).expect("reserve attempt");
    tx.execute("UPDATE runtime_events SET status='reserved'", [])
        .expect("reserve input");
    tx.commit().expect("commit");
    let mut expected_before = retry["before"].clone();
    add_receipts(&mut expected_before, &["attempt"], true);
    compare(
        &db,
        &expected_before,
        "after positive rejection and new reservation",
    );
    let later = submission(&expected_before, 170.0);
    let tx = db.transaction().expect("tx");
    // Reproduces the rejected integration: new attempt + current clock conflicts.
    freeze_start(
        &tx,
        "agent",
        "retry",
        transport(),
        &params(&expected_before),
        &later.text,
    )
    .expect("freeze");
    assert!(matches!(commit_prepared_turn(&tx,&intent(&expected_before),"request:retry",7,&later),Err(WorkerError::Notification(studio_runtime_services::notification_dispatch::NotificationDispatchError::ConflictingInput))));
    drop(tx);
    compare(&db, &expected_before, "conflicting integration rolled back");
    let tx = db.transaction().expect("tx");
    let mut rebuilt = params(&expected_before);
    rebuilt["input"] = json!([{"type":"text","text":"new compiled text"}]);
    let input = start_input(
        &tx,
        "agent",
        "retry",
        transport(),
        rebuilt,
        "new compiled text".into(),
    )
    .expect("frozen preparation");
    assert_eq!(input.text, first.text);
    commit_start(
        &tx,
        &intent(&expected_before),
        "request:retry",
        binding(7),
        &input.params,
        &later,
    )
    .expect("retry through real commit_prepared_turn");
    tx.commit().expect("commit");
    let mut expected = retry["after"].clone();
    add_receipts(&mut expected, &["attempt", "retry"], true);
    compare(
        &db,
        &expected,
        "retry committed, original transcript and receipt unchanged",
    );
}
#[test]
fn initial_binding_refuses_wrong_or_missing_prepared_generation_and_foreign_transport() {
    let root = tempfile::tempdir().expect("tmpfs");
    let cases = corpus(root.path());
    let fresh = &cases[0];
    let mut db = rusqlite::Connection::open(fresh["path"].as_str().expect("path")).expect("db");
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
fn retry_reconstructs_full_text_and_original_assets_and_rejects_a_changed_receipt() {
    let root = tempfile::tempdir().expect("tmpfs");
    let cases = corpus(root.path());
    let fresh = &cases[0];
    let retry = cases
        .iter()
        .find(|c| c["retry"] == true && c["fault"].is_null())
        .expect("retry");
    let mut db = rusqlite::Connection::open(fresh["path"].as_str().expect("path")).expect("db");
    let full = "long input ".repeat(2001);
    db.execute("UPDATE runtime_events SET text=?1 WHERE id='one'", [&full])
        .expect("long input");
    db.execute("UPDATE runtime_event_meta SET record=json_set(record,'$.assets',json('[\"asset\"]')) WHERE id='one'",[]).expect("asset");
    let before = state(&db);
    let mut original = submission(&before, 150.0);
    original.text = full.clone();
    original.inputs[0].assets = json!([{"id":"asset","name":"original asset view"}]);
    original.assets = original.inputs[0].assets.clone();
    let mut prepared = params(&before);
    prepared["input"] = json!([{"type":"text","text":full}]);
    let tx = db.transaction().expect("tx");
    commit_start(
        &tx,
        &intent(&before),
        "request:attempt",
        binding(7),
        &prepared,
        &original,
    )
    .expect("first submission");
    tx.commit().expect("commit");
    let first = state(&db);
    assert_eq!(first["runtime_items"][0]["record"]["truncated"], true);
    assert_eq!(
        first["runtime_items"][0]["record"]["inputs"][0]["truncated"],
        true
    );
    assert_eq!(first["runtime_item_fulltext"][0]["body"], full);
    let tx = db.transaction().expect("tx");
    let rejection = serde_json::from_value(
        json!({"kind":"rpc","code":-32000,"data":{"turnStartOutcome":"not_applied"}}),
    )
    .expect("rejection");
    assert!(
        recover_rejected_start(&tx, "agent", "attempt", transport(), false, &rejection)
            .expect("recover")
            .is_some()
    );
    let mut current = agent(&state(&tx)).clone();
    current["startAttempt"] = agent(&retry["before"])["startAttempt"].clone();
    current["status"] = json!("starting");
    current["inFlight"] = json!(true);
    let record = AgentRecord::from_value(current).expect("agent");
    AgentRepository::put(&tx, &record).expect("reserve");
    ExecutionHistoryRepository::record(&tx, &record).expect("record reservation");
    tx.execute("UPDATE runtime_events SET status='reserved'", [])
        .expect("reserve events");
    tx.commit().expect("commit");
    let before_retry = state(&db);
    let mut later = original.clone();
    later.submitted_at = 170.0;
    later.created_at = 171.0;
    later.assets = json!([{"id":"asset","name":"new asset view"}]);
    later.inputs[0].assets = later.assets.clone();
    later.inputs[0].text = "new compiler preview".into();
    for fault in ["clock", "assets", "input_assets"] {
        let tx = db.transaction().expect("tx");
        let path = match fault {
            "clock" => "$.at",
            "assets" => "$.assets",
            _ => "$.inputs[0].assets",
        };
        let value = if fault == "clock" {
            json!(1.0)
        } else {
            json!([{"id":"asset","name":"tampered"}])
        };
        tx.execute(
            "UPDATE runtime_items SET record=json_set(record,?1,json(?2))",
            rusqlite::params![path, value.to_string()],
        )
        .expect("tamper original identity");
        assert!(matches!(commit_start(&tx,&intent(&before_retry),"request:retry",binding(7),&prepared,&later),Err(WorkerError::Notification(studio_runtime_services::notification_dispatch::NotificationDispatchError::ConflictingInput))),"{fault}");
        drop(tx);
        compare(
            &db,
            &before_retry,
            "receipt conflict rolled back all submission writes",
        );
    }
    // A full transcript is proof for the frozen retry; a preview alone is not.
    let tx = db.transaction().expect("tx");
    tx.execute("DELETE FROM runtime_item_fulltext WHERE id='agent:one'", [])
        .expect("missing fulltext");
    let incomplete = state(&tx);
    assert!(
        commit_start(
            &tx,
            &intent(&before_retry),
            "request:retry",
            binding(7),
            &prepared,
            &later
        )
        .is_err()
    );
    compare(
        &tx,
        &incomplete,
        "missing long fulltext refuses before submission writes",
    );
    drop(tx);
    compare(&db, &before_retry, "missing fulltext refusal rolled back");
    let tx = db.transaction().expect("tx");
    commit_start(
        &tx,
        &intent(&before_retry),
        "request:retry",
        binding(7),
        &prepared,
        &later,
    )
    .expect("exact original submission from full input and saved assets");
    tx.commit().expect("commit");
    let after = state(&db);
    for table in [
        "runtime_items",
        "runtime_item_fulltext",
        "runtime_operation_receipts",
    ] {
        assert_eq!(after[table], first[table], "{table}");
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
            "json_extract(NEW.record,'$.startAttempt.submitted')=1 AND json_extract(NEW.record,'$.startAttempt.connectionId') IS NULL",
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
            let mut db = rusqlite::Connection::open(&path).expect("db");
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
            let mut db = rusqlite::Connection::open(&path).expect("restart");
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
    let mut db = rusqlite::Connection::open(fresh["path"].as_str().expect("path")).expect("db");
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

#[test]
fn python_retry_without_a_rust_receipt_migrates_only_a_complete_transcript() {
    let root = tempfile::tempdir().expect("tmpfs");
    let cases = corpus(root.path());
    let retry = cases
        .iter()
        .find(|c| c["retry"] == true && c["fault"].is_null())
        .expect("Python retry");
    for fault in [
        "none",
        "missing_item",
        "missing_inputs",
        "partial_input",
        "missing_assets",
        "missing_clock",
        "missing_text",
        "partial_text",
    ] {
        let path = root.path().join(format!("python-retry-{fault}.sqlite3"));
        std::fs::copy(retry["path"].as_str().expect("path"), &path)
            .expect("Python-written prestate");
        let mut db = rusqlite::Connection::open(&path).expect("db");
        compare(
            &db,
            &retry["before"],
            "untouched Python retry, no Rust receipt seeded",
        );
        assert_eq!(state(&db)["runtime_operation_receipts"], json!([]));
        match fault {
            "missing_item" => {
                db.execute("DELETE FROM runtime_items WHERE id='agent:one'", [])
                    .expect("missing item");
            }
            "partial_text" => {
                db.execute("UPDATE runtime_items SET record=json_set(record,'$.text','partial') WHERE id='agent:one'",[]).expect("partial text");
            }
            "none" => {}
            _ => {
                let field = match fault {
                    "missing_inputs" => "$.inputs",
                    "partial_input" => "$.inputs[0].assets",
                    "missing_assets" => "$.assets",
                    "missing_clock" => "$.at",
                    "missing_text" => "$.text",
                    _ => unreachable!(),
                };
                db.execute(
                    "UPDATE runtime_items SET record=json_remove(record,?1) WHERE id='agent:one'",
                    [field],
                )
                .expect("incomplete item");
            }
        }
        let before = state(&db);
        let tx = db.transaction().expect("tx");
        let result = commit_start(
            &tx,
            &intent(&before),
            "request:retry",
            binding(7),
            &params(&before),
            &submission(&before, 170.0),
        );
        assert_eq!(result.is_ok(), fault == "none", "{fault}: {result:?}");
        if fault == "none" {
            tx.commit().expect("commit Python-origin retry");
        } else {
            drop(tx);
        }
        drop(db);
        let db = rusqlite::Connection::open(&path).expect("restart");
        if fault == "none" {
            let mut expected = retry["after"].clone();
            add_receipts(&mut expected, &["retry"], true);
            compare(
                &db,
                &expected,
                "Python transcript migrated through commit_start without a prior Rust receipt",
            );
        } else {
            compare(
                &db,
                &before,
                "unproven retry refused, all rows unchanged after restart",
            );
        }
    }
}
