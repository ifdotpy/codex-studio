#[path = "support/prepared_python.rs"]
mod python_test_support;
use serde_json::{Value, json};
use studio_runtime_services::claude_input_recovery::runtime::{freeze_start, start_input};

fn fixtures(root: &std::path::Path) -> Vec<Value> {
    let python = python_test_support::prepared_python(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    );
    let output = std::process::Command::new(python)
        .arg("-B")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/wave7_p08_input_oracle.py"
        ))
        .arg(root)
        .output()
        .expect("oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("corpus")
}
fn row(db: &rusqlite::Connection, table: &str, id: &str) -> Value {
    let text: String = db
        .query_row(
            &format!("SELECT record FROM {table} WHERE id=?1"),
            [id],
            |row| row.get(0),
        )
        .expect("record");
    serde_json::from_str(&text).expect("json")
}
#[test]
fn fresh_and_retry_freeze_match_python_capture_and_preserve_refreshed_tools() {
    let root = tempfile::tempdir().expect("scratch");
    for case in fixtures(root.path()) {
        let mut db =
            rusqlite::Connection::open(case["path"].as_str().expect("path")).expect("open");
        let attempt = case["attempt"]["id"].as_str().expect("attempt");
        let tx = db.transaction().expect("transaction");
        let mut current = case["params"].clone();
        if case["retry"] == true {
            current["input"] =
                json!([{"type":"text","text":"newly compiled, must not replace frozen"}]);
        }
        let input = start_input(
            &tx,
            "agent",
            attempt,
            serde_json::from_value(case["transport"].clone()).expect("transport"),
            current,
            case["text"].as_str().expect("text").into(),
        )
        .expect("prepare");
        assert_eq!(input.params, case["params"]);
        freeze_start(
            &tx,
            "agent",
            attempt,
            serde_json::from_value(case["transport"].clone()).expect("transport"),
            &input.params,
            &input.text,
        )
        .expect("freeze");
        assert_eq!(
            row(&tx, "runtime_execution_attempts", attempt)["claudeInputRequest"],
            case["frozen"]
        );
        assert_eq!(
            row(&tx, "runtime_agents", "agent")["startAttempt"]["claudeInputRequest"],
            case["frozen"]
        );
        assert!(
            row(&tx, "runtime_agents", "agent")
                .get("claudePreInputRetry")
                .is_none()
        );
        tx.commit().expect("commit");
        drop(db);
        let db = rusqlite::Connection::open(case["path"].as_str().expect("path")).expect("restart");
        assert_eq!(
            row(&db, "runtime_execution_attempts", attempt)["claudeInputRequest"],
            case["frozen"]
        );
    }
}
#[test]
fn freeze_failpoints_roll_back_and_restart_without_losing_the_original_input() {
    let root = tempfile::tempdir().expect("scratch");
    for case in fixtures(root.path()) {
        for boundary in ["attempt", "agent", "before_commit", "after_commit"] {
            let path = root
                .path()
                .join(format!("cut-{}-{boundary}.sqlite3", case["retry"]));
            std::fs::copy(case["path"].as_str().expect("path"), &path).expect("clone");
            let mut db = rusqlite::Connection::open(&path).expect("open");
            let attempt = case["attempt"]["id"].as_str().expect("attempt");
            let before_agent = row(&db, "runtime_agents", "agent");
            let before_attempt = row(&db, "runtime_execution_attempts", attempt);
            let table = match boundary {
                "attempt" => Some("runtime_execution_attempts"),
                "agent" => Some("runtime_agents"),
                _ => None,
            };
            if let Some(table) = table {
                db.execute_batch(&format!("CREATE TRIGGER cut AFTER UPDATE ON {table} BEGIN SELECT RAISE(ABORT,'cut'); END;")).expect("fault");
            }
            let tx = db.transaction().expect("transaction");
            let result = freeze_start(
                &tx,
                "agent",
                attempt,
                serde_json::from_value(case["transport"].clone()).expect("transport"),
                &case["params"],
                case["text"].as_str().expect("text"),
            );
            assert_eq!(result.is_err(), table.is_some(), "{boundary}");
            if boundary == "after_commit" {
                tx.commit().expect("commit");
            } else {
                drop(tx);
            }
            if table.is_some() {
                db.execute_batch("DROP TRIGGER cut").expect("remove fault");
            }
            drop(db);
            let mut db = rusqlite::Connection::open(&path).expect("restart");
            if boundary != "after_commit" {
                assert_eq!(row(&db, "runtime_agents", "agent"), before_agent);
                assert_eq!(
                    row(&db, "runtime_execution_attempts", attempt),
                    before_attempt
                );
                let tx = db.transaction().expect("transaction");
                freeze_start(
                    &tx,
                    "agent",
                    attempt,
                    serde_json::from_value(case["transport"].clone()).expect("transport"),
                    &case["params"],
                    case["text"].as_str().expect("text"),
                )
                .expect("recover before submission");
                tx.commit().expect("commit");
            }
            assert_eq!(
                row(&db, "runtime_execution_attempts", attempt)["claudeInputRequest"],
                case["frozen"]
            );
        }
    }
}
#[test]
fn changed_configuration_submitted_attempt_and_unbound_events_are_never_frozen() {
    let root = tempfile::tempdir().expect("scratch");
    for case in fixtures(root.path()) {
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
            let path = root
                .path()
                .join(format!("fault-{}-{fault}.sqlite3", case["retry"]));
            std::fs::copy(case["path"].as_str().expect("path"), &path).expect("clone");
            let mut db = rusqlite::Connection::open(path).expect("open");
            let attempt = case["attempt"]["id"].as_str().expect("attempt");
            let mut params = case["params"].clone();
            let mut transport = case["transport"].clone();
            match fault {
                "submitted" => {
                    db.execute("UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.submitted',json('true'))",[]).expect("submitted");
                }
                "event_epoch" => {
                    db.execute("UPDATE runtime_events SET epoch=3", [])
                        .expect("event");
                }
                "configuration" => params["model"] = json!("changed"),
                "account" => transport["accountKey"] = json!("foreign"),
                "closed" => transport["closed"] = json!(true),
                "missing_marker" => {
                    db.execute("UPDATE runtime_agents SET record=json_remove(record,'$.claudePreInputRetry')",[]).expect("marker");
                }
                _ => unreachable!(),
            }
            let before = row(&db, "runtime_agents", "agent");
            let tx = db.transaction().expect("transaction");
            assert!(
                freeze_start(
                    &tx,
                    "agent",
                    attempt,
                    serde_json::from_value(transport).expect("transport"),
                    &params,
                    case["text"].as_str().expect("text")
                )
                .is_err(),
                "{fault}"
            );
            drop(tx);
            assert_eq!(row(&db, "runtime_agents", "agent"), before);
        }
    }
}
